//! Demo-data seeder for screenshots and the VHS recording.
//!
//! Populates a **fresh** database with realistic-looking but entirely fictional
//! finances so the TUI can be recorded without exposing a single real number.
//! All names are made up (`Acme Cloud`, `Globex Apps`); any resemblance to a real
//! account is coincidental. It writes only through the public [`Db`] surface
//! (plus [`Db::with_conn`] for backdated balance snapshots, mirroring the pattern
//! the unit tests use) — no schema change, no new RPC verb, no provider.
//!
//! Wired to the daemon's `seed-demo` subcommand, which refuses to overwrite an
//! existing file, so this can never run against the production DB.

use rusqlite::params;

use crate::db::{Account, Db, Transaction};
use crate::error::Result;
use crate::ids::{AccountId, BusinessId};
use crate::money::Cents;
use crate::time::now_secs;

const DAY: i64 = 86_400;
/// Inter-charge spacing for the seeded recurring series. 30 days lands squarely
/// in the detector's "monthly" band, so each ≥3-occurrence payee is detected.
const MONTH: i64 = 30 * DAY;
/// Months of history to generate (transactions + the net-worth trend).
const HISTORY_MONTHS: i64 = 6;

/// The two fictional businesses, by tag.
struct Businesses {
    acme: BusinessId,
    globex: BusinessId,
}

/// Accounts the later seed steps need by name, plus the full list of
/// `(account, current_balance_cents, currency)` used to write snapshots.
struct Seeded {
    checking: AccountId,
    acme_cash: AccountId,
    globex_cash: AccountId,
    balances: Vec<(AccountId, i64, &'static str)>,
}

/// Seed a fresh demo database. Idempotent on a given file only in the trivial
/// sense that upserts dedup; intended to run once against a brand-new DB.
pub fn seed(db: &Db) -> Result<()> {
    let biz = seed_businesses(db)?;
    let seeded = seed_accounts(db, &biz)?;
    seed_balances(db, &seeded)?;
    seed_transactions(db, &seeded, &biz)?;
    seed_subscriptions(db, &biz)?;
    seed_alerts(db)?;
    Ok(())
}

fn seed_businesses(db: &Db) -> Result<Businesses> {
    // Migration 0001 seeds a default business into every DB; deactivate it so the
    // demo shows only the two fictional ones (keeps the recording on-brand).
    db.upsert_business("main", None, Some(false))?;
    Ok(Businesses {
        acme: db.upsert_business("acme", Some("Acme Cloud LLC"), Some(true))?,
        globex: db.upsert_business("globex", Some("Globex Apps LLC"), Some(true))?,
    })
}

/// Build one account row. `provider_id`/`external_id` stay `None` — demo rows are
/// manual, and a fresh DB dedups fine on the unique name.
fn account(
    name: &str,
    kind: &str,
    asset_class: Option<&str>,
    tier: Option<&str>,
    currency: &str,
    business: Option<BusinessId>,
) -> Account {
    Account {
        id: None,
        name: name.to_string(),
        kind: kind.to_string(),
        asset_class: asset_class.map(str::to_string),
        tier: tier.map(str::to_string),
        currency: currency.to_string(),
        provider_id: None,
        business_id: business,
        external_id: None,
        active: true,
    }
}

fn seed_accounts(db: &Db, biz: &Businesses) -> Result<Seeded> {
    let mut balances: Vec<(AccountId, i64, &'static str)> = Vec::new();
    // Create an account, record its current balance for the snapshot pass, return id.
    let mut add = |a: &Account, cents: i64| -> Result<AccountId> {
        let id = db.upsert_account(a)?;
        // currency is a short static set; map to a 'static str for the snapshot list.
        let cur: &'static str = if a.currency == "USD" { "USD" } else { "GB" };
        balances.push((id, cents, cur));
        Ok(id)
    };

    // --- Tier 2: the liquid permanent-portfolio sleeves. Deliberately skewed so
    //     equity sits above the 44% upper band → the Invest view shows drift. ---
    add(
        &account(
            "Brokerage — VTI",
            "brokerage",
            Some("equity"),
            Some("t2"),
            "USD",
            None,
        ),
        52_000_00,
    )?;
    add(
        &account(
            "Brokerage — TLT",
            "brokerage",
            Some("long_treasuries"),
            Some("t2"),
            "USD",
            None,
        ),
        28_000_00,
    )?;
    add(
        &account(
            "TreasuryDirect + SGOV",
            "asset",
            Some("cash"),
            Some("t2"),
            "USD",
            None,
        ),
        32_000_00,
    )?;

    // --- Tier 1: hold-forever antifragile backbone (info-only; no drift). ---
    add(
        &account(
            "Gold (10 oz, vault)",
            "asset",
            Some("gold"),
            Some("t1"),
            "USD",
            None,
        ),
        26_500_00,
    )?;
    add(
        &account(
            "Silver (200 oz)",
            "asset",
            Some("silver"),
            Some("t1"),
            "USD",
            None,
        ),
        6_000_00,
    )?;
    add(
        &account(
            "Land parcel",
            "asset",
            Some("land"),
            Some("t1"),
            "USD",
            None,
        ),
        85_000_00,
    )?;
    add(
        &account(
            "Monero (cold wallet)",
            "asset",
            Some("monero"),
            Some("t1"),
            "USD",
            None,
        ),
        9_000_00,
    )?;

    // --- Tier 3: operating capital (outside PP math). ---
    add(
        &account(
            "Emergency fund (SGOV)",
            "asset",
            Some("cash"),
            Some("t3"),
            "USD",
            None,
        ),
        42_000_00,
    )?;
    add(
        &account(
            "SaaS working capital",
            "asset",
            Some("cash"),
            Some("t3"),
            "USD",
            None,
        ),
        25_000_00,
    )?;

    // --- Everyday banking (untiered). ---
    let checking = add(
        &account("Checking", "asset", None, None, "USD", None),
        11_800_00,
    )?;
    add(
        &account("Savings", "asset", None, None, "USD", None),
        26_500_00,
    )?;

    // --- Per-business cash. ---
    let acme_cash = add(
        &account(
            "Acme — Mercury checking",
            "asset",
            None,
            None,
            "USD",
            Some(biz.acme),
        ),
        19_400_00,
    )?;
    let globex_cash = add(
        &account(
            "Globex — Mercury checking",
            "asset",
            None,
            None,
            "USD",
            Some(biz.globex),
        ),
        7_250_00,
    )?;

    // --- Debt (kind="debt": subtracted from net worth). Stored as amount owed. ---
    add(
        &account("Visa card", "debt", None, None, "USD", None),
        2_350_00,
    )?;
    add(
        &account("Auto loan", "debt", None, None, "USD", None),
        13_900_00,
    )?;

    // --- Vultr egress: a non-USD ("GB") subscription account, excluded from net
    //     worth, that pairs with the bandwidth.high alert below. ---
    add(
        &account(
            "Vultr — globex (egress GB)",
            "subscription",
            None,
            None,
            "GB",
            Some(biz.globex),
        ),
        318,
    )?;

    Ok(Seeded {
        checking,
        acme_cash,
        globex_cash,
        balances,
    })
}

fn seed_balances(db: &Db, seeded: &Seeded) -> Result<()> {
    let now = now_secs();

    // Current balance per account → the "latest" snapshot the Accounts/Invest
    // views read.
    for (aid, cents, _) in &seeded.balances {
        db.insert_snapshot(*aid, Cents(*cents), "demo")?;
    }

    // Backdated monthly snapshots for the net-worth trend chart. USD only (the
    // series sums USD accounts). Scale every account by the same monthly factor
    // so net worth rises smoothly toward today — integer math, no floats.
    db.with_conn(|c| {
        for (aid, cents, cur) in &seeded.balances {
            if *cur != "USD" {
                continue;
            }
            for m in 1..=HISTORY_MONTHS {
                let taken = now - m * MONTH;
                let pct = 100 - 4 * m; // 1mo ago: 96% … 6mo ago: 76%
                let val = cents.saturating_mul(pct) / 100;
                c.execute(
                    "INSERT INTO balance_snapshots (account_id, taken_at, amount_cents, source)
                     VALUES (?1, ?2, ?3, 'demo')",
                    params![aid.0, taken, val],
                )?;
            }
        }
        Ok(())
    })?;

    // A couple of spot prices so any commodity display has data.
    db.insert_price_snapshot("XAU", Cents(2_650_00), "demo")?;
    db.insert_price_snapshot("XMR", Cents(162_00), "demo")?;
    Ok(())
}

/// Map a demo payee to a spend category, so the Charts "top categories" panel
/// shows named buckets instead of one "uncategorized" lump.
fn category_for(payee: &str) -> &'static str {
    match payee {
        "RENT" => "housing",
        "NETFLIX" | "SPOTIFY USA" | "ADOBE CREATIVE CLOUD" => "subscriptions",
        "AWS" | "OPENAI API" | "GITHUB" => "software",
        "WHOLE FOODS" => "groceries",
        "SHELL" => "fuel",
        "LOCAL CAFE" => "dining",
        "DELTA AIR LINES" => "travel",
        p if p.starts_with("STRIPE PAYOUT") => "revenue",
        _ => "other",
    }
}

/// Post `count` monthly transactions of `payee` ending this month and walking
/// back, so the recurring detector (≥3 monthly occurrences) picks outflows up.
/// The income/expense tag and the spend category are derived from sign + payee.
fn monthly(
    db: &Db,
    account: AccountId,
    business: Option<BusinessId>,
    payee: &str,
    amount_cents: i64,
    count: i64,
) -> Result<()> {
    let now = now_secs();
    let tag = if amount_cents >= 0 {
        "income"
    } else {
        "expense"
    };
    for i in 0..count {
        db.upsert_transaction(&Transaction {
            id: None,
            account_id: account,
            posted_at: now - i * MONTH,
            amount_cents: Cents(amount_cents),
            currency: "USD".to_string(),
            description: Some(payee.to_string()),
            category: Some(category_for(payee).to_string()),
            tag: Some(tag.to_string()),
            business_id: business,
            external_id: Some(format!("demo-{payee}-{i}")),
            source: "demo".to_string(),
        })?;
    }
    Ok(())
}

fn seed_transactions(db: &Db, seeded: &Seeded, biz: &Businesses) -> Result<()> {
    let n = HISTORY_MONTHS;
    let checking = seeded.checking;

    // Recurring personal bills (outflows → detected as monthly series).
    monthly(db, checking, None, "NETFLIX", -15_99, n)?;
    monthly(db, checking, None, "SPOTIFY USA", -11_99, n)?;
    monthly(db, checking, None, "ADOBE CREATIVE CLOUD", -54_99, n)?;
    monthly(db, checking, None, "RENT", -2_000_00, n)?;

    // Recurring business costs, tagged to a business for per-business P&L.
    monthly(db, seeded.acme_cash, Some(biz.acme), "AWS", -340_00, n)?;
    monthly(
        db,
        seeded.acme_cash,
        Some(biz.acme),
        "OPENAI API",
        -120_00,
        n,
    )?;
    monthly(
        db,
        seeded.globex_cash,
        Some(biz.globex),
        "GITHUB",
        -21_00,
        n,
    )?;

    // Recurring income (positive → not a "bill", but drives cash flow + P&L).
    monthly(
        db,
        seeded.acme_cash,
        Some(biz.acme),
        "STRIPE PAYOUT ACME",
        8_500_00,
        n,
    )?;
    monthly(
        db,
        seeded.globex_cash,
        Some(biz.globex),
        "STRIPE PAYOUT GLOBEX",
        3_200_00,
        n,
    )?;

    // A few non-recurring one-offs for texture (distinct payees → no series).
    let now = now_secs();
    let oneoffs = [
        (-86_40, "WHOLE FOODS", 3),
        (-44_10, "SHELL", 12),
        (-58_20, "LOCAL CAFE", 21),
        (-1_240_00, "DELTA AIR LINES", 40),
    ];
    for (amount, payee, days_ago) in oneoffs {
        db.upsert_transaction(&Transaction {
            id: None,
            account_id: checking,
            posted_at: now - days_ago * DAY,
            amount_cents: Cents(amount),
            currency: "USD".to_string(),
            description: Some(payee.to_string()),
            category: Some(category_for(payee).to_string()),
            tag: Some("expense".to_string()),
            business_id: None,
            external_id: Some(format!("demo-oneoff-{payee}")),
            source: "demo".to_string(),
        })?;
    }
    Ok(())
}

fn seed_subscriptions(db: &Db, biz: &Businesses) -> Result<()> {
    let now = now_secs();
    let next = now + 12 * DAY;
    // (name, amount_cents, cadence, business)
    db.upsert_subscription(
        "Rent",
        "recurring",
        Some(Cents(-2_000_00)),
        Some("monthly"),
        Some(next),
        None,
        true,
    )?;
    db.upsert_subscription(
        "Health insurance",
        "recurring",
        Some(Cents(-420_00)),
        Some("monthly"),
        Some(next),
        None,
        true,
    )?;
    db.upsert_subscription(
        "SuperGrok",
        "recurring",
        Some(Cents(-30_00)),
        Some("monthly"),
        Some(next),
        None,
        true,
    )?;
    db.upsert_subscription(
        "Domain renewal (acme.dev)",
        "recurring",
        Some(Cents(-40_00)),
        Some("yearly"),
        Some(now + 90 * DAY),
        Some(biz.acme),
        true,
    )?;
    Ok(())
}

fn seed_alerts(db: &Db) -> Result<()> {
    // Operator-defined rules across the supported kinds.
    let band =
        db.upsert_alert_rule_by_name("pp-band-breach", r#"{"kind":"pp.band_breach"}"#, "xmpp")?;
    let egress = db.upsert_alert_rule_by_name(
        "egress-globex",
        r#"{"kind":"bandwidth.high","account":"Vultr — globex (egress GB)","max_gb":300}"#,
        "xmpp",
    )?;
    db.upsert_alert_rule_by_name(
        "low-checking",
        r#"{"kind":"balance.low","account":"Checking","min_cents":1000000}"#,
        "xmpp",
    )?;
    db.upsert_alert_rule_by_name(
        "rent-reminder",
        r#"{"kind":"reminder","day_of_month":1,"hour_ast":9,"minute_ast":0,"message":"Pay rent"}"#,
        "xmpp",
    )?;

    // A couple of pending (undelivered) firings so the Alerts queue isn't empty.
    db.insert_alert_history(band, r#"{"sleeve":"equity","pct":46,"upper":44}"#, false)?;
    db.insert_alert_history(
        egress,
        r#"{"account":"Vultr — globex (egress GB)","gb":318,"max_gb":300}"#,
        false,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recurring::{self, MIN_OCCURRENCES};

    #[test]
    fn seed_populates_a_coherent_demo_db() {
        let db = Db::open_memory().unwrap();
        seed(&db).unwrap();

        // Accounts across every tier exist.
        let accts = db.list_active_accounts().unwrap();
        assert!(accts.iter().any(|a| a.tier.as_deref() == Some("t1")));
        assert!(accts.iter().any(|a| a.tier.as_deref() == Some("t2")));
        assert!(accts.iter().any(|a| a.tier.as_deref() == Some("t3")));

        // The net-worth trend has multiple points and rises toward today.
        let series = db.networth_series(None).unwrap();
        assert!(series.len() >= 2, "expected a multi-point trend");
        assert!(
            series.last().unwrap().1.as_i64() > series.first().unwrap().1.as_i64(),
            "net worth should rise toward the present"
        );
        assert!(series.last().unwrap().1.as_i64() > 0);

        // Recurring detection finds the seeded monthly bills.
        let detected = recurring::labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
        assert!(
            detected.len() >= 4,
            "expected several detected bills, got {}",
            detected.len()
        );
        assert!(detected.iter().any(|s| s.series.payee == "netflix"));

        // The alert queue has pending (undelivered) firings.
        let pending = db.list_recent_alerts(50, false).unwrap();
        assert!(pending.len() >= 2, "expected pending demo alerts");
    }
}
