//! Property tests for arca-core's highest-value invariants (Wave 2).
//!
//! These live in one integration file (not inline) to keep the source modules
//! under the file-size cap and to drive only the public API. Test names are
//! prefixed `prop_` so the safety wave (`qa.sh safety`) can `--skip prop` under
//! Miri/sanitizers, where thousands of cases would be far too slow.
//!
//! Three invariants:
//!   - money: `Cents` parses/displays without float drift, arithmetic obeys the
//!     monoid laws (commutative add, zero identity, neg involution, sum == fold).
//!   - pp: `any_band_breach(drift(a))` matches an independent reference predicate.
//!   - db: `upsert_account` is idempotent (twice == once) and updates in place.

use arca_core::db::{Account, Db};
use arca_core::money::Cents;
use arca_core::pp::{Allocation, T2_LOWER_BAND_PCT, T2_UPPER_BAND_PCT, any_band_breach, drift};
use proptest::prelude::*;

// Generous bound that still keeps every value far inside i64 range, so the
// round-trip and arithmetic laws hold without saturation/overflow corner cases
// (which are guarded separately by from_dollars_str's checked math). ±$10 trillion.
const CENTS_BOUND: i64 = 1_000_000_000_000;

// ---- money ----

proptest! {
    /// Display then re-parse is the identity (no float drift — it's all i64).
    #[test]
    fn prop_cents_display_roundtrips(n in -CENTS_BOUND..=CENTS_BOUND) {
        let c = Cents(n);
        prop_assert_eq!(Cents::from_dollars_str(&c.to_string()).unwrap(), c);
    }

    /// A canonical `[-]whole.frac` string parses to exactly the cents it denotes.
    #[test]
    fn prop_from_dollars_str_parses_canonical(
        neg in any::<bool>(),
        whole in 0u64..=10_000_000_000,
        frac in 0u64..=99,
    ) {
        let s = format!("{}{}.{:02}", if neg { "-" } else { "" }, whole, frac);
        let sign = if neg { -1 } else { 1 };
        let expected = sign * (whole as i64 * 100 + frac as i64);
        prop_assert_eq!(Cents::from_dollars_str(&s).unwrap(), Cents(expected));
    }

    /// Addition is commutative and has `ZERO` as identity (saturating, so the
    /// bounded range guarantees no saturation surprises).
    #[test]
    fn prop_add_commutative_with_identity(
        a in -CENTS_BOUND..=CENTS_BOUND,
        b in -CENTS_BOUND..=CENTS_BOUND,
    ) {
        let (a, b) = (Cents(a), Cents(b));
        prop_assert_eq!(a + b, b + a);
        prop_assert_eq!(a + Cents::ZERO, a);
        prop_assert_eq!(b - b, Cents::ZERO);
    }

    /// Negation is its own inverse over the bounded range (i64::MIN excluded).
    #[test]
    fn prop_neg_involution(a in -CENTS_BOUND..=CENTS_BOUND) {
        let a = Cents(a);
        prop_assert_eq!(-(-a), a);
    }

    /// `Sum` agrees with an explicit fold.
    #[test]
    fn prop_sum_equals_fold(xs in prop::collection::vec(-CENTS_BOUND..=CENTS_BOUND, 0..32)) {
        let cents: Vec<Cents> = xs.into_iter().map(Cents).collect();
        let summed: Cents = cents.iter().copied().sum();
        let folded = cents.iter().copied().fold(Cents::ZERO, |acc, c| acc + c);
        prop_assert_eq!(summed, folded);
    }
}

// ---- pp band math ----

/// Independent reference for `any_band_breach`: mirror `drift`'s exact float
/// arithmetic so the comparison is bit-for-bit at the band boundaries. Only the
/// three targeted sleeves (equity / long_treasuries / cash) can breach; a zero
/// total never breaches.
fn ref_any_breach(a: &Allocation) -> bool {
    let total = a.total.0;
    // drift gates every breach on `total > 0`, so a zero or net-negative T2 total
    // never trips a band — mirror that exactly.
    if total <= 0 {
        return false;
    }
    [a.equity, a.long_treasuries, a.cash].iter().any(|c| {
        let pct = c.0 as f64 / total as f64 * 100.0;
        !(T2_LOWER_BAND_PCT..=T2_UPPER_BAND_PCT).contains(&pct)
    })
}

fn alloc_strategy() -> impl Strategy<Value = Allocation> {
    let sleeve = -100_000_000_000i64..=100_000_000_000;
    (
        sleeve.clone(),
        sleeve.clone(),
        sleeve.clone(),
        sleeve.clone(),
        sleeve,
    )
        .prop_map(|(equity, long_treasuries, cash, gold_etf, other)| {
            let (equity, long_treasuries, cash, gold_etf, other) = (
                Cents(equity),
                Cents(long_treasuries),
                Cents(cash),
                Cents(gold_etf),
                Cents(other),
            );
            // total is the sum of all five sleeves, exactly as `allocation()` builds it.
            let total = equity + long_treasuries + cash + gold_etf + other;
            Allocation {
                equity,
                long_treasuries,
                cash,
                gold_etf,
                other,
                total,
            }
        })
}

proptest! {
    /// The engine's breach flag matches the independent reference for every
    /// allocation, including the total==0 and exactly-at-band edges.
    #[test]
    fn prop_band_breach_matches_reference(a in alloc_strategy()) {
        prop_assert_eq!(any_band_breach(&drift(&a)), ref_any_breach(&a));
    }

    /// Structural invariants of the drift table: always the three targeted sleeves
    /// first; gold_etf/other rows appear iff non-zero and never carry a breach.
    #[test]
    fn prop_drift_table_shape(a in alloc_strategy()) {
        let rows = drift(&a);
        prop_assert!(rows.len() >= 3);
        prop_assert_eq!(&rows[0].asset_class, "equity");
        prop_assert_eq!(&rows[1].asset_class, "long_treasuries");
        prop_assert_eq!(&rows[2].asset_class, "cash");
        let has_gold = rows.iter().any(|r| r.asset_class == "gold_etf");
        let has_other = rows.iter().any(|r| r.asset_class == "other");
        prop_assert_eq!(has_gold, a.gold_etf.0 != 0);
        prop_assert_eq!(has_other, a.other.0 != 0);
        for r in &rows {
            if r.asset_class == "gold_etf" || r.asset_class == "other" {
                prop_assert!(!r.band_breach);
            }
            prop_assert!((r.drift_pp - (r.actual_pct - r.target_pct)).abs() < 1e-9);
        }
    }
}

// ---- db idempotent upserts ----

proptest! {
    // A fresh in-memory DB per case (sqlite temp); keep the case count modest so
    // the suite stays fast under `cargo careful`.
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Upserting the same manual account twice yields the same row id and no
    /// duplicate; upserting again with a changed field updates that row in place.
    #[test]
    fn prop_upsert_account_idempotent_and_updates_in_place(
        name in "[a-zA-Z0-9_-]{1,40}",
        kind_a in prop::sample::select(vec!["checking", "brokerage", "asset", "credit"]),
        kind_b in prop::sample::select(vec!["checking", "brokerage", "asset", "credit"]),
    ) {
        let db = Db::open_memory().unwrap();
        let mk = |kind: &str| Account {
            id: None,
            name: name.clone(),
            kind: kind.to_string(),
            asset_class: None,
            tier: None,
            currency: "USD".into(),
            provider_id: None,
            business_id: None,
            external_id: None,
            active: true,
        };
        let id1 = db.upsert_account(&mk(kind_a)).unwrap();
        let id2 = db.upsert_account(&mk(kind_a)).unwrap(); // same input twice == once
        let id3 = db.upsert_account(&mk(kind_b)).unwrap(); // changed field, in place
        prop_assert_eq!(id1, id2);
        prop_assert_eq!(id1, id3);
        let rows: Vec<Account> = db
            .list_active_accounts()
            .unwrap()
            .into_iter()
            .filter(|a| a.name == name)
            .collect();
        prop_assert_eq!(rows.len(), 1);
        prop_assert_eq!(rows[0].kind.as_str(), kind_b);
    }
}
