//! Three-tier capital structure math.
//!
//! Authoritative reference: `the investment-model spec`. The four-asset
//! Browne PP is *not* the operator's framework. Replaced with a three-tier
//! split where Tier 2 is the drift-tracked PP-style operations layer.
//!
//! Tier definitions (accounts.tier column):
//! - **t1** — antifragile backbone (gold, silver, land, xmr, optional 1–3 SFR).
//!   Hold forever. No rebalance. Reported as info-only `Backbone` summary;
//!   never triggers drift alerts.
//! - **t2** — liquid PP variant: equity / long_treasuries / cash, each ~33.33%.
//!   Rebalance bands: any sleeve < 22% or > 44% of T2 total triggers a
//!   rebalance event. See `the investment-model spec`.
//! - **t3** — operating capital (emergency fund, business working capital).
//!   Not part of PP math.
//! - **NULL** — uncategorized: debts, utilities, subscriptions, business
//!   transaction cash. Excluded from both T1 backbone and T2 drift.
//!
//! Asset-class vocabulary used by `drift()` for T2 rows:
//!   - `equity`            (also accepts legacy `stocks`)
//!   - `long_treasuries`   (also accepts legacy `bonds`)
//!   - `cash`
//!   - `gold_etf`          (optional 5–10% liquidity sleeve inside T2; reported
//!     but no target — drift reported against zero)
//!
//! Asset-class vocabulary used by `backbone()` for T1 rows:
//!   - `gold`, `silver`, `xmr`, `land`, `sfr`, plus anything else (collected
//!     into `other`).

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;
use crate::money::Cents;

/// Target percentage for each of the three T2 sleeves (equity / long_treasuries / cash).
pub const T2_TARGET_PCT: f64 = 100.0 / 3.0;

/// Lower rebalance band — any T2 sleeve below this triggers a rebalance event.
pub const T2_LOWER_BAND_PCT: f64 = 22.0;

/// Upper rebalance band — any T2 sleeve above this triggers a rebalance event.
pub const T2_UPPER_BAND_PCT: f64 = 44.0;

/// Latest-balance allocation for the Tier-2 PP variant.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Allocation {
    pub equity: Cents,
    pub long_treasuries: Cents,
    pub cash: Cents,
    /// Optional 5–10% T2 gold ETF for rebalance liquidity (Rowland's "some
    /// local, some not" guidance). Not part of the 33×3 target; reported
    /// separately for visibility.
    pub gold_etf: Cents,
    /// Anything in `tier='t2'` with an unrecognized asset_class. Surfaced so
    /// misconfigured rows don't silently vanish.
    pub other: Cents,
    pub total: Cents,
}

/// One drift row from the T2 PP allocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriftRow {
    pub asset_class: String,
    pub actual_cents: Cents,
    pub actual_pct: f64,
    pub target_pct: f64,
    pub drift_pp: f64,
    /// Lower rebalance band (% of total). Zero for non-targeted rows.
    pub lower_band_pct: f64,
    /// Upper rebalance band (% of total). Zero for non-targeted rows.
    pub upper_band_pct: f64,
    /// True if `actual_pct` is outside `[lower_band_pct, upper_band_pct]` for a
    /// targeted sleeve. Always false for `gold_etf` and `other`.
    pub band_breach: bool,
}

/// Tier-1 hold-forever backbone summary. Info-only; never drift-tracked.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Backbone {
    pub gold: Cents,
    pub silver: Cents,
    pub xmr: Cents,
    pub land: Cents,
    pub sfr: Cents,
    pub other: Cents,
    pub total: Cents,
}

/// Build the Tier-2 PP allocation from `accounts.tier='t2'` rows joined to their
/// latest balance snapshot.
pub fn allocation(db: &Db) -> Result<Allocation> {
    let balances = db.latest_balances()?;
    let accounts = db.list_active_accounts()?;
    let mut alloc = Allocation::default();
    for (aid, bal) in balances {
        let Some(acct) = accounts.iter().find(|a| a.id == Some(aid)) else {
            continue;
        };
        if acct.tier.as_deref() != Some("t2") {
            continue;
        }
        if acct.currency != "USD" {
            continue; // non-USD excluded from the USD-cents PP total (no FX, v1)
        }
        match acct.asset_class.as_deref() {
            Some("equity" | "stocks") => alloc.equity += bal,
            Some("long_treasuries" | "bonds") => alloc.long_treasuries += bal,
            Some("cash") => alloc.cash += bal,
            Some("gold_etf") => alloc.gold_etf += bal,
            _ => alloc.other += bal,
        }
    }
    alloc.total = alloc.equity + alloc.long_treasuries + alloc.cash + alloc.gold_etf + alloc.other;
    Ok(alloc)
}

/// Drift table for the T2 allocation against the 33×3 target with 22/44 bands.
#[must_use]
pub fn drift(a: &Allocation) -> Vec<DriftRow> {
    let total = a.total.0;
    let pct = |c: Cents| {
        if total == 0 {
            0.0
        } else {
            (c.0 as f64 / total as f64) * 100.0
        }
    };
    let mk_targeted = |class: &str, amt: Cents| {
        let actual_pct = pct(amt);
        let breach = !(T2_LOWER_BAND_PCT..=T2_UPPER_BAND_PCT).contains(&actual_pct);
        DriftRow {
            asset_class: class.into(),
            actual_cents: amt,
            actual_pct,
            target_pct: T2_TARGET_PCT,
            drift_pp: actual_pct - T2_TARGET_PCT,
            lower_band_pct: T2_LOWER_BAND_PCT,
            upper_band_pct: T2_UPPER_BAND_PCT,
            band_breach: total > 0 && breach,
        }
    };
    let mut rows = vec![
        mk_targeted("equity", a.equity),
        mk_targeted("long_treasuries", a.long_treasuries),
        mk_targeted("cash", a.cash),
    ];
    if a.gold_etf.0 != 0 {
        let actual_pct = pct(a.gold_etf);
        rows.push(DriftRow {
            asset_class: "gold_etf".into(),
            actual_cents: a.gold_etf,
            actual_pct,
            target_pct: 0.0,
            drift_pp: actual_pct,
            lower_band_pct: 0.0,
            upper_band_pct: 0.0,
            band_breach: false,
        });
    }
    if a.other.0 != 0 {
        let actual_pct = pct(a.other);
        rows.push(DriftRow {
            asset_class: "other".into(),
            actual_cents: a.other,
            actual_pct,
            target_pct: 0.0,
            drift_pp: actual_pct,
            lower_band_pct: 0.0,
            upper_band_pct: 0.0,
            band_breach: false,
        });
    }
    rows
}

/// True if any T2 sleeve has breached a rebalance band — the predicate the
/// alert engine fires on.
#[must_use]
pub fn any_band_breach(rows: &[DriftRow]) -> bool {
    rows.iter().any(|r| r.band_breach)
}

/// Build the Tier-1 hold-forever backbone summary. Info-only.
pub fn backbone(db: &Db) -> Result<Backbone> {
    let balances = db.latest_balances()?;
    let accounts = db.list_active_accounts()?;
    let mut b = Backbone::default();
    for (aid, bal) in balances {
        let Some(acct) = accounts.iter().find(|a| a.id == Some(aid)) else {
            continue;
        };
        if acct.tier.as_deref() != Some("t1") {
            continue;
        }
        if acct.currency != "USD" {
            continue; // non-USD excluded from the USD-cents backbone total (v1)
        }
        match acct.asset_class.as_deref() {
            Some("gold") => b.gold += bal,
            Some("silver") => b.silver += bal,
            Some("xmr" | "monero") => b.xmr += bal,
            Some("land") => b.land += bal,
            Some("sfr" | "real_estate") => b.sfr += bal,
            _ => b.other += bal,
        }
    }
    b.total = b.gold + b.silver + b.xmr + b.land + b.sfr + b.other;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Account;

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

    #[test]
    fn even_t2_split_no_breach() {
        let db = Db::open_memory().unwrap();
        for (n, ac) in [("E", "equity"), ("B", "long_treasuries"), ("C", "cash")] {
            let id = db
                .upsert_account(&acct(n, "brokerage", Some(ac), Some("t2")))
                .unwrap();
            db.insert_snapshot(id, Cents(10_000_00), "manual").unwrap();
        }
        let alloc = allocation(&db).unwrap();
        assert_eq!(alloc.total, Cents(30_000_00));
        let rows = drift(&alloc);
        for r in &rows {
            assert!((r.actual_pct - T2_TARGET_PCT).abs() < 0.01, "got {r:?}");
            assert!(!r.band_breach);
        }
        assert!(!any_band_breach(&rows));
    }

    #[test]
    fn t1_rows_ignored_by_drift() {
        let db = Db::open_memory().unwrap();
        // T1 gold should not contribute to T2 allocation.
        let g = db
            .upsert_account(&acct("Gold", "asset", Some("gold"), Some("t1")))
            .unwrap();
        db.insert_snapshot(g, Cents(50_000_00), "manual").unwrap();
        let alloc = allocation(&db).unwrap();
        assert_eq!(alloc.total, Cents(0));
        let bb = backbone(&db).unwrap();
        assert_eq!(bb.gold, Cents(50_000_00));
        assert_eq!(bb.total, Cents(50_000_00));
    }

    #[test]
    fn legacy_asset_classes_map_into_t2() {
        // Existing rows tagged with the legacy 4×25 `stocks`/`bonds` strings
        // still aggregate correctly into the T2 sleeves.
        let db = Db::open_memory().unwrap();
        let s = db
            .upsert_account(&acct("VTI", "brokerage", Some("stocks"), Some("t2")))
            .unwrap();
        let b = db
            .upsert_account(&acct("TLT", "brokerage", Some("bonds"), Some("t2")))
            .unwrap();
        db.insert_snapshot(s, Cents(33_333_33), "manual").unwrap();
        db.insert_snapshot(b, Cents(33_333_33), "manual").unwrap();
        let alloc = allocation(&db).unwrap();
        assert_eq!(alloc.equity, Cents(33_333_33));
        assert_eq!(alloc.long_treasuries, Cents(33_333_33));
    }

    #[test]
    fn skewed_t2_breaches_lower_band() {
        let db = Db::open_memory().unwrap();
        let e = db
            .upsert_account(&acct("E", "brokerage", Some("equity"), Some("t2")))
            .unwrap();
        let b = db
            .upsert_account(&acct("B", "brokerage", Some("long_treasuries"), Some("t2")))
            .unwrap();
        let c = db
            .upsert_account(&acct("C", "asset", Some("cash"), Some("t2")))
            .unwrap();
        // Equity 60%, bonds 30%, cash 10% — cash < 22% (lower band breach),
        // equity > 44% (upper band breach), bonds in-band.
        db.insert_snapshot(e, Cents(60_000_00), "manual").unwrap();
        db.insert_snapshot(b, Cents(30_000_00), "manual").unwrap();
        db.insert_snapshot(c, Cents(10_000_00), "manual").unwrap();
        let alloc = allocation(&db).unwrap();
        let rows = drift(&alloc);
        let by = |name: &str| rows.iter().find(|r| r.asset_class == name).unwrap().clone();
        let eq = by("equity");
        let bn = by("long_treasuries");
        let ca = by("cash");
        assert!((eq.actual_pct - 60.0).abs() < 0.01);
        assert!((bn.actual_pct - 30.0).abs() < 0.01);
        assert!((ca.actual_pct - 10.0).abs() < 0.01);
        assert!(eq.band_breach, "60% > 44% upper band");
        assert!(!bn.band_breach, "30% in-band");
        assert!(ca.band_breach, "10% < 22% lower band");
        assert!(any_band_breach(&rows));
    }

    #[test]
    fn empty_alloc_no_breach() {
        let db = Db::open_memory().unwrap();
        let alloc = allocation(&db).unwrap();
        let rows = drift(&alloc);
        assert!(!any_band_breach(&rows));
        for r in &rows {
            assert!(r.actual_pct.abs() < f64::EPSILON);
            assert!(!r.band_breach, "no breach when total=0");
        }
    }

    #[test]
    fn t2_gold_etf_and_other_sleeves_aggregate_into_total() {
        // The optional gold_etf liquidity sleeve and the catch-all `other` (a t2
        // row with an unrecognized asset_class) both count toward the T2 total but
        // never carry a band breach.
        let db = Db::open_memory().unwrap();
        let g = db
            .upsert_account(&acct("GLD", "brokerage", Some("gold_etf"), Some("t2")))
            .unwrap();
        let o = db
            .upsert_account(&acct("WEIRD", "brokerage", Some("crypto_fund"), Some("t2")))
            .unwrap();
        db.insert_snapshot(g, Cents(5_000_00), "manual").unwrap();
        db.insert_snapshot(o, Cents(1_000_00), "manual").unwrap();
        let alloc = allocation(&db).unwrap();
        assert_eq!(alloc.gold_etf, Cents(5_000_00));
        assert_eq!(alloc.other, Cents(1_000_00));
        assert_eq!(alloc.total, Cents(6_000_00));
        let rows = drift(&alloc);
        for r in rows
            .iter()
            .filter(|r| r.asset_class == "gold_etf" || r.asset_class == "other")
        {
            assert!(!r.band_breach, "{} never breaches", r.asset_class);
        }
    }

    #[test]
    fn backbone_aggregates_every_t1_class() {
        let db = Db::open_memory().unwrap();
        for (n, ac, amt) in [
            ("G", "gold", 10_000_00),
            ("S", "silver", 2_000_00),
            ("X", "xmr", 3_000_00),
            ("L", "land", 50_000_00),
            ("H", "sfr", 80_000_00),
            ("Z", "collectible", 1_000_00), // unrecognized -> `other`
        ] {
            let id = db
                .upsert_account(&acct(n, "asset", Some(ac), Some("t1")))
                .unwrap();
            db.insert_snapshot(id, Cents(amt), "manual").unwrap();
        }
        let b = backbone(&db).unwrap();
        assert_eq!(b.gold, Cents(10_000_00));
        assert_eq!(b.silver, Cents(2_000_00));
        assert_eq!(b.xmr, Cents(3_000_00));
        assert_eq!(b.land, Cents(50_000_00));
        assert_eq!(b.sfr, Cents(80_000_00));
        assert_eq!(b.other, Cents(1_000_00));
        assert_eq!(b.total, Cents(146_000_00));
    }
}
