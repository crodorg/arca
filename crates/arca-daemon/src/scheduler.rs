//! Scheduler: poll each provider per its configured cadence.
//!
//! Cadence is a string in `providers.poll_cadence`. Phase 2 vocabulary:
//!   - `manual`     : never polled by the scheduler
//!   - `hourly`     : every 3600 s
//!   - `daily`      : every 86_400 s
//!   - `weekly`     : every 604_800 s
//!   - `every:NNN`  : every NNN seconds (testing / fine-grained control)
//!
//! Cron-like is deferred to Phase 6. Bad cadence strings are logged and treated
//! as `manual` (no polling).

use std::sync::Arc;
use std::time::Duration;

use arca_core::db::Db;
use arca_core::ids::ProviderId;
use arca_core::provider::{Ctx, Cursor, RefreshReport};
use arca_core::time::now_secs;

use crate::providers::registry::LoadedProvider;

pub struct Scheduler {
    providers: Arc<Vec<LoadedProvider>>,
    db: Arc<Db>,
}

impl Scheduler {
    pub fn new(providers: Arc<Vec<LoadedProvider>>, db: Arc<Db>) -> Self {
        Self { providers, db }
    }

    pub async fn run(self) {
        // Tick every minute; per-provider cadence checked against last_poll_at.
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            for lp in self.providers.iter() {
                let cadence = parse_cadence(&lp.row.poll_cadence);
                let Some(interval_s) = cadence else { continue };
                let last = current_last_poll(&self.db, lp.row.id);
                let due = match last {
                    Some(t) => now_secs() - t >= interval_s,
                    None => true,
                };
                if !due {
                    continue;
                }
                let _ = poll_once(&self.db, lp).await;
            }
        }
    }
}

/// Poll one provider: balances + transactions + record_poll. Returns a report
/// suitable for the `provider.refresh` RPC. Errors do not propagate — they
/// are folded into the report's `message` and `last_status` is written.
pub async fn poll_once(db: &Arc<Db>, lp: &LoadedProvider) -> RefreshReport {
    let kind = lp.impl_.kind();
    let label = lp.impl_.label().to_string();
    tracing::info!(provider = kind, label = %label, "polling");
    let ctx = Ctx::new(Arc::clone(db));

    let mut status = "ok".to_string();
    let mut total_rows: u32 = 0;
    let mut messages = Vec::new();

    match lp.impl_.refresh_balances(&ctx).await {
        Ok(r) => {
            total_rows += r.rows_written;
            if let Some(m) = r.message {
                messages.push(format!("balances: {m}"));
            }
        }
        Err(e) => {
            tracing::warn!(provider = kind, error = %e, "refresh_balances");
            status = format!("err:balance:{e}");
            messages.push(format!("balances error: {e}"));
        }
    }
    match lp.impl_.refresh_transactions(&ctx, None).await {
        Ok(r) => {
            total_rows += r.rows_written;
            if let Some(m) = r.message {
                messages.push(format!("transactions: {m}"));
            }
        }
        Err(e) => {
            tracing::warn!(provider = kind, error = %e, "refresh_transactions");
            if status == "ok" {
                status = format!("err:tx:{e}");
            }
            messages.push(format!("transactions error: {e}"));
        }
    }
    if let Err(e) = db.record_poll(lp.row.id, &status) {
        tracing::warn!(error = %e, "record_poll");
    }

    RefreshReport {
        provider_kind: format!("{kind}:{label}"),
        rows_written: total_rows,
        cursor: Cursor::default(),
        message: Some(messages.join("; ")),
    }
}

fn parse_cadence(s: &str) -> Option<i64> {
    match s {
        "manual" => None,
        "hourly" => Some(3_600),
        "daily" => Some(86_400),
        "weekly" => Some(604_800),
        other if other.starts_with("every:") => other["every:".len()..].parse::<i64>().ok(),
        _ => {
            tracing::warn!(cadence = s, "unknown poll_cadence, treating as manual");
            None
        }
    }
}

fn current_last_poll(db: &Db, id: ProviderId) -> Option<i64> {
    db.list_providers().ok().and_then(|rows| {
        rows.into_iter()
            .find(|r| r.id == id)
            .and_then(|r| r.last_poll_at)
    })
}
