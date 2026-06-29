//! Monthly Markdown report generation.
//!
//! Pure data + rendering — no I/O, no network, no delivery. The daemon's
//! `reports` engine drives this on the 1st of each month against the prior
//! calendar month (interpreted in `America/Puerto_Rico`).
//!
//! Sections (per the design spec):
//!   1. Net worth delta — current vs balance snapshot nearest the start of the
//!      reporting month (any account, any kind).
//!   2. Cash flow by business — income / expenses / net by `businesses.tag`.
//!   3. Investment performance — T2 PP allocation movement.
//!   4. PP drift table — current T2 drift (33×3 with 22/44 bands).
//!   5. Debt paydown — sum of negative transactions against `accounts.kind='debt'`.
//!   6. Top expense categories — top N negative-amount categories.
//!   7. API usage cost table — active subscriptions with their latest amount.
//!
//! "Nearest" snapshots use `<= window_start` so a missing-snapshot situation
//! degrades to "no prior data" instead of pulling from the wrong side.

use std::fmt::Write as _;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::America::Puerto_Rico;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;
use crate::money::Cents;
use crate::pp::{Allocation, Backbone, DriftRow, allocation, backbone, drift};

/// Inclusive-exclusive window in unix seconds UTC and the human label
/// (e.g. "2026-04") for the month being reported on.
#[derive(Clone, Copy, Debug)]
pub struct MonthWindow {
    pub label_year: i32,
    pub label_month: u32,
    pub since: i64,
    pub until: i64,
}

impl MonthWindow {
    /// Window for the calendar month immediately before `as_of`, interpreted
    /// in America/Puerto_Rico. Boundaries are AST midnight converted to UTC.
    #[must_use]
    pub fn prior_month_ast(as_of: DateTime<Utc>) -> Self {
        let local = as_of.with_timezone(&Puerto_Rico);
        let (py, pm) = if local.month() == 1 {
            (local.year() - 1, 12)
        } else {
            (local.year(), local.month() - 1)
        };
        let (ny, nm) = (local.year(), local.month());
        let start_local = Puerto_Rico
            .with_ymd_and_hms(py, pm, 1, 0, 0, 0)
            .single()
            .expect("AST month start is unambiguous");
        let end_local = Puerto_Rico
            .with_ymd_and_hms(ny, nm, 1, 0, 0, 0)
            .single()
            .expect("AST month start is unambiguous");
        Self {
            label_year: py,
            label_month: pm,
            since: start_local.with_timezone(&Utc).timestamp(),
            until: end_local.with_timezone(&Utc).timestamp(),
        }
    }

    #[must_use]
    pub fn label(&self) -> String {
        format!("{:04}-{:02}", self.label_year, self.label_month)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonthlyReport {
    pub label: String,
    pub since: i64,
    pub until: i64,
    /// Sum of `latest_balances()` at report time, ex debts (debts subtract).
    pub net_worth_now: Cents,
    /// Same sum computed from the latest snapshot per account that is `<= since`.
    /// `None` when no eligible snapshot exists for any account.
    pub net_worth_prior: Option<Cents>,
    pub businesses: Vec<BusinessFlow>,
    pub pp_allocation: Allocation,
    pub pp_drift: Vec<DriftRow>,
    pub backbone: Backbone,
    pub debt_paydown: Cents,
    pub top_expenses: Vec<CategorySpend>,
    pub api_usage: Vec<ApiUsageRow>,
    /// Operator-confirmed recurring series (sub/bill/debt), sorted soonest-due.
    pub recurring: Vec<RecurringReportRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusinessFlow {
    pub tag: String,
    pub display_name: String,
    pub income: Cents,
    pub expenses: Cents,
    pub net: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CategorySpend {
    pub category: String,
    /// Always non-positive — these are expenses.
    pub amount: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiUsageRow {
    pub name: String,
    pub provider_kind: String,
    pub amount: Option<Cents>,
    pub cadence: Option<String>,
}

/// A confirmed recurring series row for the report. Amounts are observed history,
/// not a claim about the next charge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecurringReportRow {
    pub name: String,
    /// `sub` | `bill` | `debt`.
    pub label: String,
    pub cadence: String,
    pub last_amount: Cents,
    pub avg_amount: Cents,
    pub predicted_next: i64,
}

/// Build a `MonthlyReport` for the given window.
pub fn build_monthly_report(db: &Db, w: MonthWindow) -> Result<MonthlyReport> {
    let net_worth_now = current_net_worth(db)?;
    let net_worth_prior = prior_net_worth(db, w.since)?;
    let businesses = business_flows(db, w.since, w.until)?;
    let alloc = allocation(db)?;
    let drift_rows = drift(&alloc);
    let bb = backbone(db)?;
    let debt_paydown = debt_paydown(db, w.since, w.until)?;
    let top_expenses = db
        .expenses_by_category(w.since, w.until, 10)?
        .into_iter()
        .map(|(category, amount)| CategorySpend { category, amount })
        .collect();
    let api_usage = api_usage_rows(db)?;
    let recurring = recurring_rows(db)?;
    Ok(MonthlyReport {
        label: w.label(),
        since: w.since,
        until: w.until,
        net_worth_now,
        net_worth_prior,
        businesses,
        pp_allocation: alloc,
        pp_drift: drift_rows,
        backbone: bb,
        debt_paydown,
        top_expenses,
        api_usage,
        recurring,
    })
}

/// Confirmed recurring series, soonest-due first. Unlabeled detected series are
/// excluded — the report shows what the operator has actually confirmed.
fn recurring_rows(db: &Db) -> Result<Vec<RecurringReportRow>> {
    let mut rows: Vec<RecurringReportRow> =
        crate::recurring::labeled_series(db, None, crate::recurring::MIN_OCCURRENCES)?
            .into_iter()
            .filter_map(|ls| {
                let label = ls.label?;
                let s = ls.series;
                Some(RecurringReportRow {
                    name: ls.display_name.unwrap_or(s.display),
                    label,
                    cadence: s.cadence.as_str().to_string(),
                    last_amount: s.last_amount,
                    avg_amount: s.avg_amount,
                    predicted_next: s.predicted_next,
                })
            })
            .collect();
    rows.sort_by_key(|r| r.predicted_next);
    Ok(rows)
}

fn current_net_worth(db: &Db) -> Result<Cents> {
    let accounts = db.list_active_accounts()?;
    let balances = db.latest_balances()?;
    let mut net = Cents::ZERO;
    for a in &accounts {
        let Some(aid) = a.id else { continue };
        let bal = balances
            .iter()
            .find(|(b, _)| *b == aid)
            .map(|(_, c)| *c)
            .unwrap_or(Cents::ZERO);
        if a.kind == "subscription" || a.currency != "USD" {
            continue; // non-USD excluded: no FX conversion into a USD total (v1)
        }
        if a.kind == "debt" {
            net -= bal;
        } else {
            net += bal;
        }
    }
    Ok(net)
}

fn prior_net_worth(db: &Db, before: i64) -> Result<Option<Cents>> {
    db.with_conn(|conn| {
        // Latest snapshot per account taken_at <= before.
        let mut stmt = conn.prepare(
            "SELECT a.kind, b.amount_cents
               FROM accounts a
               JOIN (
                 SELECT account_id, amount_cents,
                        ROW_NUMBER() OVER (
                          PARTITION BY account_id
                          ORDER BY taken_at DESC, id DESC
                        ) AS rn
                   FROM balance_snapshots
                  WHERE taken_at <= ?1
               ) b ON b.account_id = a.id AND b.rn = 1
              WHERE a.active = 1 AND a.kind != 'subscription' AND a.currency = 'USD'",
        )?;
        let mut net = Cents::ZERO;
        let mut any = false;
        let rows = stmt.query_map(params![before], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (kind, amt) = row?;
            any = true;
            if kind == "debt" {
                net -= Cents(amt);
            } else {
                net += Cents(amt);
            }
        }
        Ok(if any { Some(net) } else { None })
    })
}

fn business_flows(db: &Db, since: i64, until: i64) -> Result<Vec<BusinessFlow>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, tag, display_name FROM businesses WHERE active = 1 ORDER BY tag",
        )?;
        let bizes: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut out = Vec::with_capacity(bizes.len());
        for (id, tag, name) in bizes {
            let (income, expenses): (i64, i64) = conn.query_row(
                "SELECT COALESCE(SUM(CASE WHEN amount_cents > 0 THEN amount_cents ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN amount_cents < 0 THEN amount_cents ELSE 0 END), 0)
                   FROM transactions
                  WHERE business_id = ?1 AND posted_at >= ?2 AND posted_at < ?3",
                params![id, since, until],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let income = Cents(income);
            let expenses = Cents(expenses);
            out.push(BusinessFlow {
                tag,
                display_name: name,
                income,
                expenses,
                net: income + expenses,
            });
        }
        Ok(out)
    })
}

fn debt_paydown(db: &Db, since: i64, until: i64) -> Result<Cents> {
    db.with_conn(|conn| {
        // Negative amounts against a debt account = principal paydown
        // (positive number reported). Positive amounts on debt accounts (charges
        // or new draws) reduce the paydown figure.
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(-t.amount_cents), 0)
               FROM transactions t
               JOIN accounts a ON a.id = t.account_id
              WHERE a.kind = 'debt' AND t.posted_at >= ?1 AND t.posted_at < ?2",
            params![since, until],
            |r| r.get(0),
        )?;
        Ok(Cents(sum))
    })
}

fn api_usage_rows(db: &Db) -> Result<Vec<ApiUsageRow>> {
    let subs = db.list_active_subscriptions()?;
    let mut rows: Vec<ApiUsageRow> = subs
        .into_iter()
        .map(|s| ApiUsageRow {
            name: s.name,
            provider_kind: s.provider_kind,
            amount: s.amount_cents,
            cadence: s.cadence,
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

/// Render the monthly report to a Markdown string. Stable enough that the
/// daemon can write it to `/var/arca/reports/YYYY-MM.md` for the arca-xmpp
/// bridge to push without further processing.
#[must_use]
pub fn render_markdown(r: &MonthlyReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# arca monthly report — {}", r.label);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Window: {} → {} (AST midnight boundaries).",
        ast_date(r.since),
        ast_date(r.until)
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## Net worth");
    let _ = writeln!(out);
    let _ = writeln!(out, "| | amount |");
    let _ = writeln!(out, "|---|---:|");
    let _ = writeln!(out, "| Now | {} |", r.net_worth_now);
    match r.net_worth_prior {
        Some(p) => {
            let _ = writeln!(out, "| Start of {} | {} |", r.label, p);
            let _ = writeln!(out, "| Delta | {} |", r.net_worth_now - p);
        }
        None => {
            let _ = writeln!(
                out,
                "| Start of {} | _no snapshot before window start_ |",
                r.label
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Cash flow by business");
    let _ = writeln!(out);
    if r.businesses.is_empty() {
        let _ = writeln!(out, "_No businesses configured._");
    } else {
        let _ = writeln!(out, "| Business | Income | Expenses | Net |");
        let _ = writeln!(out, "|---|---:|---:|---:|");
        for b in &r.businesses {
            let _ = writeln!(
                out,
                "| {} ({}) | {} | {} | {} |",
                b.display_name, b.tag, b.income, b.expenses, b.net
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Investment performance (Tier 2)");
    let _ = writeln!(out);
    let _ = writeln!(out, "T2 total: **{}**", r.pp_allocation.total);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Permanent portfolio drift (33×3, 22/44 bands)");
    let _ = writeln!(out);
    if r.pp_drift.is_empty() || r.pp_allocation.total.0 == 0 {
        let _ = writeln!(out, "_No Tier-2 balances recorded._");
    } else {
        let _ = writeln!(
            out,
            "| Sleeve | Actual | % | Target % | Drift pp | Band | Status |"
        );
        let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|:--|");
        for row in &r.pp_drift {
            let band = if row.upper_band_pct > 0.0 {
                format!("{:.0}–{:.0}%", row.lower_band_pct, row.upper_band_pct)
            } else {
                "—".into()
            };
            let status = if row.band_breach {
                "**BREACH**"
            } else if row.upper_band_pct > 0.0 {
                "ok"
            } else {
                "—"
            };
            let _ = writeln!(
                out,
                "| {} | {} | {:.2}% | {:.2}% | {:+.2} | {} | {} |",
                row.asset_class,
                row.actual_cents,
                row.actual_pct,
                row.target_pct,
                row.drift_pp,
                band,
                status
            );
        }
    }
    let _ = writeln!(out);

    let bb = &r.backbone;
    let _ = writeln!(out, "## Tier 1 backbone (hold-forever, info only)");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Asset | Latest |");
    let _ = writeln!(out, "|---|---:|");
    let _ = writeln!(out, "| Gold | {} |", bb.gold);
    let _ = writeln!(out, "| Silver | {} |", bb.silver);
    let _ = writeln!(out, "| XMR | {} |", bb.xmr);
    let _ = writeln!(out, "| Land | {} |", bb.land);
    let _ = writeln!(out, "| SFR | {} |", bb.sfr);
    if bb.other.0 != 0 {
        let _ = writeln!(out, "| Other | {} |", bb.other);
    }
    let _ = writeln!(out, "| **Total** | **{}** |", bb.total);
    let _ = writeln!(out);

    let _ = writeln!(out, "## Debt paydown");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Principal reduced in {}: **{}** (positive = paid down).",
        r.label, r.debt_paydown
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## Top expense categories");
    let _ = writeln!(out);
    if r.top_expenses.is_empty() {
        let _ = writeln!(out, "_No expense rows in window._");
    } else {
        let _ = writeln!(out, "| Category | Spend |");
        let _ = writeln!(out, "|---|---:|");
        for c in &r.top_expenses {
            let _ = writeln!(out, "| {} | {} |", c.category, c.amount);
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## API & subscriptions");
    let _ = writeln!(out);
    if r.api_usage.is_empty() {
        let _ = writeln!(out, "_None active._");
    } else {
        let _ = writeln!(out, "| Name | Kind | Latest | Cadence |");
        let _ = writeln!(out, "|---|---|---:|---|");
        for u in &r.api_usage {
            let amt = u
                .amount
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".into());
            let cad = u.cadence.clone().unwrap_or_else(|| "—".into());
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                u.name, u.provider_kind, amt, cad
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Recurring (confirmed)");
    let _ = writeln!(out);
    if r.recurring.is_empty() {
        let _ = writeln!(out, "_No confirmed recurring series._");
    } else {
        let _ = writeln!(
            out,
            "_Amounts are observed history; next charge is an estimate._"
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "| Name | Label | Cadence | Last | Avg | Next (est) |");
        let _ = writeln!(out, "|---|---|---|---:|---:|---|");
        for s in &r.recurring {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                s.name,
                s.label,
                s.cadence,
                s.last_amount,
                s.avg_amount,
                ast_date(s.predicted_next)
            );
        }
    }
    out
}

fn ast_date(ts_secs: i64) -> String {
    Utc.timestamp_opt(ts_secs, 0)
        .single()
        .map(|d| d.with_timezone(&Puerto_Rico).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| format!("invalid_ts({ts_secs})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Account, Transaction};
    use crate::ids::AccountId;

    fn acct(name: &str, kind: &str, ac: Option<&str>, tier: Option<&str>) -> Account {
        Account {
            id: None,
            name: name.into(),
            kind: kind.into(),
            asset_class: ac.map(str::to_string),
            tier: tier.map(str::to_string),
            currency: "USD".into(),
            provider_id: None,
            business_id: None,
            external_id: None,
            active: true,
        }
    }

    fn tx(aid: AccountId, posted_at: i64, amt: i64, ext: &str) -> Transaction {
        Transaction {
            id: None,
            account_id: aid,
            posted_at,
            amount_cents: Cents(amt),
            currency: "USD".into(),
            description: None,
            category: Some("rent".into()),
            tag: None,
            business_id: None,
            external_id: Some(ext.into()),
            source: "manual".into(),
        }
    }

    #[test]
    fn prior_month_window_january_wraps() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).single().unwrap();
        let w = MonthWindow::prior_month_ast(as_of);
        assert_eq!(w.label(), "2025-12");
        assert!(w.since < w.until);
    }

    #[test]
    fn report_with_no_prior_snapshot_reports_none() {
        let db = Db::open_memory().unwrap();
        let w = MonthWindow {
            label_year: 2026,
            label_month: 4,
            since: 1_775_000_000,
            until: 1_777_000_000,
        };
        let r = build_monthly_report(&db, w).unwrap();
        assert!(r.net_worth_prior.is_none());
        assert_eq!(r.net_worth_now, Cents::ZERO);
    }

    #[test]
    fn debt_paydown_counts_negative_debt_tx() {
        let db = Db::open_memory().unwrap();
        let card = db.upsert_account(&acct("CC", "debt", None, None)).unwrap();
        // -$200 against debt = $200 paydown.
        db.upsert_transaction(&tx(card, 1_775_500_000, -200_00, "a"))
            .unwrap();
        // +$50 = a new draw, reduces paydown.
        db.upsert_transaction(&tx(card, 1_775_600_000, 50_00, "b"))
            .unwrap();
        let w = MonthWindow {
            label_year: 2026,
            label_month: 4,
            since: 1_775_000_000,
            until: 1_777_000_000,
        };
        let r = build_monthly_report(&db, w).unwrap();
        assert_eq!(r.debt_paydown, Cents(150_00));
    }

    #[test]
    fn render_markdown_contains_section_headers() {
        let db = Db::open_memory().unwrap();
        let e = db
            .upsert_account(&acct("E", "brokerage", Some("equity"), Some("t2")))
            .unwrap();
        db.insert_snapshot(e, Cents(10_000_00), "manual").unwrap();
        let w = MonthWindow::prior_month_ast(Utc::now());
        let report = build_monthly_report(&db, w).unwrap();
        let md = render_markdown(&report);
        assert!(md.contains("# arca monthly report"));
        assert!(md.contains("## Net worth"));
        assert!(md.contains("## Cash flow by business"));
        assert!(md.contains("## Permanent portfolio drift"));
        assert!(md.contains("## Tier 1 backbone"));
        assert!(md.contains("## Debt paydown"));
        assert!(md.contains("## Top expense categories"));
        assert!(md.contains("## API & subscriptions"));
    }

    #[test]
    fn top_expenses_ordered_by_magnitude_desc() {
        let db = Db::open_memory().unwrap();
        let a = db
            .upsert_account(&acct("Checking", "asset", Some("cash"), None))
            .unwrap();
        let w = MonthWindow {
            label_year: 2026,
            label_month: 4,
            since: 1_775_000_000,
            until: 1_777_000_000,
        };
        // Three negative txns with different categories.
        for (i, (cat, amt)) in [("rent", -1_500_00), ("food", -250_00), ("gas", -80_00)]
            .iter()
            .enumerate()
        {
            let t = Transaction {
                id: None,
                account_id: a,
                posted_at: w.since + (i as i64) * 100,
                amount_cents: Cents(*amt),
                currency: "USD".into(),
                description: None,
                category: Some((*cat).into()),
                tag: None,
                business_id: None,
                external_id: Some(format!("e{i}")),
                source: "manual".into(),
            };
            db.upsert_transaction(&t).unwrap();
        }
        let r = build_monthly_report(&db, w).unwrap();
        let cats: Vec<&str> = r.top_expenses.iter().map(|c| c.category.as_str()).collect();
        assert_eq!(cats, vec!["rent", "food", "gas"]);
    }
}
