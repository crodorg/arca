//! Debt view queries.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;
use crate::money::Cents;
use crate::rpc::{DebtBalance, DebtScheduled, Scope};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebtView {
    pub open: Vec<DebtBalance>,
    pub scheduled: Vec<DebtScheduled>,
    pub total_open: Cents,
}

pub fn debt_view(db: &Db, scope: Scope) -> Result<DebtView> {
    let (since, until) = scope_bounds(scope);
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT a.name, COALESCE(b.amount_cents, 0)
               FROM accounts a
               LEFT JOIN (
                 SELECT account_id, amount_cents,
                        ROW_NUMBER() OVER (
                          PARTITION BY account_id ORDER BY taken_at DESC, id DESC
                        ) AS rn
                   FROM balance_snapshots
               ) b ON b.account_id = a.id AND b.rn = 1
              WHERE a.kind = 'debt' AND a.active = 1
              ORDER BY a.name",
        )?;
        let mut open = Vec::new();
        let mut total = Cents::ZERO;
        let rows = stmt.query_map([], |r| {
            Ok(DebtBalance {
                account_name: r.get::<_, String>(0)?,
                balance: Cents(r.get::<_, i64>(1)?),
            })
        })?;
        for row in rows {
            let b = row?;
            total += b.balance;
            open.push(b);
        }

        let mut stmt = conn.prepare(
            "SELECT pe.due_at, pe.amount_cents, pe.description
               FROM planned_expenses pe
               JOIN accounts a ON a.id = pe.account_id
              WHERE a.kind = 'debt'
                AND pe.status = 'planned'
                AND pe.due_at >= ?1 AND pe.due_at < ?2
              ORDER BY pe.due_at",
        )?;
        let scheduled = stmt
            .query_map(params![since, until], |r| {
                Ok(DebtScheduled {
                    due_at: r.get(0)?,
                    amount: Cents(r.get::<_, i64>(1)?),
                    description: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(DebtView {
            open,
            scheduled,
            total_open: total,
        })
    })
}

/// Active declared recurring obligations (the `subscriptions` table, kind
/// `recurring`) projected to their next occurrence ≥ now via `cadence` — one
/// upcoming row per obligation. These are NOT debt: they never touch
/// `total_open`, `open_balances`, or net worth; they ride alongside scheduled
/// debt service purely so the operator sees money going out. Subscriptions
/// without an amount or next-charge date, or with an unknown cadence, are
/// skipped (we never invent a date). Soonest first.
pub fn recurring_obligations(db: &Db) -> Result<Vec<DebtScheduled>> {
    use chrono::Utc;
    let now = Utc::now().timestamp();
    let mut out: Vec<DebtScheduled> = db
        .list_active_subscriptions()?
        .into_iter()
        .filter(|s| s.provider_kind == "recurring")
        .filter_map(|s| {
            let amount = s.amount_cents?;
            let next0 = s.next_charge_at?;
            let due = project_next(next0, s.cadence.as_deref(), now)?;
            Some(DebtScheduled {
                due_at: due,
                amount,
                description: s.name,
            })
        })
        .collect();
    out.sort_by_key(|d| d.due_at);
    Ok(out)
}

/// Project a recurring charge from `start` to its next occurrence ≥ `now`.
/// Monthly/quarterly/yearly step by whole calendar months (so a "1st of month"
/// charge stays on the 1st); weekly/biweekly step fixed day counts. Returns
/// `None` for an unknown/None cadence rather than looping or guessing, and
/// `None` if the loop backstop is exhausted (so the `≥ now` postcondition always
/// holds — callers skip rather than surface a past date).
///
/// Note: monthly+ steps use `checked_add_months`, which clamps to the last valid
/// day when the source day-of-month is absent (Jan 31 → Feb 28). Because each
/// step works off the previously-bumped date, a 31st-of-month charge that passes
/// through February re-anchors earlier (28th) and does not snap back — an
/// accepted forecast approximation (`predicted_next` is a forecast, not a
/// commitment, and amounts are untouched).
fn project_next(start: i64, cadence: Option<&str>, now: i64) -> Option<i64> {
    use chrono::{Duration, Months, TimeZone, Utc};
    if start >= now {
        return Some(start);
    }
    let mut dt = Utc.timestamp_opt(start, 0).single()?;
    let bump = |d: chrono::DateTime<Utc>, cad: &str| -> Option<chrono::DateTime<Utc>> {
        match cad {
            "monthly" => d.checked_add_months(Months::new(1)),
            "quarterly" => d.checked_add_months(Months::new(3)),
            "yearly" => d.checked_add_months(Months::new(12)),
            "weekly" => Some(d + Duration::days(7)),
            "biweekly" => Some(d + Duration::days(14)),
            _ => None,
        }
    };
    let cad = cadence?;
    for _ in 0..1200 {
        if dt.timestamp() >= now {
            return Some(dt.timestamp());
        }
        dt = bump(dt, cad)?;
    }
    // Backstop exhausted (pathological stored date). Fail closed rather than
    // return a value that breaks the documented `≥ now` postcondition.
    None
}

fn scope_bounds(scope: Scope) -> (i64, i64) {
    use chrono::Utc;
    scope_bounds_at(Utc::now(), scope)
}

/// Half-open `[since, until)` window for a scope, anchored at `now` (UTC
/// calendar). `until` is the start of the *next* calendar period — the exact
/// month/year boundary, not a fixed day count — so short months (28/29/30 days)
/// and non-leap years don't bleed into the following period.
fn scope_bounds_at(now: chrono::DateTime<chrono::Utc>, scope: Scope) -> (i64, i64) {
    use chrono::{Datelike, Months, TimeZone, Utc};
    match scope {
        Scope::Month => {
            let start_dt = Utc
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single();
            let start = start_dt.map_or(0, |d| d.timestamp());
            let until = start_dt
                .and_then(|d| d.checked_add_months(Months::new(1)))
                .map_or(start, |d| d.timestamp());
            (start, until)
        }
        Scope::Year | Scope::Ytd => {
            let start_dt = Utc.with_ymd_and_hms(now.year(), 1, 1, 0, 0, 0).single();
            let start = start_dt.map_or(0, |d| d.timestamp());
            let until = start_dt
                .and_then(|d| d.checked_add_months(Months::new(12)))
                .map_or(start, |d| d.timestamp());
            (start, until)
        }
        Scope::All => (0, i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Cents;

    #[test]
    fn project_next_advances_monthly_to_future() {
        let now = 1_800_000_000; // fixed reference
        // A monthly charge whose stored date is ~3 months in the past projects to
        // the next occurrence that is >= now (not the stale date, not earlier).
        let start = now - 90 * 86_400;
        let due = project_next(start, Some("monthly"), now).unwrap();
        assert!(due >= now, "projected {due} must be >= now {now}");
        // ...and it's the *first* such occurrence (previous step is < now).
        assert!(due - now < 32 * 86_400, "should be within one month of now");
    }

    #[test]
    fn project_next_keeps_future_date_and_rejects_unknown() {
        let now = 1_800_000_000;
        let future = now + 10 * 86_400;
        assert_eq!(project_next(future, Some("monthly"), now), Some(future));
        // Unknown/None cadence never guesses a date.
        assert_eq!(project_next(now - 86_400, Some("whenever"), now), None);
        assert_eq!(project_next(now - 86_400, None, now), None);
    }

    #[test]
    fn recurring_obligations_projects_active_recurring_only() {
        let db = Db::open_memory().unwrap();
        let now = chrono::Utc::now().timestamp();
        // Active monthly rent with a stale next_charge_at → should project forward.
        db.upsert_subscription(
            "Rent",
            "recurring",
            Some(Cents(-2_000_00)),
            Some("monthly"),
            Some(now - 100 * 86_400),
            None,
            true,
        )
        .unwrap();
        // A usage_based row must be excluded (not a fixed obligation).
        db.upsert_subscription(
            "OpenAI",
            "usage_based",
            Some(Cents(-50_00)),
            Some("monthly"),
            Some(now - 5 * 86_400),
            None,
            true,
        )
        .unwrap();
        let out = recurring_obligations(&db).unwrap();
        assert_eq!(
            out.len(),
            1,
            "only the recurring obligation, not usage_based"
        );
        assert_eq!(out[0].description, "Rent");
        assert_eq!(out[0].amount, Cents(-2_000_00));
        assert!(out[0].due_at >= now, "rent projected to a future date");
    }

    #[test]
    fn update_subscription_renames_and_deactivates() {
        let db = Db::open_memory().unwrap();
        let now = chrono::Utc::now().timestamp();
        db.upsert_subscription(
            "Rent",
            "recurring",
            Some(Cents(-2_000_00)),
            Some("monthly"),
            Some(now + 5 * 86_400),
            None,
            true,
        )
        .unwrap();
        // Rename: 1 row changed, the obligation now shows the new name.
        assert_eq!(db.rename_subscription("Rent", "Apartment").unwrap(), 1);
        let out = recurring_obligations(&db).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].description, "Apartment");
        // Renaming a name that no longer exists changes nothing.
        assert_eq!(db.rename_subscription("Rent", "X").unwrap(), 0);
        // Deactivate by the new name drops it from the obligations; reactivate restores.
        assert_eq!(db.set_subscription_active("Apartment", false).unwrap(), 1);
        assert!(recurring_obligations(&db).unwrap().is_empty());
        assert_eq!(db.set_subscription_active("Apartment", true).unwrap(), 1);
        assert_eq!(recurring_obligations(&db).unwrap().len(), 1);
    }

    #[test]
    fn scope_bounds_month_ends_at_next_calendar_month() {
        use chrono::{TimeZone, Utc};
        // Non-leap February: window must be exactly 28 days, not 31.
        let feb = Utc.with_ymd_and_hms(2026, 2, 14, 9, 0, 0).unwrap();
        let (since, until) = scope_bounds_at(feb, Scope::Month);
        assert_eq!(
            since,
            Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(
            until,
            Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!((until - since) / 86_400, 28, "Feb 2026 is 28 days, not 31");

        // Leap February: 29 days.
        let leap = Utc.with_ymd_and_hms(2024, 2, 14, 9, 0, 0).unwrap();
        let (s2, u2) = scope_bounds_at(leap, Scope::Month);
        assert_eq!((u2 - s2) / 86_400, 29, "Feb 2024 is 29 days");

        // December rolls to next January (year boundary).
        let dec = Utc.with_ymd_and_hms(2026, 12, 9, 0, 0, 0).unwrap();
        let (_s3, u3) = scope_bounds_at(dec, Scope::Month);
        assert_eq!(
            u3,
            Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn scope_bounds_year_ends_at_next_january() {
        use chrono::{TimeZone, Utc};
        // Non-leap year: 365 days, not the old fixed 366.
        let y = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let (since, until) = scope_bounds_at(y, Scope::Year);
        assert_eq!(
            since,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(
            until,
            Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
        assert_eq!((until - since) / 86_400, 365, "2026 is 365 days");

        // Leap year: 366 days.
        let leap = Utc.with_ymd_and_hms(2024, 7, 1, 0, 0, 0).unwrap();
        let (s2, u2) = scope_bounds_at(leap, Scope::Year);
        assert_eq!((u2 - s2) / 86_400, 366, "2024 is 366 days");
    }
}
