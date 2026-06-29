use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::America::Puerto_Rico;

pub use chrono::{DateTime as Dt, Utc as UtcTz};

/// Now as unix seconds.
pub fn now_secs() -> i64 {
    Utc::now().timestamp()
}

/// Render a unix-seconds timestamp as a legible date-time in the operator's
/// display tz (AST). For real event timestamps (alert fired-at, snapshot
/// as-of) where the clock time carries meaning.
pub fn display_ast(ts_secs: i64) -> String {
    match Utc.timestamp_opt(ts_secs, 0).single() {
        Some(utc) => utc
            .with_timezone(&Puerto_Rico)
            .format("%b %d, %Y %H:%M %Z")
            .to_string(),
        None => format!("invalid_ts({ts_secs})"),
    }
}

/// Render a unix-seconds timestamp as a legible calendar date (no clock).
///
/// Date-level data — transaction post dates, predicted next charges, due dates —
/// is stored as midnight UTC. We format it in UTC on purpose: converting to AST
/// (UTC-4) would drag a midnight-UTC date back to the previous evening and print
/// the wrong day. So this is a pure date render, no tz shift, no time-of-day.
pub fn display_date(ts_secs: i64) -> String {
    match Utc.timestamp_opt(ts_secs, 0).single() {
        Some(utc) => utc.format("%a %b %d, %Y").to_string(),
        None => format!("invalid_ts({ts_secs})"),
    }
}

/// Compact month-day (`May 22`) in UTC. For chart axis labels where space is
/// tight. Same midnight-UTC, no-tz-shift reasoning as [`display_date`].
pub fn display_short_date(ts_secs: i64) -> String {
    match Utc.timestamp_opt(ts_secs, 0).single() {
        Some(utc) => utc.format("%b %d").to_string(),
        None => format!("invalid_ts({ts_secs})"),
    }
}

/// ISO calendar date (`YYYY-MM-DD`) in UTC. Machine-readable; for CSV export.
/// Same midnight-UTC reasoning as [`display_date`] — no tz shift.
pub fn display_iso_date(ts_secs: i64) -> String {
    match Utc.timestamp_opt(ts_secs, 0).single() {
        Some(utc) => utc.format("%Y-%m-%d").to_string(),
        None => format!("invalid_ts({ts_secs})"),
    }
}

/// Parse a `YYYY-MM-DD` calendar date to unix seconds at midnight UTC — the
/// inverse of [`display_iso_date`], for operator-entered due dates (a
/// subscription's next charge, a planned expense). Same midnight-UTC convention
/// as [`display_date`]: date-level data, no tz shift. `None` if it doesn't parse.
pub fn parse_ymd(s: &str) -> Option<i64> {
    use chrono::NaiveDate;
    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()?;
    d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp())
}

pub fn parse_utc(ts_secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(ts_secs, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_format() {
        // 2026-05-22 12:00:00 UTC == 08:00 AST.
        let out = display_ast(1_779_796_800);
        assert!(out.contains("08:00"), "got {out}");
        assert!(out.contains("AST"), "got {out}");
    }
}
