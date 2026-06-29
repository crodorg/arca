//! Alert engine.
//!
//! Reads `alert_rules`, evaluates each on a tick, and writes results to
//! `alert_history` with `delivered=false`. Delivery is out of band: the
//! `arca-xmpp` bridge polls undelivered rows and pushes them to the operator's
//! JID (see the Hermes integration section in the design spec). The daemon itself
//! never opens a network connection or spawns a process to deliver.
//!
//! Supported rule kinds (`rule_json.kind`):
//!   - `pp.band_breach` — any Tier-2 sleeve outside the 22/44 rebalance bands.
//!     Operator-defined (NOT auto-seeded): drift is computed from
//!     balance_snapshots, so until a real brokerage feed populates the T2
//!     sleeves the breach is an estimate. Opt in with `alert.upsert` once the
//!     sleeves carry real numbers.
//!   - `provider.stale` — an automatic poller is failing (`last_status` of
//!     `error`/`stale`) or has gone silent past its cadence's grace window.
//!     This is the honest-failure tripwire: a Plaid token expiry or a Mercury
//!     401 surfaces to the operator instead of dying quietly.
//!   - `balance.low` — a named account's latest balance fell below a threshold
//!     (`{"account": "...", "min_cents": N}`). Operator-defined (not seeded);
//!     overdraft / runway protection.
//!   - `bandwidth.high` — a named account's latest snapshot rose above a
//!     threshold (`{"account": "...", "max_gb": N}`). Operator-defined (not
//!     seeded); pairs with the `vultr` provider's month-to-date egress account
//!     to warn before a VPS hits its transfer quota.
//!
//! Dedup: a rule that already fired within `dedup_window_secs` is skipped.
//! History rows for skipped evaluations are not written — only an actual fire
//! lands in `alert_history`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::America::Puerto_Rico;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::interval;

use arca_core::db::{AlertRule, Db, ProviderRow};
use arca_core::money::Cents;
use arca_core::pp::{Allocation, DriftRow, allocation, any_band_breach, drift};
use arca_core::time::now_secs;

/// Staleness threshold for a provider whose `poll_cadence` isn't one of the
/// known intervals: 3 days without a successful poll. Overridable per-rule via
/// `rule_json` `{"kind":"provider.stale","max_age_secs":N}`.
const DEFAULT_STALE_MAX_AGE_SECS: i64 = 3 * 86_400;

use crate::config::AlertsCfg;

pub struct AlertEngine {
    db: Arc<Db>,
    cfg: AlertsCfg,
}

impl AlertEngine {
    pub fn new(db: Arc<Db>, cfg: AlertsCfg) -> Self {
        // pp.band_breach is deliberately NOT auto-seeded: T2 drift is computed
        // from balance_snapshots, and until a real brokerage feed (e.g. a
        // Vanguard provider) populates the T2 sleeves, those balances are
        // operator-seeded placeholders — so every breach is an estimate, and a
        // standing rule floods the queue with noise. The kind stays fully
        // supported (eval/render/validate); the operator opts in with
        // `alert.upsert {"kind":"pp.band_breach"}` once the sleeves carry real
        // numbers. (Mirrors balance.low, which is also operator-defined.)
        //
        // Seed the provider-stale tripwire so a silently-failing poller alerts
        // out of the box. Idempotent — keys on `name`.
        if let Err(e) =
            db.upsert_alert_rule_by_name("provider.stale", r#"{"kind":"provider.stale"}"#, "xmpp")
        {
            tracing::warn!(error = %e, "alert: seed provider.stale rule");
        }
        Self { db, cfg }
    }

    pub async fn run(self) {
        let mut tick = interval(Duration::from_secs(self.cfg.check_interval_secs));
        // Skip the immediate first tick — gives the scheduler a moment to do
        // its first poll, so we don't fire alerts based on empty state.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = self.evaluate_once() {
                tracing::warn!(error = %e, "alert: evaluate_once");
            }
        }
    }

    fn evaluate_once(&self) -> Result<()> {
        let rules = self.db.list_active_alert_rules()?;
        for rule in rules {
            let kind = rule_kind(&rule.rule_json);
            match kind.as_deref() {
                Some("pp.band_breach") => self.eval_pp_band_breach(&rule)?,
                Some("provider.stale") => self.eval_provider_stale(&rule)?,
                Some("balance.low") => self.eval_balance_low(&rule)?,
                Some("bandwidth.high") => self.eval_bandwidth_high(&rule)?,
                Some("reminder") => self.eval_reminder(&rule)?,
                Some(other) => {
                    tracing::warn!(rule = %rule.name, kind = other, "alert: unknown rule kind");
                }
                None => {
                    tracing::warn!(rule = %rule.name, "alert: rule_json has no kind");
                }
            }
        }
        Ok(())
    }

    fn eval_pp_band_breach(&self, rule: &AlertRule) -> Result<()> {
        let alloc = allocation(&self.db).context("pp allocation")?;
        let rows = drift(&alloc);
        if !any_band_breach(&rows) {
            return Ok(());
        }
        if self.within_dedup_window(rule.id)? {
            tracing::debug!(rule = %rule.name, "alert: in dedup window, skip");
            return Ok(());
        }
        let payload = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into());
        // Record the breach with delivered=false. The arca-xmpp bridge polls
        // undelivered rows, pushes them to the operator's JID, and marks them
        // delivered. The daemon never delivers directly.
        self.db.insert_alert_history(rule.id, &payload, false)?;
        tracing::info!(rule = %rule.name, "alert: T2 band breach recorded (pending push)");
        Ok(())
    }

    fn eval_provider_stale(&self, rule: &AlertRule) -> Result<()> {
        let max_age =
            rule_i64_field(&rule.rule_json, "max_age_secs").unwrap_or(DEFAULT_STALE_MAX_AGE_SECS);
        let rows = self.db.list_providers().context("list providers")?;
        let stale = find_stale(&rows, now_secs(), max_age);
        if stale.is_empty() {
            return Ok(());
        }
        if self.within_dedup_window(rule.id)? {
            tracing::debug!(rule = %rule.name, "alert: in dedup window, skip");
            return Ok(());
        }
        let payload = serde_json::to_string(&stale).unwrap_or_else(|_| "[]".into());
        self.db.insert_alert_history(rule.id, &payload, false)?;
        tracing::info!(
            rule = %rule.name,
            count = stale.len(),
            "alert: provider(s) stale recorded (pending push)"
        );
        Ok(())
    }

    fn eval_balance_low(&self, rule: &AlertRule) -> Result<()> {
        let v: Value = serde_json::from_str(&rule.rule_json).context("parse balance.low rule")?;
        let (Some(account), Some(min_cents)) = (
            v.get("account").and_then(Value::as_str),
            v.get("min_cents").and_then(Value::as_i64),
        ) else {
            // Shouldn't happen — alert.upsert validates these — but a rule
            // inserted out of band could miss them. Warn, don't fire.
            tracing::warn!(rule = %rule.name, "alert: balance.low missing account/min_cents");
            return Ok(());
        };

        let accounts = self.db.list_active_accounts()?;
        let Some(acct) = accounts.iter().find(|a| a.name == account) else {
            tracing::warn!(rule = %rule.name, account, "alert: balance.low account not found");
            return Ok(());
        };
        let aid = acct.id.expect("active account has id");
        let balances = self.db.latest_balances()?;
        let Some(bal) = balances.iter().find(|(a, _)| *a == aid).map(|(_, c)| *c) else {
            // No snapshot yet — nothing to compare against. Skip quietly; the
            // first refresh will give us a balance to judge.
            return Ok(());
        };
        if bal.as_i64() >= min_cents {
            return Ok(());
        }
        if self.within_dedup_window(rule.id)? {
            tracing::debug!(rule = %rule.name, "alert: in dedup window, skip");
            return Ok(());
        }
        let hit = BalanceLowHit {
            account: account.to_string(),
            balance_cents: bal.as_i64(),
            min_cents,
        };
        let payload = serde_json::to_string(&hit).unwrap_or_else(|_| "{}".into());
        self.db.insert_alert_history(rule.id, &payload, false)?;
        tracing::info!(rule = %rule.name, account, "alert: balance below threshold (pending push)");
        Ok(())
    }

    fn eval_bandwidth_high(&self, rule: &AlertRule) -> Result<()> {
        let v: Value =
            serde_json::from_str(&rule.rule_json).context("parse bandwidth.high rule")?;
        let (Some(account), Some(max_gb)) = (
            v.get("account").and_then(Value::as_str),
            v.get("max_gb").and_then(Value::as_i64),
        ) else {
            // alert.upsert validates these; an out-of-band insert could miss them.
            tracing::warn!(rule = %rule.name, "alert: bandwidth.high missing account/max_gb");
            return Ok(());
        };

        let accounts = self.db.list_active_accounts()?;
        let Some(acct) = accounts.iter().find(|a| a.name == account) else {
            tracing::warn!(rule = %rule.name, account, "alert: bandwidth.high account not found");
            return Ok(());
        };
        let aid = acct.id.expect("active account has id");
        let balances = self.db.latest_balances()?;
        let Some(used_gb) = balances
            .iter()
            .find(|(a, _)| *a == aid)
            .map(|(_, c)| c.as_i64())
        else {
            // No snapshot yet — first refresh will give us a number to judge.
            return Ok(());
        };
        if used_gb <= max_gb {
            return Ok(());
        }
        if self.within_dedup_window(rule.id)? {
            tracing::debug!(rule = %rule.name, "alert: in dedup window, skip");
            return Ok(());
        }
        let hit = BandwidthHighHit {
            account: account.to_string(),
            used_gb,
            max_gb,
        };
        let payload = serde_json::to_string(&hit).unwrap_or_else(|_| "{}".into());
        self.db.insert_alert_history(rule.id, &payload, false)?;
        tracing::info!(rule = %rule.name, account, "alert: bandwidth above threshold (pending push)");
        Ok(())
    }

    /// Time-based reminder: fire on `day_of_month` at/after `hour_ast:minute_ast`
    /// (AST wall-clock), once per month, catching up if no evaluation lands on the
    /// target day (daemon down / tick delayed past midnight). Unlike the predicate
    /// alerts there's no data condition — it's a calendar nudge pushed to XMPP.
    fn eval_reminder(&self, rule: &AlertRule) -> Result<()> {
        let v: Value = serde_json::from_str(&rule.rule_json).context("parse reminder rule")?;
        let (Some(day_of_month), Some(hour_ast), Some(message)) = (
            v.get("day_of_month").and_then(Value::as_u64),
            v.get("hour_ast").and_then(Value::as_u64),
            v.get("message").and_then(Value::as_str),
        ) else {
            // alert.upsert validates these; an out-of-band insert could miss them.
            tracing::warn!(rule = %rule.name, "alert: reminder missing day_of_month/hour_ast/message");
            return Ok(());
        };
        let minute_ast = v.get("minute_ast").and_then(Value::as_u64).unwrap_or(0);

        // Catch-up firing: compute this month's target instant (day clamped to the
        // month's last valid day) and fire once we've reached it AND haven't
        // already fired at/after it this month. Unlike a day-equality gate, a late
        // evaluation — daemon down/restarting across the target, even past midnight
        // into day+1 — still fires this month. The "last fire >= target" check is
        // the per-month dedup (the generic 24h window isn't used for reminders).
        let now_local = Utc::now().with_timezone(&Puerto_Rico);
        let Some(target) = reminder_target_secs(
            now_local,
            day_of_month as u32,
            hour_ast as u32,
            minute_ast as u32,
        ) else {
            tracing::warn!(rule = %rule.name, "alert: reminder target instant invalid; skip");
            return Ok(());
        };
        if !reminder_should_fire(now_secs(), target, self.db.latest_alert_fire(rule.id)?) {
            return Ok(());
        }
        let hit = ReminderHit {
            message: message.to_string(),
            day_of_month: day_of_month as u32,
            hour_ast: hour_ast as u32,
            minute_ast: minute_ast as u32,
        };
        let payload = serde_json::to_string(&hit).unwrap_or_else(|_| "{}".into());
        self.db.insert_alert_history(rule.id, &payload, false)?;
        tracing::info!(rule = %rule.name, "alert: reminder fired (pending push)");
        Ok(())
    }

    fn within_dedup_window(&self, rule_id: i64) -> Result<bool> {
        let Some(last) = self.db.latest_alert_fire(rule_id)? else {
            return Ok(false);
        };
        Ok(now_secs() - last < self.cfg.dedup_window_secs)
    }
}

fn rule_kind(rule_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(rule_json).ok()?;
    v.get("kind").and_then(Value::as_str).map(str::to_string)
}

fn rule_i64_field(rule_json: &str, key: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(rule_json).ok()?;
    v.get(key).and_then(Value::as_i64)
}

/// AST target instant (unix secs) for the reminder in `now`'s calendar month:
/// `day_of_month` at `hour_ast:minute_ast`, in `now`'s timezone. The day is
/// clamped to the month's last valid day, so a reminder set to 29/30/31 still has
/// a target (the 28th/29th/30th) in months that lack that day instead of silently
/// never firing. DB- and clock-free so it unit-tests without fixtures (mirrors
/// [`find_stale`]). `None` only if the clamped local instant is invalid/ambiguous
/// (unreachable for validated inputs).
fn reminder_target_secs<Tz: TimeZone>(
    now: DateTime<Tz>,
    day_of_month: u32,
    hour_ast: u32,
    minute_ast: u32,
) -> Option<i64> {
    let eff_day = day_of_month.min(days_in_month(now.year(), now.month()));
    now.timezone()
        .with_ymd_and_hms(now.year(), now.month(), eff_day, hour_ast, minute_ast, 0)
        .single()
        .map(|dt| dt.timestamp())
}

/// Reminder firing decision: fire iff we've reached this month's `target` instant
/// and haven't already fired at/after it. The "last fire ≥ target" guard yields
/// exactly one fire per month while still letting a late (post-target, even
/// next-day) evaluation catch up. Pure, so it unit-tests without a clock.
fn reminder_should_fire(now: i64, target: i64, last_fire: Option<i64>) -> bool {
    if now < target {
        return false;
    }
    match last_fire {
        Some(last) => last < target,
        None => true,
    }
}

/// Number of days in the given (proleptic Gregorian) month.
fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_this = chrono::NaiveDate::from_ymd_opt(year, month, 1);
    let first_next = chrono::NaiveDate::from_ymd_opt(ny, nm, 1);
    match (first_this, first_next) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        _ => 31, // unreachable for valid month; fail safe to the longest month
    }
}

/// One provider judged stale by [`find_stale`]. Serialized as the alert payload
/// and rendered by [`render_provider_stale`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaleProvider {
    pub kind: String,
    pub label: String,
    pub last_status: Option<String>,
    pub last_poll_at: Option<i64>,
    /// Human-readable cause: `last_status=error` or `no poll in 4d`.
    pub reason: String,
}

/// Grace window before an automatic poller is judged stale, derived from its
/// cadence. `None` means the provider is operator-driven (`manual`/`on_demand`)
/// and never trips the staleness check — the operator drives those by hand and
/// sees failures immediately.
fn cadence_max_age(cadence: &str, default_max: i64) -> Option<i64> {
    match cadence {
        "manual" | "on_demand" => None,
        "hourly" => Some(3 * 3_600),
        "daily" => Some(2 * 86_400),
        "weekly" => Some(9 * 86_400),
        _ => Some(default_max),
    }
}

/// Pure staleness predicate over provider rows (DB-free, so it unit-tests
/// without fixtures). A non-operator-driven provider is stale when:
///   - `last_status` is `error`/`stale` (fails immediately, any cadence), or
///   - it polled successfully once but not within its cadence grace window.
///
/// A never-polled provider (`last_poll_at = None`) is *not* flagged on age
/// alone — that avoids a flood on first boot before the scheduler's first
/// pass. Such a row only trips if its `last_status` is already an error.
pub fn find_stale(rows: &[ProviderRow], now: i64, default_max_age_secs: i64) -> Vec<StaleProvider> {
    let mut out = Vec::new();
    for r in rows {
        let Some(max_age) = cadence_max_age(&r.poll_cadence, default_max_age_secs) else {
            continue;
        };
        let status = r.last_status.as_deref();
        if matches!(status, Some("error" | "stale")) {
            out.push(StaleProvider {
                kind: r.kind.clone(),
                label: r.label.clone(),
                last_status: r.last_status.clone(),
                last_poll_at: r.last_poll_at,
                reason: format!("last_status={}", status.unwrap_or("?")),
            });
            continue;
        }
        if let Some(t) = r.last_poll_at
            && now - t > max_age
        {
            let days = (now - t) / 86_400;
            out.push(StaleProvider {
                kind: r.kind.clone(),
                label: r.label.clone(),
                last_status: r.last_status.clone(),
                last_poll_at: r.last_poll_at,
                reason: format!("no poll in {days}d (grace {}d)", max_age / 86_400),
            });
        }
    }
    out
}

/// Render a human-readable provider-stale summary. Canonical text form of the
/// alert payload — the arca-xmpp bridge and a future `arca alerts pending` read
/// verb can reuse it.
pub fn render_provider_stale(stale: &[StaleProvider]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str("Provider(s) stale or failing.\n");
    out.push_str("=============================\n\n");
    for s in stale {
        let _ = writeln!(out, "{} ({}) — {}", s.label, s.kind, s.reason);
    }
    out.push_str("\nCheck credentials / connectivity; `arca refresh` to retry.\n");
    out
}

/// Alert rule kinds the engine evaluates. A rule whose `kind` isn't here would
/// be recorded but never fire, so `alert.upsert` rejects it up front (honest
/// failure — the operator learns immediately, not via silence).
pub const KNOWN_ALERT_KINDS: &[&str] = &[
    "pp.band_breach",
    "provider.stale",
    "balance.low",
    "bandwidth.high",
    "reminder",
];

/// Validate an `alert.upsert` `rule_json`: it must be a JSON object carrying a
/// `kind` field naming a known predicate, plus any per-kind required params.
/// Returns the kind on success. Honest failure — a rule missing the params it
/// needs to fire is rejected at write time, not stored to silently no-op.
pub fn validate_rule_json(rule_json: &str) -> std::result::Result<String, String> {
    let v: Value =
        serde_json::from_str(rule_json).map_err(|e| format!("rule_json is not valid JSON: {e}"))?;
    let kind = v
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "rule_json missing string field \"kind\"".to_string())?;
    if !KNOWN_ALERT_KINDS.contains(&kind) {
        return Err(format!(
            "unknown alert kind {kind:?}; known: {}",
            KNOWN_ALERT_KINDS.join(", ")
        ));
    }
    if kind == "balance.low" {
        if v.get("account").and_then(Value::as_str).is_none() {
            return Err("balance.low requires a string \"account\"".into());
        }
        if v.get("min_cents").and_then(Value::as_i64).is_none() {
            return Err("balance.low requires an integer \"min_cents\"".into());
        }
    }
    if kind == "bandwidth.high" {
        if v.get("account").and_then(Value::as_str).is_none() {
            return Err("bandwidth.high requires a string \"account\"".into());
        }
        if v.get("max_gb").and_then(Value::as_i64).is_none() {
            return Err("bandwidth.high requires an integer \"max_gb\"".into());
        }
    }
    if kind == "reminder" {
        if !matches!(v.get("day_of_month").and_then(Value::as_u64), Some(1..=31)) {
            return Err("reminder requires an integer \"day_of_month\" in 1..=31".into());
        }
        if !matches!(v.get("hour_ast").and_then(Value::as_u64), Some(0..=23)) {
            return Err("reminder requires an integer \"hour_ast\" in 0..=23".into());
        }
        if let Some(m) = v.get("minute_ast")
            && !matches!(m.as_u64(), Some(0..=59))
        {
            return Err("reminder \"minute_ast\" must be an integer in 0..=59".into());
        }
        match v.get("message").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => {}
            _ => return Err("reminder requires a non-empty string \"message\"".into()),
        }
    }
    Ok(kind.to_string())
}

/// A fired `balance.low` alert. Serialized as the payload, parsed by
/// [`summarize_alert`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BalanceLowHit {
    pub account: String,
    pub balance_cents: i64,
    pub min_cents: i64,
}

/// A fired `bandwidth.high` alert. Serialized as the payload, parsed by
/// [`summarize_alert`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BandwidthHighHit {
    pub account: String,
    pub used_gb: i64,
    pub max_gb: i64,
}

/// A fired `reminder` alert. Serialized as the payload; the bridge and
/// [`summarize_alert`] render `message` (the rest is for context/debugging).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReminderHit {
    pub message: String,
    pub day_of_month: u32,
    pub hour_ast: u32,
    pub minute_ast: u32,
}

/// Compact one-line summary of an alert firing, for the `alert.pending` view.
/// Branches on the rule kind to read its payload shape; an unknown kind falls
/// back to a truncated raw payload.
#[must_use]
pub fn summarize_alert(rule_kind: Option<&str>, payload_json: &str) -> String {
    match rule_kind {
        Some("provider.stale") => match serde_json::from_str::<Vec<StaleProvider>>(payload_json) {
            Ok(v) if !v.is_empty() => {
                let parts: Vec<String> = v
                    .iter()
                    .map(|s| format!("{} ({})", s.label, s.reason))
                    .collect();
                format!("{} provider(s) stale: {}", v.len(), parts.join(", "))
            }
            _ => "provider(s) stale".into(),
        },
        Some("pp.band_breach") => match serde_json::from_str::<Vec<DriftRow>>(payload_json) {
            Ok(rows) => {
                let breached: Vec<String> = rows
                    .iter()
                    .filter(|r| r.band_breach)
                    .map(|r| format!("{} {:.0}%", r.asset_class, r.actual_pct))
                    .collect();
                if breached.is_empty() {
                    "T2 rebalance bands breached".into()
                } else {
                    format!("T2 band breach: {}", breached.join(", "))
                }
            }
            _ => "T2 rebalance bands breached".into(),
        },
        Some("balance.low") => match serde_json::from_str::<BalanceLowHit>(payload_json) {
            Ok(h) => format!(
                "{} low: {} (min {})",
                h.account,
                Cents(h.balance_cents),
                Cents(h.min_cents)
            ),
            _ => "account balance low".into(),
        },
        Some("bandwidth.high") => match serde_json::from_str::<BandwidthHighHit>(payload_json) {
            Ok(h) => format!("{} high: {} GB (max {} GB)", h.account, h.used_gb, h.max_gb),
            _ => "bandwidth high".into(),
        },
        Some("reminder") => match serde_json::from_str::<ReminderHit>(payload_json) {
            Ok(h) => format!("⏰ {}", h.message),
            _ => "reminder".into(),
        },
        _ => {
            let trimmed = payload_json.trim();
            if trimmed.chars().count() > 120 {
                let cut: String = trimmed.chars().take(117).collect();
                format!("{cut}...")
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// Render a human-readable band-breach summary. Kept as the canonical text
/// form of the alert payload — a future `arca alerts pending` read verb and the
/// arca-xmpp bridge can reuse it.
pub fn render_pp_band_breach(alloc: &Allocation, rows: &[DriftRow]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str("T2 rebalance bands breached (22/44).\n");
    out.push_str("================================\n\n");
    let _ = writeln!(out, "T2 total: {}", alloc.total);
    out.push('\n');
    let _ = writeln!(
        out,
        "{:<18} {:>14} {:>8} {:>8} {:>10} {:>10}",
        "sleeve", "actual", "%", "target", "drift_pp", "status"
    );
    out.push_str(&"-".repeat(72));
    out.push('\n');
    for r in rows {
        let status = if r.band_breach {
            "BREACH"
        } else if r.upper_band_pct > 0.0 {
            "ok"
        } else {
            "-"
        };
        let _ = writeln!(
            out,
            "{:<18} {:>14} {:>7.2}% {:>7.2}% {:>+9.2} {:>10}",
            r.asset_class,
            r.actual_cents.to_string(),
            r.actual_pct,
            r.target_pct,
            r.drift_pp,
            status,
        );
    }
    let _ = writeln!(
        out,
        "\nBands: lower={:.0}%  upper={:.0}%",
        arca_core::pp::T2_LOWER_BAND_PCT,
        arca_core::pp::T2_UPPER_BAND_PCT,
    );
    out.push_str("\nRebalance via new contributions where possible (Rowland).\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::db::Account;
    use arca_core::ids::ProviderId;
    use arca_core::money::Cents;
    use arca_core::pp::drift;

    fn prow(
        kind: &str,
        cadence: &str,
        last_poll_at: Option<i64>,
        last_status: Option<&str>,
    ) -> ProviderRow {
        ProviderRow {
            id: ProviderId(0),
            kind: kind.into(),
            label: kind.into(),
            config_json: "{}".into(),
            secret_ref: None,
            poll_cadence: cadence.into(),
            last_poll_at,
            last_status: last_status.map(str::to_string),
        }
    }

    const NOW: i64 = 1_900_000_000;

    #[test]
    fn stale_flags_error_status_any_cadence() {
        let rows = [prow("plaid", "daily", Some(NOW - 10), Some("error"))];
        let stale = find_stale(&rows, NOW, DEFAULT_STALE_MAX_AGE_SECS);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].reason.contains("last_status=error"));
    }

    #[test]
    fn stale_flags_aged_daily_poller() {
        // Polled OK 5 days ago; daily grace is 2 days → stale on age.
        let rows = [prow("mercury", "daily", Some(NOW - 5 * 86_400), Some("ok"))];
        let stale = find_stale(&rows, NOW, DEFAULT_STALE_MAX_AGE_SECS);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].reason.contains("no poll in 5d"));
    }

    #[test]
    fn stale_ignores_fresh_poller() {
        let rows = [prow("mercury", "daily", Some(NOW - 3_600), Some("ok"))];
        assert!(find_stale(&rows, NOW, DEFAULT_STALE_MAX_AGE_SECS).is_empty());
    }

    #[test]
    fn stale_ignores_manual_cadence() {
        // Operator-driven; never trips on age, even if last poll errored long ago.
        let rows = [prow(
            "manual",
            "manual",
            Some(NOW - 99 * 86_400),
            Some("ok"),
        )];
        assert!(find_stale(&rows, NOW, DEFAULT_STALE_MAX_AGE_SECS).is_empty());
    }

    #[test]
    fn stale_ignores_never_polled_without_error() {
        // Fresh boot: cadence set, never polled, no error yet → no false flood.
        let rows = [prow("stripe", "daily", None, None)];
        assert!(find_stale(&rows, NOW, DEFAULT_STALE_MAX_AGE_SECS).is_empty());
    }

    #[test]
    fn stale_flags_never_polled_with_error() {
        let rows = [prow("stripe", "daily", None, Some("error"))];
        assert_eq!(find_stale(&rows, NOW, DEFAULT_STALE_MAX_AGE_SECS).len(), 1);
    }

    #[test]
    fn weekly_grace_wider_than_daily() {
        // 5 days silent: stale for a daily poller, fine for a weekly one.
        let daily = [prow("a", "daily", Some(NOW - 5 * 86_400), Some("ok"))];
        let weekly = [prow("b", "weekly", Some(NOW - 5 * 86_400), Some("ok"))];
        assert_eq!(find_stale(&daily, NOW, DEFAULT_STALE_MAX_AGE_SECS).len(), 1);
        assert!(find_stale(&weekly, NOW, DEFAULT_STALE_MAX_AGE_SECS).is_empty());
    }

    #[test]
    fn validate_rule_json_accepts_known_kinds() {
        assert_eq!(
            validate_rule_json(r#"{"kind":"provider.stale"}"#).unwrap(),
            "provider.stale"
        );
        assert_eq!(
            validate_rule_json(r#"{"kind":"pp.band_breach"}"#).unwrap(),
            "pp.band_breach"
        );
    }

    #[test]
    fn validate_rule_json_rejects_garbage_and_unknown() {
        assert!(validate_rule_json("not json").is_err());
        assert!(validate_rule_json(r#"{"nope":1}"#).is_err());
        let e = validate_rule_json(r#"{"kind":"totally.bogus"}"#).unwrap_err();
        assert!(e.contains("unknown alert kind"), "got: {e}");
    }

    #[test]
    fn summarize_provider_stale_lists_labels() {
        let payload = serde_json::to_string(&vec![StaleProvider {
            kind: "plaid".into(),
            label: "plaid".into(),
            last_status: Some("error".into()),
            last_poll_at: None,
            reason: "last_status=error".into(),
        }])
        .unwrap();
        let s = summarize_alert(Some("provider.stale"), &payload);
        assert!(s.contains("1 provider(s) stale"), "got: {s}");
        assert!(s.contains("plaid"));
    }

    #[test]
    fn summarize_unknown_kind_truncates_payload() {
        let s = summarize_alert(None, "  {\"x\":1}  ");
        assert_eq!(s, "{\"x\":1}");
    }

    #[test]
    fn validate_balance_low_requires_account_and_min() {
        assert!(validate_rule_json(r#"{"kind":"balance.low","min_cents":100}"#).is_err());
        assert!(validate_rule_json(r#"{"kind":"balance.low","account":"x"}"#).is_err());
        assert_eq!(
            validate_rule_json(r#"{"kind":"balance.low","account":"x","min_cents":100}"#).unwrap(),
            "balance.low"
        );
    }

    #[test]
    fn summarize_balance_low_formats_money() {
        let payload = serde_json::to_string(&BalanceLowHit {
            account: "First Bank Checking".into(),
            balance_cents: 12_345,
            min_cents: 50_000,
        })
        .unwrap();
        let s = summarize_alert(Some("balance.low"), &payload);
        assert!(s.contains("First Bank Checking"), "got: {s}");
        assert!(s.contains("$123.45"), "got: {s}");
        assert!(s.contains("$500.00"), "got: {s}");
    }

    fn checking(name: &str) -> Account {
        Account {
            id: None,
            name: name.into(),
            kind: "asset".into(),
            asset_class: Some("cash".into()),
            tier: None,
            currency: "USD".into(),
            provider_id: None,
            business_id: None,
            external_id: None,
            active: true,
        }
    }

    #[test]
    fn balance_low_fires_below_threshold_only() {
        let db = Arc::new(Db::open_memory().unwrap());
        let aid = db.upsert_account(&checking("Checking")).unwrap();
        db.set_alert_rule(
            "checking.low",
            r#"{"kind":"balance.low","account":"Checking","min_cents":50000}"#,
            "xmpp",
            true,
        )
        .unwrap();
        let engine = AlertEngine::new(Arc::clone(&db), AlertsCfg::default());
        let rule_id = db
            .list_active_alert_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "checking.low")
            .unwrap()
            .id;

        // Above threshold → no fire.
        db.insert_snapshot(aid, Cents(600_00), "manual").unwrap();
        engine.evaluate_once().unwrap();
        assert!(db.latest_alert_fire(rule_id).unwrap().is_none());

        // Drop below → fires.
        db.insert_snapshot(aid, Cents(100_00), "manual").unwrap();
        engine.evaluate_once().unwrap();
        assert!(db.latest_alert_fire(rule_id).unwrap().is_some());
    }

    #[test]
    fn validate_bandwidth_high_requires_account_and_max() {
        assert!(validate_rule_json(r#"{"kind":"bandwidth.high","max_gb":1600}"#).is_err());
        assert!(validate_rule_json(r#"{"kind":"bandwidth.high","account":"x"}"#).is_err());
        assert_eq!(
            validate_rule_json(r#"{"kind":"bandwidth.high","account":"x","max_gb":1600}"#).unwrap(),
            "bandwidth.high"
        );
    }

    #[test]
    fn summarize_bandwidth_high_formats_gb() {
        let payload = serde_json::to_string(&BandwidthHighHit {
            account: "Vultr — web1 (egress GB)".into(),
            used_gb: 1700,
            max_gb: 1600,
        })
        .unwrap();
        let s = summarize_alert(Some("bandwidth.high"), &payload);
        assert!(s.contains("Vultr — web1"), "got: {s}");
        assert!(s.contains("1700 GB"), "got: {s}");
        assert!(s.contains("max 1600 GB"), "got: {s}");
    }

    #[test]
    fn bandwidth_high_fires_above_threshold_only() {
        let db = Arc::new(Db::open_memory().unwrap());
        // The egress account; eval keys on name + latest snapshot, not currency.
        let aid = db.upsert_account(&checking("Vultr egress")).unwrap();
        db.set_alert_rule(
            "web1.bandwidth",
            r#"{"kind":"bandwidth.high","account":"Vultr egress","max_gb":1600}"#,
            "xmpp",
            true,
        )
        .unwrap();
        let engine = AlertEngine::new(Arc::clone(&db), AlertsCfg::default());
        let rule_id = db
            .list_active_alert_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "web1.bandwidth")
            .unwrap()
            .id;

        // Under threshold → no fire (snapshot value is GB, carried in Cents).
        db.insert_snapshot(aid, Cents(1200), "vultr").unwrap();
        engine.evaluate_once().unwrap();
        assert!(db.latest_alert_fire(rule_id).unwrap().is_none());

        // Over threshold → fires.
        db.insert_snapshot(aid, Cents(1700), "vultr").unwrap();
        engine.evaluate_once().unwrap();
        assert!(db.latest_alert_fire(rule_id).unwrap().is_some());
    }

    #[test]
    fn engine_seeds_and_fires_provider_stale() {
        let db = Arc::new(Db::open_memory().unwrap());
        db.upsert_provider(&prow("plaid", "daily", None, None))
            .unwrap();
        // Force the row into an error state.
        let pid = db
            .list_providers()
            .unwrap()
            .into_iter()
            .find(|r| r.kind == "plaid")
            .unwrap()
            .id;
        db.record_poll(pid, "error").unwrap();

        let engine = AlertEngine::new(Arc::clone(&db), AlertsCfg::default());
        engine.evaluate_once().unwrap();

        let rule = db
            .list_active_alert_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "provider.stale")
            .unwrap();
        assert!(
            db.latest_alert_fire(rule.id).unwrap().is_some(),
            "provider.stale should fire on an errored poller"
        );
    }

    fn t2(name: &str, ac: &str) -> Account {
        Account {
            id: None,
            name: name.into(),
            kind: "brokerage".into(),
            asset_class: Some(ac.into()),
            tier: Some("t2".into()),
            currency: "USD".into(),
            provider_id: None,
            business_id: None,
            external_id: None,
            active: true,
        }
    }

    #[test]
    fn render_includes_breach_row_and_bands() {
        let db = Db::open_memory().unwrap();
        let e = db.upsert_account(&t2("E", "equity")).unwrap();
        let b = db.upsert_account(&t2("B", "long_treasuries")).unwrap();
        let c = db.upsert_account(&t2("C", "cash")).unwrap();
        db.insert_snapshot(e, Cents(60_000_00), "manual").unwrap();
        db.insert_snapshot(b, Cents(30_000_00), "manual").unwrap();
        db.insert_snapshot(c, Cents(10_000_00), "manual").unwrap();
        let alloc = arca_core::pp::allocation(&db).unwrap();
        let rows = drift(&alloc);
        let body = render_pp_band_breach(&alloc, &rows);
        assert!(
            body.contains("BREACH"),
            "body missing BREACH marker: {body}"
        );
        assert!(body.contains("Bands: lower=22%  upper=44%"));
        assert!(body.contains("equity"));
        assert!(body.contains("cash"));
    }

    #[test]
    fn engine_skips_when_no_breach() {
        let db = Arc::new(Db::open_memory().unwrap());
        // Even split → no breach. Engine should fire nothing.
        for (n, ac) in [("E", "equity"), ("B", "long_treasuries"), ("C", "cash")] {
            let id = db.upsert_account(&t2(n, ac)).unwrap();
            db.insert_snapshot(id, Cents(10_000_00), "manual").unwrap();
        }
        // pp.band_breach is opt-in now (not auto-seeded); add it for the test.
        db.upsert_alert_rule_by_name("pp.band_breach", r#"{"kind":"pp.band_breach"}"#, "xmpp")
            .unwrap();
        let engine = AlertEngine::new(Arc::clone(&db), AlertsCfg::default());
        engine.evaluate_once().unwrap();
        let rule = db
            .list_active_alert_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "pp.band_breach")
            .unwrap();
        assert!(db.latest_alert_fire(rule.id).unwrap().is_none());
    }

    #[test]
    fn engine_records_history_on_breach() {
        let db = Arc::new(Db::open_memory().unwrap());
        // Skewed split → breach.
        let e = db.upsert_account(&t2("E", "equity")).unwrap();
        let b = db.upsert_account(&t2("B", "long_treasuries")).unwrap();
        let c = db.upsert_account(&t2("C", "cash")).unwrap();
        db.insert_snapshot(e, Cents(60_000_00), "manual").unwrap();
        db.insert_snapshot(b, Cents(30_000_00), "manual").unwrap();
        db.insert_snapshot(c, Cents(10_000_00), "manual").unwrap();
        // No delivery happens in-process; the breach is recorded with
        // delivered=false so dedup works and the bridge can pick it up.
        db.upsert_alert_rule_by_name("pp.band_breach", r#"{"kind":"pp.band_breach"}"#, "xmpp")
            .unwrap();
        let cfg = AlertsCfg::default();
        let engine = AlertEngine::new(Arc::clone(&db), cfg);
        engine.evaluate_once().unwrap();
        let rule = db
            .list_active_alert_rules()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "pp.band_breach")
            .unwrap();
        assert!(
            db.latest_alert_fire(rule.id).unwrap().is_some(),
            "history row should be written even when delivery fails"
        );
    }

    #[test]
    fn engine_respects_dedup_window() {
        let db = Arc::new(Db::open_memory().unwrap());
        let e = db.upsert_account(&t2("E", "equity")).unwrap();
        let b = db.upsert_account(&t2("B", "long_treasuries")).unwrap();
        let c = db.upsert_account(&t2("C", "cash")).unwrap();
        db.insert_snapshot(e, Cents(60_000_00), "manual").unwrap();
        db.insert_snapshot(b, Cents(30_000_00), "manual").unwrap();
        db.insert_snapshot(c, Cents(10_000_00), "manual").unwrap();
        db.upsert_alert_rule_by_name("pp.band_breach", r#"{"kind":"pp.band_breach"}"#, "xmpp")
            .unwrap();
        let engine = AlertEngine::new(Arc::clone(&db), AlertsCfg::default());
        engine.evaluate_once().unwrap();
        engine.evaluate_once().unwrap();
        // Two fires within same window → only one history row.
        let count: i64 = db
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM alert_history", [], |r| r.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(count, 1, "dedup window should suppress second fire");
    }

    #[test]
    fn reminder_target_is_clamped_month_instant() {
        let inst = |y, m, d, h, mi| {
            Puerto_Rico
                .with_ymd_and_hms(y, m, d, h, mi, 0)
                .unwrap()
                .timestamp()
        };
        // An existing day: target is that day at the given time, in `now`'s month.
        let now = Puerto_Rico.with_ymd_and_hms(2026, 5, 3, 8, 0, 0).unwrap();
        assert_eq!(
            reminder_target_secs(now, 15, 15, 0).unwrap(),
            inst(2026, 5, 15, 15, 0)
        );
        // day 31 in 30-day April clamps to the 30th (last-day-of-month).
        let apr = Puerto_Rico.with_ymd_and_hms(2026, 4, 10, 0, 0, 0).unwrap();
        assert_eq!(
            reminder_target_secs(apr, 31, 9, 0).unwrap(),
            inst(2026, 4, 30, 9, 0)
        );
        // day 30 in non-leap Feb → 28th; day 31 in leap Feb → 29th.
        let feb = Puerto_Rico.with_ymd_and_hms(2026, 2, 5, 0, 0, 0).unwrap();
        assert_eq!(
            reminder_target_secs(feb, 30, 9, 0).unwrap(),
            inst(2026, 2, 28, 9, 0)
        );
        let feb24 = Puerto_Rico.with_ymd_and_hms(2024, 2, 5, 0, 0, 0).unwrap();
        assert_eq!(
            reminder_target_secs(feb24, 31, 9, 0).unwrap(),
            inst(2024, 2, 29, 9, 0)
        );
    }

    #[test]
    fn reminder_fires_once_per_month_with_catch_up() {
        let target = Puerto_Rico
            .with_ymd_and_hms(2026, 5, 1, 10, 30, 0)
            .unwrap()
            .timestamp();
        // Not yet reached → no fire.
        assert!(!reminder_should_fire(target - 60, target, None));
        // Reached, not fired this month → fire.
        assert!(reminder_should_fire(target, target, None));
        // Catch-up: the first evaluation lands the next day (daemon was down) and
        // still fires this month — the old day-equality gate dropped it entirely.
        assert!(reminder_should_fire(target + 30 * 3600, target, None));
        // Already fired at/after the target → no second fire this month.
        assert!(!reminder_should_fire(
            target + 5 * 3600,
            target,
            Some(target)
        ));
        assert!(!reminder_should_fire(
            target + 30 * 3600,
            target,
            Some(target + 60)
        ));
        // A fire from last month doesn't block this month's target.
        let last_month = Puerto_Rico
            .with_ymd_and_hms(2026, 4, 1, 10, 30, 0)
            .unwrap()
            .timestamp();
        assert!(reminder_should_fire(target, target, Some(last_month)));
    }

    #[test]
    fn validate_reminder_requires_fields() {
        // Missing required fields.
        assert!(validate_rule_json(r#"{"kind":"reminder","hour_ast":15,"message":"x"}"#).is_err());
        assert!(
            validate_rule_json(r#"{"kind":"reminder","day_of_month":15,"message":"x"}"#).is_err()
        );
        assert!(
            validate_rule_json(r#"{"kind":"reminder","day_of_month":15,"hour_ast":15}"#).is_err()
        );
        // Out-of-range day / hour, blank message.
        assert!(
            validate_rule_json(
                r#"{"kind":"reminder","day_of_month":0,"hour_ast":15,"message":"x"}"#
            )
            .is_err()
        );
        assert!(
            validate_rule_json(
                r#"{"kind":"reminder","day_of_month":15,"hour_ast":24,"message":"x"}"#
            )
            .is_err()
        );
        assert!(
            validate_rule_json(
                r#"{"kind":"reminder","day_of_month":15,"hour_ast":15,"message":"   "}"#
            )
            .is_err()
        );
        // Valid.
        assert_eq!(
            validate_rule_json(
                r#"{"kind":"reminder","day_of_month":15,"hour_ast":15,"minute_ast":0,"message":"Pay the bills"}"#
            )
            .unwrap(),
            "reminder"
        );
    }

    #[test]
    fn summarize_reminder_shows_message() {
        let payload = serde_json::to_string(&ReminderHit {
            message: "Pay rent".into(),
            day_of_month: 1,
            hour_ast: 10,
            minute_ast: 30,
        })
        .unwrap();
        let s = summarize_alert(Some("reminder"), &payload);
        assert!(s.contains("Pay rent"), "got: {s}");
    }
}
