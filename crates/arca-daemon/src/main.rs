use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};

use arca_core::db::Db;
use arca_daemon::{
    alerts, calendar as cal_engine, config, log_writer, pledge, providers, reports, rpc, scheduler,
    secrets,
};
use config::Config;
use log_writer::{LogReopener, ReopenWriter};
use rpc::handler::State;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(name = "arca-daemon", version)]
struct Cli {
    /// Path to TOML config file.
    #[arg(long, env = "ARCA_CONF", default_value = "/etc/arca/arca.conf")]
    conf: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Seed a fresh database with fictional finances for screenshots / the VHS
    /// demo. Refuses to overwrite an existing file, so it can never touch a real
    /// DB. Run, then point a daemon at the file with `--conf` to serve it.
    SeedDemo {
        /// Path for the new demo database (must not already exist).
        #[arg(long)]
        db: PathBuf,
    },
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // One-shot subcommands run before any config load / sandbox / listener setup.
    if let Some(Cmd::SeedDemo { db }) = &cli.cmd {
        return run_seed_demo(db);
    }

    let cfg = Config::load(&cli.conf)?;
    let (_log_guard, log_reopener) = init_tracing(&cfg.daemon.log_path);
    tracing::info!(conf = %cli.conf.display(), "arca-daemon starting");

    // Write PID file before pledge/unveil so newsyslog (running as root) can
    // signal us on rotation. We tolerate write failures here — they only mean
    // newsyslog can't trigger log reopen.
    if let Some(parent) = cfg.daemon.pid_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Err(e) = std::fs::write(&cfg.daemon.pid_path, format!("{}\n", std::process::id())) {
        tracing::warn!(path = %cfg.daemon.pid_path.display(), error = %e, "pid file write failed");
    }

    let secrets = std::sync::Arc::new(secrets::Secrets::load(
        cfg.daemon.secrets_age.as_deref(),
        cfg.daemon.secrets_key.as_deref(),
    )?);

    // DB.
    if let Some(parent) = cfg.daemon.db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db = Arc::new(Db::open(&cfg.daemon.db_path)?);

    // Load providers from the DB. The manual row is seeded if missing.
    let loaded = Arc::new(providers::registry::load(&db, &secrets)?);

    // pledge/unveil — apply on OpenBSD, no-op elsewhere.
    apply_sandbox(&cfg)?;

    let state = Arc::new(State {
        db: Arc::clone(&db),
        started_at: Instant::now(),
        version: VERSION,
        providers: Arc::clone(&loaded),
    });

    // Bind listeners eagerly so a bind error aborts before scheduler/signals run.
    // Two Unix sockets: read (bridge) and write (operator-only, UID-gated).
    if let Some(parent) = cfg.daemon.read_socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = cfg.daemon.write_socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if cfg.daemon.operator_uid.is_none() {
        tracing::warn!(
            "daemon.operator_uid unset — write socket fails closed (set `id -u <operator>`)"
        );
    }
    let read_listener = rpc::server::bind_unix(&cfg.daemon.read_socket_path, 0o660)?;
    let write_listener = rpc::server::bind_unix(&cfg.daemon.write_socket_path, 0o660)?;
    let tcp_listener = rpc::server::bind_tcp(&cfg.daemon.tcp_bind).await?;

    let read_state = Arc::clone(&state);
    let read_path = cfg.daemon.read_socket_path.clone();
    let read_task = tokio::spawn(async move {
        if let Err(e) = rpc::server::serve_unix(
            read_state,
            read_listener,
            read_path,
            rpc::server::SocketRole::Read,
            None,
        )
        .await
        {
            tracing::error!(error = %e, "read socket listener");
        }
    });

    let write_state = Arc::clone(&state);
    let write_path = cfg.daemon.write_socket_path.clone();
    let operator_uid = cfg.daemon.operator_uid;
    let write_task = tokio::spawn(async move {
        if let Err(e) = rpc::server::serve_unix(
            write_state,
            write_listener,
            write_path,
            rpc::server::SocketRole::Write,
            operator_uid,
        )
        .await
        {
            tracing::error!(error = %e, "write socket listener");
        }
    });

    let tcp_state = Arc::clone(&state);
    let tcp_bind = cfg.daemon.tcp_bind.clone();
    let tcp_task = tokio::spawn(async move {
        if let Err(e) = rpc::server::serve_tcp(tcp_state, tcp_listener, tcp_bind).await {
            tracing::error!(error = %e, "tcp listener");
        }
    });

    let sched = scheduler::Scheduler::new(loaded, Arc::clone(&db));
    let sched_task = tokio::spawn(sched.run());

    let alert_engine = alerts::AlertEngine::new(Arc::clone(&db), cfg.alerts.clone());
    let alert_task = tokio::spawn(alert_engine.run());

    let reports_engine = reports::ReportsEngine::new(Arc::clone(&db), cfg.reports.clone());
    let reports_task = tokio::spawn(reports_engine.run());

    let calendar_engine = cal_engine::CalendarEngine::new(Arc::clone(&db), cfg.calendar.clone());
    let calendar_task = tokio::spawn(calendar_engine.run());

    // Signals.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut sigusr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => { tracing::info!("SIGTERM, shutting down"); break; }
            _ = sigint.recv()  => { tracing::info!("SIGINT, shutting down");  break; }
            _ = sighup.recv()  => {
                // v1: config + secrets are loaded once at startup. Each provider
                // captures its credential at build time, so re-decrypting secrets
                // without rebuilding the whole registry would be a no-op — and a
                // live registry hot-swap is out of scope for v1. Credential/config
                // rotation is a restart (`rcctl restart arca`), which the deploy
                // flow already performs. Log honestly rather than imply a reload
                // happened. (SIGUSR1 handles log rotation; see the security model.)
                tracing::info!(
                    "SIGHUP received: secret/config reload is restart-only in v1 — \
                     run `rcctl restart arca` to apply new secrets or config"
                );
            }
            _ = sigusr1.recv() => {
                match log_reopener.as_ref().map(LogReopener::reopen) {
                    Some(Ok(())) => tracing::info!("SIGUSR1: log file reopened"),
                    Some(Err(e)) => tracing::error!(error = %e, "SIGUSR1: log reopen failed"),
                    None => tracing::warn!("SIGUSR1 received but no file-backed log writer"),
                }
            }
        }
    }

    read_task.abort();
    write_task.abort();
    tcp_task.abort();
    sched_task.abort();
    alert_task.abort();
    reports_task.abort();
    calendar_task.abort();
    let _ = std::fs::remove_file(&cfg.daemon.pid_path);
    Ok(())
}

/// `seed-demo` subcommand. Creates a fresh database and fills it with fictional
/// finances for screenshots / the VHS recording. Refuses to overwrite an existing
/// file so it can never clobber a real DB.
fn run_seed_demo(db_path: &Path) -> anyhow::Result<()> {
    if db_path.exists() {
        anyhow::bail!(
            "refusing to seed: {} already exists (seed-demo only creates a fresh DB)",
            db_path.display()
        );
    }
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db = Db::open(db_path)?;
    arca_core::demo::seed(&db)?;
    println!("seeded fictional demo data into {}", db_path.display());
    Ok(())
}

/// Initialize tracing. Writes to `log_path` if its parent directory exists
/// (production), otherwise falls back to stderr (tests, missing dir).
///
/// Returns `(Option<WorkerGuard>, Option<LogReopener>)`: the guard keeps the
/// non-blocking writer alive; the reopener lets the SIGUSR1 handler swap the
/// underlying File after newsyslog rotates.
fn init_tracing(
    log_path: &std::path::Path,
) -> (
    Option<tracing_appender::non_blocking::WorkerGuard>,
    Option<LogReopener>,
) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let parent_ok = log_path.parent().is_some_and(std::path::Path::is_dir);
    if !parent_ok {
        fmt().with_env_filter(filter).with_target(false).init();
        return (None, None);
    }

    let Ok((writer, reopener)) = ReopenWriter::open(log_path) else {
        fmt().with_env_filter(filter).with_target(false).init();
        return (None, None);
    };
    let (nb, guard) = tracing_appender::non_blocking(writer);
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(nb)
        .init();
    (Some(guard), Some(reopener))
}

fn apply_sandbox(cfg: &Config) -> anyhow::Result<()> {
    // unveil — narrow before pledge.
    pledge::unveil_path(&path_str(&cfg.daemon.db_path), "rwc")?;
    if let Some(parent) = cfg.daemon.db_path.parent() {
        pledge::unveil_path(&path_str(parent), "rwc")?;
    }
    if let Some(parent) = cfg.daemon.log_path.parent() {
        pledge::unveil_path(&path_str(parent), "rwc")?;
    }
    if let Some(parent) = cfg.daemon.read_socket_path.parent() {
        pledge::unveil_path(&path_str(parent), "rwc")?;
    }
    if let Some(parent) = cfg.daemon.write_socket_path.parent() {
        pledge::unveil_path(&path_str(parent), "rwc")?;
    }
    // reports + .ics digest paths. Default to /var/arca/reports (already
    // covered by the db-parent unveil), but a custom path needs its own entry.
    pledge::unveil_path(&path_str(&cfg.reports.reports_dir), "rwc")?;
    pledge::unveil_path(&path_str(&cfg.calendar.ics_dir), "rwc")?;
    pledge::unveil_path("/etc/arca", "r")?;
    // /tmp: report / .ics scratch space.
    pledge::unveil_path("/tmp", "rwc")?;
    pledge::unveil_finalize()?;
    // flock: SQLite WAL file locking; fattr: chmod on the Unix socket after bind.
    // inet: reqwest HTTP providers + the future arca-xmpp transport. No `proc
    // exec`: the daemon spawns nothing — alerts/reports/.ics are recorded or
    // written to disk and pushed out of band by the arca-xmpp bridge.
    pledge::pledge_promises("stdio rpath wpath cpath flock fattr inet unix dns")?;
    Ok(())
}

fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}
