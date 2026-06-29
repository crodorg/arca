#![allow(dead_code)] // Phase 1: forward-compat fields used by later phases.

use std::path::PathBuf;

use chrono::Weekday;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub daemon: DaemonCfg,
    #[serde(default)]
    pub alerts: AlertsCfg,
    #[serde(default)]
    pub reports: ReportsCfg,
    #[serde(default)]
    pub calendar: CalendarCfg,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DaemonCfg {
    pub db_path: PathBuf,
    pub log_path: PathBuf,
    /// Read socket: `snapshot.*`, `tx.list`, `health`. Mode 0660; the arca-xmpp
    /// bridge connects here. Write verbs are rejected at dispatch.
    #[serde(default = "default_read_socket")]
    pub read_socket_path: PathBuf,
    /// Write socket: read verbs plus `manual.*` and `provider.refresh`. Mode
    /// 0660 and gated to `operator_uid` via getpeereid(2). The TUI connects here.
    #[serde(default = "default_write_socket")]
    pub write_socket_path: PathBuf,
    /// Numeric UID allowed to send write verbs over `write_socket_path`
    /// (`id -u <operator>`). If unset, the write socket fails closed — every
    /// connection is rejected. TCP (over WireGuard) is gated by pf, not this.
    #[serde(default)]
    pub operator_uid: Option<u32>,
    #[serde(default = "default_pid_path")]
    pub pid_path: PathBuf,
    pub tcp_bind: String,
    pub secrets_age: Option<PathBuf>,
    pub secrets_key: Option<PathBuf>,
    #[serde(default = "default_tz")]
    pub tz_display: String,
}

/// Configuration for the alert engine. All fields have defaults so an absent
/// `[alerts]` section yields a working engine — it ticks, evaluates rules, and
/// records breaches to `alert_history`. Delivery is out of band: the arca-xmpp
/// bridge pushes undelivered rows to the operator's JID.
#[derive(Clone, Debug, Deserialize)]
pub struct AlertsCfg {
    /// Seconds between evaluation ticks. Default 300 (5 min) — frequent enough
    /// that a minute-precise `reminder` (e.g. 10:30) fires punctually; the
    /// per-tick cost is a handful of in-memory predicates + SELECTs.
    #[serde(default = "default_alert_check_interval")]
    pub check_interval_secs: u64,
    /// Dedup window — same rule won't re-fire within this many seconds.
    /// Default 86400 (24h).
    #[serde(default = "default_dedup_window")]
    pub dedup_window_secs: i64,
}

impl Default for AlertsCfg {
    fn default() -> Self {
        Self {
            check_interval_secs: default_alert_check_interval(),
            dedup_window_secs: default_dedup_window(),
        }
    }
}

fn default_check_interval() -> u64 {
    3_600
}
fn default_alert_check_interval() -> u64 {
    300
}
fn default_dedup_window() -> i64 {
    86_400
}

/// Configuration for the monthly Markdown report engine. Defaults to "build on
/// the 1st of each month at or after 06:00 AST"; writes the .md file under
/// `reports_dir` (delivery is out of band, handled by arca-xmpp).
#[derive(Clone, Debug, Deserialize)]
pub struct ReportsCfg {
    #[serde(default = "default_reports_dir")]
    pub reports_dir: PathBuf,
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,
    /// Earliest day of the (AST) month on which a run may fire. Default 1.
    #[serde(default = "default_min_day")]
    pub min_day_of_month: u32,
    /// AST hour-of-day threshold for the run. Default 6.
    #[serde(default = "default_run_hour")]
    pub hour_at_or_after: u32,
}

impl Default for ReportsCfg {
    fn default() -> Self {
        Self {
            reports_dir: default_reports_dir(),
            check_interval_secs: default_check_interval(),
            min_day_of_month: default_min_day(),
            hour_at_or_after: default_run_hour(),
        }
    }
}

fn default_reports_dir() -> PathBuf {
    PathBuf::from("/var/arca/reports")
}
fn default_min_day() -> u32 {
    1
}
fn default_run_hour() -> u32 {
    6
}

/// Configuration for the weekly .ics calendar digest engine. The .ics is written
/// under `ics_dir`; delivery is out of band, handled by arca-xmpp.
#[derive(Clone, Debug, Deserialize)]
pub struct CalendarCfg {
    #[serde(default = "default_ics_dir")]
    pub ics_dir: PathBuf,
    #[serde(default = "default_check_interval")]
    pub check_interval_secs: u64,
    /// Which weekday triggers the digest. Default: Monday.
    #[serde(default = "default_weekday", deserialize_with = "weekday_from_str")]
    pub day_of_week: Weekday,
    #[serde(default = "default_digest_hour")]
    pub hour_at_or_after: u32,
    #[serde(default = "default_lookahead_days")]
    pub lookahead_days: i64,
    /// Send an empty VCALENDAR even when there are zero upcoming events.
    /// Default false (we still record the job-run to avoid hourly re-checks).
    #[serde(default)]
    pub send_when_empty: bool,
}

impl Default for CalendarCfg {
    fn default() -> Self {
        Self {
            ics_dir: default_ics_dir(),
            check_interval_secs: default_check_interval(),
            day_of_week: default_weekday(),
            hour_at_or_after: default_digest_hour(),
            lookahead_days: default_lookahead_days(),
            send_when_empty: false,
        }
    }
}

fn default_ics_dir() -> PathBuf {
    PathBuf::from("/var/arca/reports")
}
fn default_weekday() -> Weekday {
    Weekday::Mon
}
fn default_digest_hour() -> u32 {
    7
}
fn default_lookahead_days() -> i64 {
    30
}

fn weekday_from_str<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Weekday, D::Error> {
    use serde::de::{Error, Unexpected};
    let s = String::deserialize(d)?;
    match s.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        _ => Err(D::Error::invalid_value(
            Unexpected::Str(&s),
            &"weekday name",
        )),
    }
}

fn default_tz() -> String {
    "America/Puerto_Rico".into()
}

fn default_read_socket() -> PathBuf {
    PathBuf::from("/var/run/arca/read.sock")
}

fn default_write_socket() -> PathBuf {
    PathBuf::from("/var/run/arca/write.sock")
}

fn default_pid_path() -> PathBuf {
    PathBuf::from("/var/run/arca/arca.pid")
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(cfg)
    }
}
