//! Detection tests reaching `recurring.rs` through its public `Db` path.
//!
//! recurring.rs sits at the file-size cap, so these live out here rather than
//! inline. Each pins a correctness point a Wave-2 mutation survivor exposed:
//!   - `Cadence::as_str`'s strings are contractual — the monthly report and the
//!     `.ics` calendar render them verbatim.
//!   - `classify` keys cadence off the MEDIAN gap (`gaps[len/2]`), not an index
//!     artifact like `gaps[len % 2]`.
//!   - only genuine outflows (`amount < 0`) are charges; a regular $0.00 series is
//!     not one (the `< 0` boundary must not slip to `<= 0`).

use arca_core::db::Db;
use arca_core::recurring::{Cadence, MIN_OCCURRENCES, labeled_series};

const DAY: i64 = 86_400;

fn open_db_with_account() -> Db {
    let db = Db::open_memory().unwrap();
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO accounts (id, name, kind, currency, active, created_at)
             VALUES (1, 'CC', 'asset', 'USD', 1, 0)",
            [],
        )
        .map_err(Into::into)
    })
    .unwrap();
    db
}

fn insert_charge(db: &Db, day: i64, cents: i64, desc: &str, ext: &str) {
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO transactions
               (account_id, posted_at, amount_cents, currency, description, source, external_id)
             VALUES (1, ?1, ?2, 'USD', ?3, 'manual', ?4)",
            rusqlite::params![day * DAY, cents, desc, ext],
        )
        .map_err(Into::into)
    })
    .unwrap();
}

#[test]
fn cadence_as_str_is_stable() {
    assert_eq!(Cadence::Weekly.as_str(), "weekly");
    assert_eq!(Cadence::Biweekly.as_str(), "biweekly");
    assert_eq!(Cadence::Monthly.as_str(), "monthly");
    assert_eq!(Cadence::Quarterly.as_str(), "quarterly");
    assert_eq!(Cadence::Yearly.as_str(), "yearly");
}

#[test]
fn classify_uses_the_median_gap_not_an_index_artifact() {
    // Gaps [5, 5, 30, 30] (sorted, len 4): the median is index len/2 = 2 -> 30 days
    // -> Monthly. A `len/2 -> len % 2` slip reads index 0 -> 5 days -> Weekly.
    let db = open_db_with_account();
    for (i, day) in [0_i64, 5, 10, 40, 70].iter().enumerate() {
        insert_charge(&db, *day, -1599, "MedianCo", &format!("m{i}"));
    }
    let series = labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
    let s = series
        .iter()
        .find(|ls| ls.series.payee == "medianco")
        .expect("MedianCo detected as a series");
    assert_eq!(s.series.cadence, Cadence::Monthly);
}

#[test]
fn zero_amount_transactions_are_not_charges() {
    // A perfectly regular monthly $0.00 "series" must NOT be detected — only true
    // outflows (amount < 0) are charges. A genuine outflow series is the control,
    // proving detection itself works on the same DB.
    let db = open_db_with_account();
    for (i, day) in [0_i64, 30, 61, 91].iter().enumerate() {
        insert_charge(&db, *day, 0, "ZeroCo", &format!("z{i}"));
        insert_charge(&db, *day, -1599, "RealCo", &format!("r{i}"));
    }
    let series = labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
    assert!(
        series.iter().any(|ls| ls.series.payee == "realco"),
        "control outflow series should be detected"
    );
    assert!(
        !series.iter().any(|ls| ls.series.payee == "zeroco"),
        "a $0.00 series must not be treated as recurring charges"
    );
}
