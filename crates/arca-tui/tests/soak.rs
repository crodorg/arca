//! PTY soak + performance harness (Waves 4 & 5).
//!
//! Runs the *real* `arca` TUI inside a pseudo-terminal, talking to a *real*
//! throwaway `arca-daemon` (temp socket + temp sqlite seeded with the fictional
//! demo data), so it exercises what the unit suite can't reach: crossterm
//! raw-mode + the blocking `event::poll` input path, the render loop over live
//! RPC data, SIGWINCH/resize handling, and clean shutdown.
//!
//! Unlike the upstream harness there is NO terminal-capability probe to answer — arca's
//! TUI is plain ratatui/crossterm and queries nothing at startup. We still force
//! the pty raw from the master side so keystrokes are delivered byte-by-byte
//! rather than line-buffered/echoed (see ~/wiki/concepts/pty-tui-test-harness.md).
//!
//! Two ignored tests (need the release binaries + a few seconds):
//!   cargo build --release
//!   cargo test --release -p arca-tui --test soak -- --ignored --nocapture
//! Tunables (env): ARCA_SOAK_BIN (default target/release/arca),
//!   ARCA_SOAK_DAEMON (default target/release/arca-daemon),
//!   ARCA_SOAK_ITERS (default 300), ARCA_SOAK_STEP_MS (default 25).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Unique-per-instance temp dir suffix without `Date::now`/random (kept simple
/// and deterministic across the two tests in one process).
static SEQ: AtomicUsize = AtomicUsize::new(0);

fn bin(env_key: &str, name: &str) -> PathBuf {
    // `cargo test` runs with cwd = the package dir (crates/arca-tui), but the
    // workspace `target/` is at the repo root, so the default is anchored to the
    // manifest dir rather than cwd. `qa.sh stress` overrides with absolute paths.
    let p = std::env::var_os(env_key).map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/release")
                .join(name)
        },
        PathBuf::from,
    );
    assert!(
        p.exists(),
        "{env_key} binary not found at {} — build it first: cargo build --release",
        p.display()
    );
    // Absolute: portable-pty does PATH resolution otherwise, and our cwd becomes
    // the temp dir, so a relative path would not resolve.
    std::fs::canonicalize(&p).expect("canonicalize binary path")
}

// ---------- the throwaway daemon ----------

/// A real `arca-daemon` serving a freshly-seeded demo DB on temp Unix sockets.
/// Mirrors `demo/record.sh`. Owns the temp dir; killing/cleanup is on Drop.
struct DemoDaemon {
    dir: PathBuf,
    child: std::process::Child,
}

impl DemoDaemon {
    fn start(daemon_bin: &Path) -> DemoDaemon {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("arca-soak-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("reports")).expect("mkdir temp dir");
        std::fs::create_dir_all(dir.join("home")).expect("mkdir tui home");

        let db = dir.join("arca.db");
        let seeded = Command::new(daemon_bin)
            .args(["seed-demo", "--db"])
            .arg(&db)
            .status()
            .expect("spawn seed-demo");
        assert!(seeded.success(), "seed-demo failed: {seeded:?}");

        // Minimal config: no secrets (the demo DB has no credentialed providers),
        // ephemeral loopback TCP (no port clash), no operator_uid (the TUI uses the
        // ungated read socket). The TUI never sends a write verb during the soak.
        // Temp paths are ASCII (env temp_dir + our own names), so plain quoting is
        // valid TOML — no escaping needed.
        let conf = dir.join("arca.conf");
        let body = format!(
            "[daemon]\n\
             db_path           = \"{db}\"\n\
             log_path          = \"{log}\"\n\
             read_socket_path  = \"{read}\"\n\
             write_socket_path = \"{write}\"\n\
             pid_path          = \"{pid}\"\n\
             tcp_bind          = \"127.0.0.1:0\"\n\
             tz_display        = \"America/Puerto_Rico\"\n\
             \n[reports]\nreports_dir = \"{reports}\"\n\
             \n[calendar]\nics_dir = \"{reports}\"\n",
            db = db.display(),
            log = dir.join("arca.log").display(),
            read = dir.join("read.sock").display(),
            write = dir.join("write.sock").display(),
            pid = dir.join("arca.pid").display(),
            reports = dir.join("reports").display(),
        );
        std::fs::write(&conf, body).expect("write conf");

        let out = std::fs::File::create(dir.join("daemon.out")).expect("daemon.out");
        let err = out.try_clone().expect("clone daemon.out");
        let child = Command::new(daemon_bin)
            .arg("--conf")
            .arg(&conf)
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("spawn arca-daemon");

        let d = DemoDaemon { dir, child };

        // Wait for the read socket to appear (daemon binds it eagerly at startup).
        let sock = d.read_sock();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !sock.exists() {
            assert!(
                Instant::now() < deadline,
                "daemon did not create {} within 8s.\ndaemon.out:\n{}",
                sock.display(),
                std::fs::read_to_string(d.dir.join("daemon.out")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        d
    }

    fn read_sock(&self) -> PathBuf {
        self.dir.join("read.sock")
    }

    fn tui_home(&self) -> PathBuf {
        self.dir.join("home")
    }
}

impl Drop for DemoDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------- the pty-driven TUI ----------

/// Put the pty into raw mode from the harness side via the master fd (on Linux
/// `tcsetattr` on the master sets the slave's line discipline). Without this the
/// slave defaults to canonical mode with echo, so keystrokes are line-buffered
/// and echoed instead of delivered byte-by-byte — and a TUI never sees them.
#[cfg(unix)]
fn set_pty_raw(master_fd: std::os::unix::io::RawFd) {
    unsafe {
        let mut t = std::mem::MaybeUninit::<libc::termios>::zeroed().assume_init();
        if libc::tcgetattr(master_fd, &raw mut t) == 0 {
            libc::cfmakeraw(&raw mut t);
            let _ = libc::tcsetattr(master_fd, libc::TCSANOW, &raw const t);
        }
    }
}

/// Resident set size in kB from /proc/<pid>/status (Linux).
fn read_vmrss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Total CPU time (utime+stime, in clock ticks) from /proc/<pid>/stat (Linux).
/// Fields 14/15, indexed after the comm `)` so a comm with spaces/parens can't
/// shift the offsets.
fn read_cpu_jiffies(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let fields: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[cfg(unix)]
fn clk_tck() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

/// A running `arca` TUI child in a pty, with a background reader that drains
/// output, counts bytes, and watches for panics.
struct Tui {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    bytes_seen: Arc<AtomicUsize>,
    panicked: Arc<AtomicBool>,
    tail: Arc<Mutex<Vec<u8>>>,
    pid: Option<u32>,
    _reader: JoinHandle<()>,
}

impl Tui {
    fn spawn(bin: &Path, socket: &Path, home: &Path) -> Tui {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // Raw from the start so input is delivered byte-by-byte, not echoed.
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            set_pty_raw(fd);
        }

        // env_clear() so the child never inherits TMUX/DBUS or other host surprises.
        let mut cmd = CommandBuilder::new(bin);
        cmd.env_clear();
        cmd.cwd(home);
        cmd.env("HOME", home);
        cmd.env("TERM", "xterm-256color");
        cmd.env("LANG", "C.UTF-8");
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.arg("--socket");
        cmd.arg(socket);

        let child = pair.slave.spawn_command(cmd).expect("spawn arca in pty");
        drop(pair.slave); // EOF propagates to the reader when arca exits.
        let pid = child.process_id();

        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
        let mut reader = pair.master.try_clone_reader().expect("pty reader");

        let bytes_seen = Arc::new(AtomicUsize::new(0));
        let panicked = Arc::new(AtomicBool::new(false));
        let tail = Arc::new(Mutex::new(Vec::<u8>::new()));

        let r_bytes = Arc::clone(&bytes_seen);
        let r_panicked = Arc::clone(&panicked);
        let r_tail = Arc::clone(&tail);
        let reader = std::thread::spawn(move || {
            let mut acc: Vec<u8> = Vec::with_capacity(8192);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF / EIO => child gone
                    Ok(n) => {
                        r_bytes.fetch_add(n, Ordering::Relaxed);
                        acc.extend_from_slice(&buf[..n]);
                        if acc.windows(8).any(|w| w == b"panicked") {
                            r_panicked.store(true, Ordering::Relaxed);
                        }
                        if acc.len() > 64 * 1024 {
                            let cut = acc.len() - 8192;
                            acc.drain(..cut);
                        }
                        if let Ok(mut t) = r_tail.lock() {
                            t.extend_from_slice(&buf[..n]);
                            let len = t.len();
                            if len > 4096 {
                                t.drain(..len - 4096);
                            }
                        }
                    }
                }
            }
        });

        Tui {
            child,
            master: pair.master,
            writer,
            bytes_seen,
            panicked,
            tail,
            pid,
            _reader: reader,
        }
    }

    fn send(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn panicked(&self) -> bool {
        self.panicked.load(Ordering::Relaxed)
    }

    fn tail_str(&self) -> String {
        self.tail
            .lock()
            .map(|t| String::from_utf8_lossy(&t).into_owned())
            .unwrap_or_default()
    }

    fn rss_kb(&self) -> Option<u64> {
        self.pid.and_then(read_vmrss_kb)
    }

    #[cfg(unix)]
    fn cpu_jiffies(&self) -> Option<u64> {
        self.pid.and_then(read_cpu_jiffies)
    }

    /// Block until arca has painted a frame's worth of bytes.
    fn wait_first_frame(&mut self, timeout: Duration) -> Result<Duration, String> {
        let start = Instant::now();
        let deadline = start + timeout;
        while self.bytes_seen.load(Ordering::Relaxed) < 2000 {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                return Err(format!(
                    "arca did not render within {timeout:?} ({} bytes) — startup hang or daemon \
                     unreachable.\nlast output:\n{}",
                    self.bytes_seen.load(Ordering::Relaxed),
                    self.tail_str()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(start.elapsed())
    }

    fn exited(&mut self) -> Option<portable_pty::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Send Esc (clear any pending modal/filter state) then 'q', and wait for the
    /// process to tear down. Returns true if it exited in time.
    fn quit(&mut self, timeout: Duration) -> bool {
        self.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(150));
        self.send(b"q");
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.exited().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        false
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "PTY soak: needs the release binaries; run with --ignored"]
fn pty_soak_drives_render_resize_quit_without_panic_or_leak() {
    let daemon = DemoDaemon::start(&bin("ARCA_SOAK_DAEMON", "arca-daemon"));
    let iters = env_usize("ARCA_SOAK_ITERS", 300);
    let step = Duration::from_millis(env_usize("ARCA_SOAK_STEP_MS", 25) as u64);

    let mut h = Tui::spawn(
        &bin("ARCA_SOAK_BIN", "arca"),
        &daemon.read_sock(),
        &daemon.tui_home(),
    );
    if let Err(e) = h.wait_first_frame(Duration::from_secs(15)) {
        panic!("{e}");
    }

    std::thread::sleep(Duration::from_millis(500));
    let rss_baseline = h.rss_kb();
    let mut rss_max = rss_baseline.unwrap_or(0);
    let mut rss_mid = 0u64;
    let mut rss_late = 0u64;

    // View-switch letters + scroll + Tab. Deliberately NO Enter/digits (they open
    // action popups/menus that would capture later keys); resize exercises relayout.
    let keys: &[&[u8]] = &[
        b"m",      // Money
        b"j",      // down
        b"t",      // Tx
        b"j",      // down
        b"\x1b[B", // Down arrow
        b"p",      // Pp
        b"v",      // Charts
        b"a",      // Alerts
        b"e",      // Expenses
        b"\t",     // Tab (zone cycle where applicable)
        b"k",      // up
        b"o",      // Menu
    ];
    let sizes = [(80u16, 24u16), (100, 30), (60, 20), (120, 40), (90, 26)];

    for i in 0..iters {
        assert!(!h.panicked(), "arca panicked during soak (iter {i})");
        if let Some(status) = h.exited() {
            panic!("arca exited early at iter {i} with status {status:?}");
        }
        h.send(keys[i % keys.len()]);
        if i % 7 == 0 {
            let (cols, rows) = sizes[(i / 7) % sizes.len()];
            h.resize(cols, rows);
        }
        if i % 25 == 0 {
            if let Some(kb) = h.rss_kb() {
                rss_max = rss_max.max(kb);
                if i >= iters / 2 {
                    if rss_mid == 0 {
                        rss_mid = kb;
                    }
                    rss_late = kb;
                }
            }
        }
        std::thread::sleep(step);
    }

    let exited = h.quit(Duration::from_secs(8));
    assert!(
        exited,
        "arca did not exit within 8s of 'q' (possible shutdown hang)"
    );
    assert!(!h.panicked(), "arca panicked during soak");

    // Leak guard: the soak's SECOND HALF should be roughly flat. Cold-start warmup
    // legitimately grows RSS to a plateau, so comparing the post-warmup midpoint to
    // the end isolates runaway per-iteration growth.
    eprintln!(
        "soak: iters={iters} rss_baseline={rss_baseline:?}kB rss_mid={rss_mid}kB \
         rss_max={rss_max}kB rss_late={rss_late}kB"
    );
    if rss_mid > 0 && rss_late > 0 {
        let ratio = rss_late as f64 / rss_mid as f64;
        assert!(
            ratio < 1.25,
            "RSS grew {ratio:.2}x in the soak's second half (mid {rss_mid}kB -> late {rss_late}kB) \
             — possible leak"
        );
    }
}

/// Wave 5 performance gate. arca's TUI event loop renders every <=200ms via a
/// blocking `event::poll`, so idle CPU is low but not strictly zero; a regression
/// to a busy-spin would peg a core. This measures CPU over an idle window (no
/// input) and the time-to-first-frame as committed regression guards.
#[cfg(unix)]
#[test]
#[ignore = "perf gate: needs the release binaries; run with --ignored"]
fn idle_cpu_stays_low_and_first_frame_is_fast() {
    let daemon = DemoDaemon::start(&bin("ARCA_SOAK_DAEMON", "arca-daemon"));
    let mut h = Tui::spawn(
        &bin("ARCA_SOAK_BIN", "arca"),
        &daemon.read_sock(),
        &daemon.tui_home(),
    );
    let first_frame = match h.wait_first_frame(Duration::from_secs(15)) {
        Ok(d) => d,
        Err(e) => panic!("{e}"),
    };

    // Settle past warmup, then measure CPU over a quiet window with NO input.
    std::thread::sleep(Duration::from_millis(750));
    let tck = clk_tck();
    let c0 = h.cpu_jiffies();
    let w0 = Instant::now();
    std::thread::sleep(Duration::from_secs(4));
    assert!(!h.panicked(), "arca panicked while idle");
    let c1 = h.cpu_jiffies();
    let elapsed = w0.elapsed().as_secs_f64();

    let cpu_pct = match (c0, c1) {
        (Some(a), Some(b)) => (b.saturating_sub(a) as f64) / (tck as f64 * elapsed) * 100.0,
        _ => -1.0,
    };
    eprintln!(
        "perf: first_frame={:.0}ms idle_cpu={:.1}% (window {:.1}s, CLK_TCK={tck})",
        first_frame.as_secs_f64() * 1000.0,
        cpu_pct,
        elapsed
    );

    let exited = h.quit(Duration::from_secs(8));
    assert!(exited, "arca did not exit within 8s of 'q'");

    // Busy-spin regression guard. The 200ms poll loop idles in the low single
    // digits; a spin pegs ~100% of a core. The ceiling is generous for CI jitter
    // but far below a spin. (-1.0 => /proc read failed; skip rather than false-fail.)
    if cpu_pct >= 0.0 {
        assert!(
            cpu_pct < 15.0,
            "idle CPU {cpu_pct:.1}% — busy-spin regression? (expected low single digits)"
        );
    }
    assert!(
        first_frame < Duration::from_secs(10),
        "first frame took {first_frame:?} — startup/probe regression?"
    );
}
