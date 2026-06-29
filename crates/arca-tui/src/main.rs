mod app;
mod client;

use std::io;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use arca_core::rpc::{Request, Response, Scope};
use arca_core::time::display_iso_date;

use crate::app::{App, Connection};

/// arca — personal finance client. With no subcommand, launches the TUI; with a
/// verb (money, debt, pp, …) runs a one-shot query and prints JSON.
#[derive(Parser, Debug)]
#[command(name = "arca", version)]
struct Cli {
    /// Unix socket path. Defaults to the operator-only write socket (serves all
    /// verbs). The arca-xmpp bridge instead targets the read socket explicitly:
    /// `--socket /var/run/arca/read.sock`.
    #[arg(
        long,
        conflicts_with = "tcp",
        default_value = "/var/run/arca/write.sock"
    )]
    socket: PathBuf,

    /// TCP host:port (loopback only; latent — remote access is the mesh-SSH TUI
    /// over the Unix socket, not raw TCP).
    #[arg(long)]
    tcp: Option<String>,

    /// Render each view once and exit — for CI smoke tests.
    #[arg(long)]
    smoke: bool,

    /// Send one RPC request as JSON, print JSON response, exit. No TUI.
    /// Reads from stdin when value is "-".
    #[arg(long, value_name = "JSON")]
    cmd: Option<String>,

    /// One-shot query verb. Prints the JSON response and exits; no TUI.
    /// This is what the arca-xmpp bridge / Hermes allowlist target.
    #[command(subcommand)]
    verb: Option<Verb>,
}

/// Friendly one-shot verbs that map onto RPC `Request` kinds. Read verbs are
/// safe for the bridge's read socket; `refresh` mutates and is operator-only.
#[derive(clap::Subcommand, Debug)]
enum Verb {
    /// Net-worth + cash snapshot (read).
    Money,
    /// Permanent-portfolio (Tier-2) drift + Tier-1 backbone (read).
    Pp,
    /// Per-business P&L (read).
    Business {
        /// Business tag, e.g. `main`.
        tag: String,
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
    },
    /// Debt balances + scheduled debt service (read).
    Debt {
        #[arg(long, value_enum, default_value = "month")]
        scope: ScopeArg,
    },
    /// Transactions, filterable (read).
    Tx {
        /// Unix-seconds lower bound.
        #[arg(long)]
        since: Option<i64>,
        /// Filter by tag (e.g. `income`).
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Export transactions as CSV to stdout (read). Redirect to a file, e.g.
    /// `arca export > tx.csv`. Columns: posted_at,date,account,amount_cents,
    /// amount,category,tag,description.
    Export {
        /// Unix-seconds lower bound.
        #[arg(long)]
        since: Option<i64>,
        /// Filter by tag (e.g. `income`).
        #[arg(long)]
        tag: Option<String>,
        /// Row cap (default 100000 — effectively all).
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Recurring payees from transaction history — subs/bills/debts (read).
    Recurring {
        /// Unix-seconds lower bound on history.
        #[arg(long)]
        since: Option<i64>,
        /// Minimum occurrences to call a payee recurring (default 3).
        #[arg(long)]
        min: Option<usize>,
    },
    /// Daemon liveness + last-poll status (read).
    Health,
    /// Recent alerts — the undelivered queue unless `--all` (read).
    Alerts {
        /// Include already-delivered alerts, not just the pending queue.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Trigger a provider refresh (write; operator-only once sockets split).
    Refresh {
        /// Limit to one provider kind, e.g. `plaid`.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Register or update a provider row — the runtime way to wire a
    /// credentialed provider like plaid/mercury/stripe (write; operator-only).
    ProviderSet {
        /// Provider kind, e.g. `plaid`. Must be one the registry can build.
        #[arg(long)]
        kind: String,
        /// Unique label (the upsert key with kind), e.g. `Plaid - First Bank`.
        #[arg(long)]
        label: String,
        /// config_json (JSON object), e.g.
        /// `{"plaid_env":"sandbox","institution_name":"Sandbox Bank"}`.
        #[arg(long)]
        config: Option<String>,
        /// `secrets.age` key holding this item's token, e.g.
        /// `plaid_navyfed_access_token`.
        #[arg(long)]
        secret_ref: Option<String>,
        /// Poll cadence: daily|weekly|hourly|manual (default daily).
        #[arg(long)]
        cadence: Option<String>,
    },
    /// Create or update an alert rule (write; operator-only).
    AlertSet {
        /// Rule name (unique key — re-using a name updates it).
        #[arg(long)]
        name: String,
        /// rule_json, e.g. `{"kind":"provider.stale","max_age_secs":259200}`.
        #[arg(long)]
        rule: String,
        #[arg(long)]
        channel: Option<String>,
        /// Create the rule disabled.
        #[arg(long)]
        inactive: bool,
    },
    /// Label a detected recurring series as a sub/bill/debt (write; operator-only).
    /// `--match-key` is the `payee` field from `arca recurring`.
    RecurringConfirm {
        /// Normalized payee key from `arca recurring` (e.g. `netflix`).
        #[arg(long)]
        match_key: String,
        /// Label: sub | bill | debt | ignore. Omit to rename only (`--name`).
        #[arg(long)]
        label: Option<String>,
        /// Optional friendly name overriding the raw descriptor.
        #[arg(long)]
        name: Option<String>,
        /// Optional business tag to attribute the series to.
        #[arg(long)]
        business: Option<String>,
        /// Soft-dismiss the label instead of enabling it.
        #[arg(long)]
        dismiss: bool,
    },
    /// Create or update a business/venture tag so providers & accounts can bind
    /// to it via `--business` (write; operator-only). Upserts on `--tag`.
    BusinessSet {
        /// Stable tag key (lowercase, no spaces), e.g. `acme`.
        #[arg(long)]
        tag: String,
        /// Friendly display name (defaults to the tag on first insert).
        #[arg(long)]
        name: Option<String>,
        /// Mark the business inactive (re-running without this re-enables it).
        #[arg(long)]
        inactive: bool,
    },
    /// Declare a recurring obligation (rent, insurance, a fixed bill). Stored as
    /// a `recurring` subscription; shown in Bills/upcoming projected forward by
    /// cadence and on the calendar/report. NOT debt — never affects net worth.
    SubscriptionSet {
        /// Obligation name, e.g. `Rent`. Upserts on this.
        #[arg(long)]
        name: String,
        /// Amount in dollars; negative for an outflow, e.g. `-2000`.
        #[arg(long)]
        amount: String,
        /// monthly | yearly | quarterly | weekly | biweekly.
        #[arg(long)]
        cadence: String,
        /// Next due date: `YYYY-MM-DD` or raw unix seconds.
        #[arg(long = "next", value_parser = parse_next_charge)]
        next_charge_at: i64,
        /// Bind to a business tag (optional).
        #[arg(long)]
        business: Option<String>,
        /// Mark inactive (re-running without this re-enables it).
        #[arg(long)]
        inactive: bool,
    },
}

/// Parse the `--next` due date: a raw unix-seconds integer or a `YYYY-MM-DD`
/// calendar date (midnight UTC, matching the date-level storage convention).
fn parse_next_charge(s: &str) -> Result<i64, String> {
    if let Ok(ts) = s.trim().parse::<i64>() {
        return Ok(ts);
    }
    arca_core::time::parse_ymd(s)
        .ok_or_else(|| format!("invalid date '{s}'; use YYYY-MM-DD or unix seconds"))
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ScopeArg {
    Month,
    Year,
    Ytd,
    All,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::Month => Scope::Month,
            ScopeArg::Year => Scope::Year,
            ScopeArg::Ytd => Scope::Ytd,
            ScopeArg::All => Scope::All,
        }
    }
}

impl Verb {
    fn into_request(self) -> Request {
        match self {
            Verb::Money => Request::SnapshotMoney,
            Verb::Pp => Request::SnapshotPp,
            Verb::Business { tag, scope } => Request::SnapshotBusiness {
                tag,
                scope: scope.map(Into::into),
            },
            Verb::Debt { scope } => Request::SnapshotDebt {
                scope: scope.into(),
            },
            Verb::Tx { since, tag, limit } => Request::TxList { since, tag, limit },
            Verb::Recurring { since, min } => Request::RecurringList {
                since,
                min_occurrences: min,
                include_ignored: None,
            },
            // Export emits CSV, not a JSON response; main() intercepts it before
            // this maps to a Request.
            Verb::Export { .. } => unreachable!("export is handled in main()"),
            Verb::ProviderSet {
                kind,
                label,
                config,
                secret_ref,
                cadence,
            } => Request::ManualUpsertProvider {
                provider_kind: kind,
                label,
                config_json: config,
                secret_ref,
                poll_cadence: cadence,
            },
            Verb::Health => Request::Health,
            Verb::Alerts { all, limit } => Request::AlertPending {
                limit,
                include_delivered: Some(all),
            },
            Verb::Refresh { provider } => Request::ProviderRefresh {
                kind_filter: provider,
            },
            Verb::AlertSet {
                name,
                rule,
                channel,
                inactive,
            } => Request::AlertUpsert {
                name,
                rule_json: rule,
                channel,
                active: Some(!inactive),
            },
            Verb::RecurringConfirm {
                match_key,
                label,
                name,
                business,
                dismiss,
            } => Request::RecurringConfirm {
                match_key,
                label,
                display_name: name,
                business_tag: business,
                active: Some(!dismiss),
            },
            Verb::BusinessSet {
                tag,
                name,
                inactive,
            } => Request::ManualUpsertBusiness {
                tag,
                display_name: name,
                active: Some(!inactive),
            },
            Verb::SubscriptionSet {
                name,
                amount,
                cadence,
                next_charge_at,
                business,
                inactive,
            } => Request::ManualUpsertSubscription {
                name,
                amount,
                cadence,
                next_charge_at,
                business_tag: business,
                active: Some(!inactive),
            },
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = if let Some(addr) = cli.tcp.clone() {
        Connection {
            socket: None,
            tcp: Some(addr),
        }
    } else {
        Connection {
            socket: Some(cli.socket.clone()),
            tcp: None,
        }
    };

    if let Some(raw) = cli.cmd.as_deref() {
        return cmd_mode(&conn, raw).await;
    }

    if let Some(verb) = cli.verb {
        match verb {
            Verb::Export { since, tag, limit } => {
                return export_csv(&conn, since, tag, limit).await;
            }
            other => return call_and_emit(&conn, &other.into_request()).await,
        }
    }

    if cli.smoke {
        return smoke_test(conn).await;
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut a = App::new(conn);
    let result = app::event_loop(&mut term, &mut a).await;

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

/// One-shot RPC from a raw JSON request string (or stdin when "-").
async fn cmd_mode(conn: &Connection, raw: &str) -> Result<()> {
    let json_text = if raw == "-" {
        use std::io::Read;
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        s
    } else {
        raw.to_string()
    };
    let req: Request =
        serde_json::from_str(json_text.trim()).map_err(|e| anyhow!("parse request: {e}"))?;
    call_and_emit(conn, &req).await
}

/// Send one request, print the response as pretty JSON, and exit. Exit codes:
/// 0 success / 2 on `Response::Error` / 1 on transport or parse error.
async fn call_and_emit(conn: &Connection, req: &Request) -> Result<()> {
    let resp = client::call(conn.transport(), req).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    if matches!(resp, Response::Error(_)) {
        std::process::exit(2);
    }
    Ok(())
}

/// Pull transactions and print CSV to stdout. Reuses the read-only `tx.list`
/// verb, so it works over the bridge's read socket too.
async fn export_csv(
    conn: &Connection,
    since: Option<i64>,
    tag: Option<String>,
    limit: Option<i64>,
) -> Result<()> {
    let req = Request::TxList {
        since,
        tag,
        limit: Some(limit.unwrap_or(100_000)),
    };
    let page = match client::call(conn.transport(), &req).await? {
        Response::TxList(p) => p,
        Response::Error(e) => return Err(anyhow!("{}: {}", e.code, e.msg)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    let mut out =
        String::from("posted_at,date,account,amount_cents,amount,category,tag,description\n");
    for r in page.rows {
        // Numeric/date columns are machine-generated → plain escape. Free-text
        // columns are externally sourced (merchant names via Plaid) → also
        // neutralize spreadsheet formula injection.
        let cells = [
            csv_field(&r.posted_at.to_string()),
            csv_field(&display_iso_date(r.posted_at)),
            csv_text(&r.account),
            csv_field(&r.amount.as_i64().to_string()),
            csv_field(&dollars_plain(r.amount.as_i64())),
            csv_text(&r.category.unwrap_or_default()),
            csv_text(&r.tag.unwrap_or_default()),
            csv_text(&r.description.unwrap_or_default()),
        ];
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}

/// Plain decimal dollars (no `$`, no grouping) for spreadsheet import: `-12.34`.
fn dollars_plain(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let a = cents.unsigned_abs();
    format!("{sign}{}.{:02}", a / 100, a % 100)
}

/// RFC-4180-ish CSV escaping: quote fields containing comma, quote, CR or LF;
/// double any embedded quote.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// CSV-escape an externally-sourced *text* field, additionally neutralizing
/// spreadsheet formula injection: a leading `= + - @` (or tab/CR) makes
/// Excel/Sheets evaluate the cell as a formula, so we prefix such a field with a
/// single quote to force literal text. Numeric columns we generate ourselves
/// skip this (a `'`-prefixed negative amount would not parse).
fn csv_text(s: &str) -> String {
    let needs_guard = s
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '\t' | '\r'));
    if needs_guard {
        csv_field(&format!("'{s}"))
    } else {
        csv_field(s)
    }
}

/// Try each view once; report which ones rendered without panic. Does not require
/// a live daemon — if RPC fails, the view shows the error and we still pass the
/// render smoke test.
async fn smoke_test(conn: Connection) -> Result<()> {
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut term = Terminal::new(backend).map_err(|e| anyhow!("test backend: {e}"))?;

    let mut a = App::new(conn);
    for v in [
        crate::app::View::Menu,
        crate::app::View::Money,
        crate::app::View::Business,
        crate::app::View::Pp,
        crate::app::View::Bills,
        crate::app::View::Tx,
        crate::app::View::Alerts,
        crate::app::View::Charts,
        crate::app::View::Expenses,
        crate::app::View::Help,
    ] {
        a.view = v;
        a.refresh_current().await;
        app::render(&mut term, &mut a)?;
        println!("rendered {:?}", v);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{csv_field, csv_text, dollars_plain};

    #[test]
    fn dollars_plain_signs_and_pads() {
        assert_eq!(dollars_plain(0), "0.00");
        assert_eq!(dollars_plain(-5), "-0.05");
        assert_eq!(dollars_plain(-199), "-1.99");
        assert_eq!(dollars_plain(123_456), "1234.56");
    }

    #[test]
    fn csv_field_quotes_specials() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("she said \"hi\""), "\"she said \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn csv_text_neutralizes_formula_injection() {
        assert_eq!(csv_text("=1+1"), "'=1+1");
        assert_eq!(csv_text("+1"), "'+1");
        assert_eq!(csv_text("@SUM(A1)"), "'@SUM(A1)");
        assert_eq!(csv_text("-cmd"), "'-cmd");
        // guard then quote when the value also needs escaping
        assert_eq!(csv_text("=a,b"), "\"'=a,b\"");
        // ordinary text is left alone
        assert_eq!(csv_text("Netflix"), "Netflix");
    }
}
