//! Weekly .ics digest engine.
//!
//! Ticks hourly. On any tick where:
//!   - today (AST) is the configured `day_of_week` (default Monday)
//!   - the local hour (AST) is >= the configured `hour_at_or_after` (default 7)
//!   - `scheduled_jobs.last_run_at` for `digest.weekly` is older than the start
//!     of the current AST week (Monday 00:00 AST)
//!
//! ...the engine builds a VCALENDAR from upcoming planned expenses +
//! subscription renewals (next `lookahead_days`, default 30), writes it to
//! `<ics_dir>`, and records the run. Delivery is out of band: the `arca-xmpp`
//! bridge picks up new .ics files and pushes them to the operator's JID (see
//! the design spec "Hermes integration").

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::America::Puerto_Rico;
use tokio::time::interval;

use arca_core::calendar::{build_weekly_digest, count_upcoming, upcoming_events, weekly_filename};
use arca_core::db::Db;

use crate::config::CalendarCfg;

const JOB_NAME: &str = "digest.weekly";

pub struct CalendarEngine {
    db: Arc<Db>,
    cfg: CalendarCfg,
}

impl CalendarEngine {
    pub fn new(db: Arc<Db>, cfg: CalendarCfg) -> Self {
        Self { db, cfg }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(self.cfg.check_interval_secs));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = self.maybe_run(Utc::now()) {
                tracing::warn!(error = %e, "calendar: maybe_run");
            }
        }
    }

    pub fn maybe_run(&self, as_of: DateTime<Utc>) -> Result<()> {
        if !is_due(&self.cfg, as_of, self.db.last_job_run(JOB_NAME)?) {
            return Ok(());
        }
        let count = count_upcoming(&self.db, as_of.timestamp(), self.cfg.lookahead_days)?;
        if count == 0 && !self.cfg.send_when_empty {
            // Still record the run so we don't recheck every hour for a week.
            self.db
                .record_job_run(JOB_NAME, as_of.timestamp(), "ok:empty")?;
            tracing::info!("calendar: nothing upcoming, skip send");
            return Ok(());
        }
        let ics = build_weekly_digest(&self.db, as_of.timestamp(), self.cfg.lookahead_days)
            .context("build_weekly_digest")?;
        // Persist a copy alongside reports for the operator.
        let path = self.write_to_disk(as_of.timestamp(), &ics)?;
        tracing::info!(
            count = count,
            path = %path.display(),
            "calendar: weekly run written"
        );
        self.db.record_job_run(JOB_NAME, as_of.timestamp(), "ok")?;
        Ok(())
    }

    fn write_to_disk(&self, ts: i64, ics: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.cfg.ics_dir)
            .with_context(|| format!("mkdir {}", self.cfg.ics_dir.display()))?;
        let path = self.cfg.ics_dir.join(weekly_filename(ts));
        std::fs::write(&path, ics).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }
}

/// Public for tests / on-demand callers — actually fetches and returns the
/// rendered VCALENDAR text without writing or sending.
pub fn preview(db: &Db, as_of_secs: i64, days: i64) -> Result<String> {
    let _ = upcoming_events(db, as_of_secs, days)?;
    Ok(build_weekly_digest(db, as_of_secs, days)?)
}

fn is_due(cfg: &CalendarCfg, as_of: DateTime<Utc>, last_run: Option<i64>) -> bool {
    let local = as_of.with_timezone(&Puerto_Rico);
    if local.weekday() != cfg.day_of_week {
        return false;
    }
    if local.hour() < cfg.hour_at_or_after {
        return false;
    }
    let Some(week_start) = start_of_week(local.date_naive(), cfg.day_of_week) else {
        return false;
    };
    let week_start_local = Puerto_Rico
        .with_ymd_and_hms(
            week_start.year(),
            week_start.month(),
            week_start.day(),
            0,
            0,
            0,
        )
        .single();
    let week_start_secs = match week_start_local {
        Some(d) => d.with_timezone(&Utc).timestamp(),
        None => return false,
    };
    match last_run {
        Some(t) => t < week_start_secs,
        None => true,
    }
}

fn start_of_week(today: NaiveDate, day_of_week: Weekday) -> Option<NaiveDate> {
    // Walk backward until we land on `day_of_week`. For the configured day,
    // this just returns today; otherwise rewinds up to 6 days.
    let mut d = today;
    for _ in 0..7 {
        if d.weekday() == day_of_week {
            return Some(d);
        }
        d = d.pred_opt()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    fn cfg(dir: PathBuf) -> CalendarCfg {
        CalendarCfg {
            ics_dir: dir,
            check_interval_secs: 3600,
            day_of_week: Weekday::Mon,
            hour_at_or_after: 7,
            lookahead_days: 30,
            send_when_empty: false,
        }
    }

    #[test]
    fn not_due_wrong_weekday() {
        let c = cfg(PathBuf::from("/tmp"));
        // 2026-05-22 is a Friday → 13:00 UTC = 09:00 AST Friday.
        let as_of = Utc
            .with_ymd_and_hms(2026, 5, 22, 13, 0, 0)
            .single()
            .unwrap();
        assert!(!is_due(&c, as_of, None));
    }

    #[test]
    fn due_on_monday_after_hour() {
        let c = cfg(PathBuf::from("/tmp"));
        // 2026-05-25 is a Monday → 12:00 UTC = 08:00 AST.
        let as_of = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .unwrap();
        assert!(is_due(&c, as_of, None));
    }

    #[test]
    fn not_due_if_already_run_this_week() {
        let c = cfg(PathBuf::from("/tmp"));
        let as_of = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .unwrap();
        // last_run earlier same day.
        let last = Utc
            .with_ymd_and_hms(2026, 5, 25, 11, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert!(!is_due(&c, as_of, Some(last)));
    }

    #[test]
    fn empty_with_send_when_empty_false_records_job() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        let c = cfg(tmp.path().to_path_buf());
        let engine = CalendarEngine::new(Arc::clone(&db), c);
        let as_of = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .unwrap();
        engine.maybe_run(as_of).unwrap();
        assert!(db.last_job_run(JOB_NAME).unwrap().is_some());
    }

    #[test]
    fn non_empty_writes_ics() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO accounts (name, kind, currency, active, created_at)
                 VALUES ('CC', 'debt', 'USD', 1, 0)",
                [],
            )
            .map_err(Into::into)
        })
        .unwrap();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO planned_expenses (due_at, amount_cents, description, account_id, status)
                 VALUES (?1, ?2, 'Rent', 1, 'planned')",
                params![1_780_000_000_i64, -150_000_i64],
            )
            .map_err(Into::into)
        })
        .unwrap();
        let c = cfg(tmp.path().to_path_buf());
        let engine = CalendarEngine::new(Arc::clone(&db), c);
        // Pick an AS-OF that's Monday, in the as_of < due_at window.
        let as_of = Utc
            .with_ymd_and_hms(2026, 5, 25, 12, 0, 0)
            .single()
            .unwrap();
        // No delivery in-process; the .ics is still written to disk.
        engine.maybe_run(as_of).unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(std::ffi::OsStr::to_str) == Some("ics"))
            .collect();
        assert!(!entries.is_empty(), "no .ics file written");
    }
}
