//! Monthly Markdown report engine.
//!
//! Ticks hourly. On any tick where:
//!   - today (AST) is day-of-month >= the configured `min_day_of_month` (default 1)
//!   - the local hour (AST) is >= the configured `hour_at_or_after` (default 6)
//!   - the prior-month report's `scheduled_jobs.last_run_at` is older than the
//!     start of the current AST month
//!
//! ...the engine builds the prior-month report, writes it to
//! `<reports_dir>/<YYYY-MM>.md`, and records the run in `scheduled_jobs`.
//! Delivery is out of band: the `arca-xmpp` bridge picks up new report files
//! and pushes them to the operator's JID (see the design spec "Hermes integration").
//!
//! Errors do not abort the loop — they're logged and the next tick retries.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use chrono_tz::America::Puerto_Rico;
use tokio::time::interval;

use arca_core::db::Db;
use arca_core::report::{MonthWindow, build_monthly_report, render_markdown};

use crate::config::ReportsCfg;

const JOB_NAME: &str = "report.monthly";

pub struct ReportsEngine {
    db: Arc<Db>,
    cfg: ReportsCfg,
}

impl ReportsEngine {
    pub fn new(db: Arc<Db>, cfg: ReportsCfg) -> Self {
        Self { db, cfg }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(self.cfg.check_interval_secs));
        tick.tick().await; // skip immediate first
        loop {
            tick.tick().await;
            if let Err(e) = self.maybe_run(Utc::now()) {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    source = %e.source().map(ToString::to_string).unwrap_or_default(),
                    "reports: maybe_run"
                );
            }
        }
    }

    /// Public for tests / on-demand RPC plumbing later.
    pub fn maybe_run(&self, as_of: DateTime<Utc>) -> Result<()> {
        if !is_due(&self.cfg, as_of, self.db.last_job_run(JOB_NAME)?) {
            return Ok(());
        }
        let window = MonthWindow::prior_month_ast(as_of);
        let report = build_monthly_report(&self.db, window).context("build_monthly_report")?;
        let markdown = render_markdown(&report);
        let path = self.write_to_disk(&window, &markdown)?;
        tracing::info!(
            label = %window.label(),
            path = %path.display(),
            "reports: monthly run written"
        );
        self.db.record_job_run(JOB_NAME, as_of.timestamp(), "ok")?;
        Ok(())
    }

    fn write_to_disk(&self, window: &MonthWindow, markdown: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.cfg.reports_dir)
            .with_context(|| format!("mkdir {}", self.cfg.reports_dir.display()))?;
        let path = self.cfg.reports_dir.join(format!("{}.md", window.label()));
        std::fs::write(&path, markdown).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }
}

/// Pure decision logic — extracted so tests can drive it directly without
/// spawning a tokio interval.
fn is_due(cfg: &ReportsCfg, as_of: DateTime<Utc>, last_run: Option<i64>) -> bool {
    let local = as_of.with_timezone(&Puerto_Rico);
    if local.day() < cfg.min_day_of_month {
        return false;
    }
    if local.hour() < cfg.hour_at_or_after {
        return false;
    }
    let month_start_local = Puerto_Rico
        .with_ymd_and_hms(local.year(), local.month(), 1, 0, 0, 0)
        .single();
    let month_start_secs = match month_start_local {
        Some(d) => d.with_timezone(&Utc).timestamp(),
        None => return false,
    };
    match last_run {
        Some(t) => t < month_start_secs,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg(dir: PathBuf) -> ReportsCfg {
        ReportsCfg {
            reports_dir: dir,
            check_interval_secs: 3600,
            min_day_of_month: 1,
            hour_at_or_after: 6,
        }
    }

    #[test]
    fn not_due_before_min_day() {
        let c = cfg(PathBuf::from("/tmp"));
        // 2026-05-02 17:00 UTC = 13:00 AST on day 2.
        let as_of = Utc.with_ymd_and_hms(2026, 5, 2, 17, 0, 0).single().unwrap();
        let mut c2 = c.clone();
        c2.min_day_of_month = 3;
        assert!(!is_due(&c2, as_of, None));
    }

    #[test]
    fn not_due_before_hour() {
        let c = cfg(PathBuf::from("/tmp"));
        // 04:00 UTC = 00:00 AST.
        let as_of = Utc.with_ymd_and_hms(2026, 5, 1, 4, 0, 0).single().unwrap();
        assert!(!is_due(&c, as_of, None));
    }

    #[test]
    fn due_on_first_at_or_after_hour() {
        let c = cfg(PathBuf::from("/tmp"));
        // 12:00 UTC = 08:00 AST on day 1.
        let as_of = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap();
        assert!(is_due(&c, as_of, None));
    }

    #[test]
    fn not_due_if_last_run_this_month() {
        let c = cfg(PathBuf::from("/tmp"));
        let as_of = Utc.with_ymd_and_hms(2026, 5, 3, 12, 0, 0).single().unwrap();
        // last_run yesterday — same May AST month.
        let last_run = Utc
            .with_ymd_and_hms(2026, 5, 2, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert!(!is_due(&c, as_of, Some(last_run)));
    }

    #[test]
    fn due_if_last_run_previous_month() {
        let c = cfg(PathBuf::from("/tmp"));
        let as_of = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap();
        let last_run = Utc
            .with_ymd_and_hms(2026, 4, 15, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert!(is_due(&c, as_of, Some(last_run)));
    }

    #[test]
    fn maybe_run_writes_file_and_records_job() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_memory().unwrap());
        let c = cfg(tmp.path().to_path_buf());
        let engine = ReportsEngine::new(Arc::clone(&db), c);
        // Force a "due" timestamp.
        let as_of = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).single().unwrap();
        engine.maybe_run(as_of).unwrap();
        let path = tmp.path().join("2026-04.md");
        assert!(path.exists(), "expected {} to exist", path.display());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# arca monthly report — 2026-04"));
        assert!(
            db.last_job_run(JOB_NAME).unwrap().is_some(),
            "scheduled_jobs row should be written"
        );
    }
}
