//! Recurring-series detection over transaction history.
//!
//! A subscription, bill, or recurring debt payment is just a *payee that
//! repeats on a regular cadence*. Rather than scrape each biller's portal
//! (fragile, OTP-gated, forbidden JS), we derive the series straight from the
//! transactions we already pull (Plaid `merchant_name`, Mercury, manual). We do
//! not know the *next* amount until it posts — so we report the observed history
//! (last / avg / min / max) and a predicted next date from the cadence instead.
//!
//! The detector [`detect`] is pure: it takes a projection of `transactions` and
//! returns the series — no DB, no I/O. [`labeled_series`] is the DB-backed
//! convenience that feeds the detector from the `transactions` table and joins
//! the operator's persisted labels (the `recurring_series` table); it is shared
//! by the RPC handler, the calendar engine, and the report engine. Detection is
//! heuristic; thresholds are the documented `const`s below.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;
use crate::money::Cents;
use crate::rpc::LabeledSeries;

/// A series must show at least this many charges before we call it recurring —
/// two points define a line but not a rhythm.
pub const MIN_OCCURRENCES: usize = 3;

const DAY_SECS: i64 = 86_400;

/// A minimal projection of a `transactions` row fed to the detector. The caller
/// decides which rows to pass (typically outflows: `amount_cents < 0`).
#[derive(Clone, Debug)]
pub struct Txn {
    pub posted_at: i64,
    pub amount_cents: i64,
    pub description: String,
}

/// How often a series repeats. Bands are deliberately gapped (e.g. nothing
/// between monthly and quarterly) so an irregular payee fails to classify rather
/// than getting force-fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    Weekly,
    Biweekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl Cadence {
    /// Ordered ascending by period so median-gap matching picks the tightest fit.
    const ALL: [Cadence; 5] = [
        Cadence::Weekly,
        Cadence::Biweekly,
        Cadence::Monthly,
        Cadence::Quarterly,
        Cadence::Yearly,
    ];

    /// Inclusive acceptable gap band, in days, for the median inter-charge gap.
    fn band(self) -> (f64, f64) {
        match self {
            Cadence::Weekly => (5.0, 9.0),
            Cadence::Biweekly => (12.0, 16.0),
            Cadence::Monthly => (26.0, 35.0),
            Cadence::Quarterly => (80.0, 100.0),
            Cadence::Yearly => (350.0, 380.0),
        }
    }

    /// Lowercase name, for display in reports/calendar.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Cadence::Weekly => "weekly",
            Cadence::Biweekly => "biweekly",
            Cadence::Monthly => "monthly",
            Cadence::Quarterly => "quarterly",
            Cadence::Yearly => "yearly",
        }
    }

    /// Canonical period in days, used to project the next expected charge date.
    fn period_days(self) -> i64 {
        match self {
            Cadence::Weekly => 7,
            Cadence::Biweekly => 14,
            Cadence::Monthly => 30,
            Cadence::Quarterly => 91,
            Cadence::Yearly => 365,
        }
    }
}

/// A detected recurring payee with its observed history and a predicted next
/// charge date. Money fields are observed, not promised — the next charge may
/// differ (variable utility bill, price change).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Series {
    /// Normalized grouping key (lowercased, digits stripped).
    pub payee: String,
    /// A representative original description (the most recent charge's).
    pub display: String,
    pub cadence: Cadence,
    pub count: usize,
    pub first_seen: i64,
    pub last_seen: i64,
    /// Most recent charge amount (chronologically last, not largest).
    pub last_amount: Cents,
    pub min_amount: Cents,
    pub max_amount: Cents,
    pub avg_amount: Cents,
    /// `last_seen + cadence.period_days()`. A forecast, not a commitment.
    pub predicted_next: i64,
}

/// What a confirmed series *is*, set by the operator. Detection can't know this —
/// a monthly $15 charge could be a subscription, a financed-purchase bill, or a
/// recurring debt payment. Persisted (keyed by [`normalize_payee`]) in the
/// `recurring_series` table; the detector never sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesLabel {
    Sub,
    Bill,
    Debt,
    /// Operator marked this detected payee as NOT recurring (a false positive).
    /// An active `Ignore` label suppresses the series from `labeled_series`
    /// entirely (see there) — it never reaches reports/calendar/list.
    Ignore,
}

impl SeriesLabel {
    /// Parse the stored/CLI string form. Rejects anything else so a bad label
    /// can't reach the DB.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sub" => Some(Self::Sub),
            "bill" => Some(Self::Bill),
            "debt" => Some(Self::Debt),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sub => "sub",
            Self::Bill => "bill",
            Self::Debt => "debt",
            Self::Ignore => "ignore",
        }
    }
}

/// Normalize a transaction description into a grouping key: lowercase, drop all
/// digits (store numbers, dates, reference ids), and collapse every other
/// non-alphanumeric run to a single space. "SQ *COFFEE 1234" and
/// "SQ *COFFEE 5678" both become "sq coffee". Heuristic — two genuinely
/// different payees that differ only in digits would merge (rare).
///
/// Public so the confirm path can re-derive the same key the detector uses
/// (it's idempotent on already-normalized input, so re-normalizing a `payee`
/// returned by [`detect`] is a no-op).
#[must_use]
pub fn normalize_payee(desc: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for ch in desc.chars() {
        if ch.is_ascii_digit() {
            continue;
        }
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim().to_string()
}

/// Curated, build-time table of well-known recurring merchants → the label they
/// almost always carry. This is the "inbuilt context" that lets arca pre-fill a
/// guess for a freshly-detected payee. It is **static data, not a model and not a
/// network call** — arca stays deterministic by design (no LLM in any command
/// path). Suggestions are advisory only: the operator confirms each one (a single
/// keypress in the Bills triage table), so a wrong or missing guess costs nothing.
///
/// Needles match as a substring of the *normalized* payee key (see
/// [`normalize_payee`]: lowercased, digits stripped, punctuation collapsed to
/// single spaces). Entries must therefore be lowercase, digit-free, and
/// distinctive — no single letters, nothing that would smear across unrelated
/// merchants. First match wins, so list a more specific needle before a broader
/// one that would also hit. Deliberately no bare `amazon` (that's shopping, not a
/// sub); `amazon prime` is specific enough to be a subscription.
const KNOWN_MERCHANTS: &[(&str, SeriesLabel)] = &[
    // streaming / media subscriptions
    ("netflix", SeriesLabel::Sub),
    ("spotify", SeriesLabel::Sub),
    ("hulu", SeriesLabel::Sub),
    ("disney", SeriesLabel::Sub),
    ("hbo", SeriesLabel::Sub),
    ("peacock", SeriesLabel::Sub),
    ("paramount", SeriesLabel::Sub),
    ("youtube", SeriesLabel::Sub),
    ("apple com bill", SeriesLabel::Sub),
    ("itunes", SeriesLabel::Sub),
    ("amazon prime", SeriesLabel::Sub),
    ("audible", SeriesLabel::Sub),
    ("twitch", SeriesLabel::Sub),
    ("patreon", SeriesLabel::Sub),
    // software / SaaS / cloud subscriptions
    ("adobe", SeriesLabel::Sub),
    ("microsoft", SeriesLabel::Sub),
    ("dropbox", SeriesLabel::Sub),
    ("github", SeriesLabel::Sub),
    ("openai", SeriesLabel::Sub),
    ("chatgpt", SeriesLabel::Sub),
    ("anthropic", SeriesLabel::Sub),
    ("supergrok", SeriesLabel::Sub),
    ("notion", SeriesLabel::Sub),
    ("vercel", SeriesLabel::Sub),
    ("digitalocean", SeriesLabel::Sub),
    ("linode", SeriesLabel::Sub),
    ("cloudflare", SeriesLabel::Sub),
    ("namecheap", SeriesLabel::Sub),
    ("proton", SeriesLabel::Sub),
    // utilities / insurance / telecom — recurring bills
    ("volt", SeriesLabel::Bill), // electric utility
    ("power", SeriesLabel::Bill),
    ("grid", SeriesLabel::Bill),    // electric distribution
    ("aqua", SeriesLabel::Bill),    // water authority
    ("liberty", SeriesLabel::Bill), // cable / internet
    ("telco", SeriesLabel::Bill),   // telecom
    ("t mobile", SeriesLabel::Bill),
    ("verizon", SeriesLabel::Bill),
    ("geico", SeriesLabel::Bill),
    ("progressive", SeriesLabel::Bill),
    ("state farm", SeriesLabel::Bill),
    ("allstate", SeriesLabel::Bill),
];

/// Suggest a label for an as-yet-unconfirmed payee from [`KNOWN_MERCHANTS`].
/// `payee` is normalized first (idempotent on a key returned by [`detect`]), then
/// substring-matched; first hit wins. `None` means "no inbuilt guess — the
/// operator picks." Advisory only: the caller surfaces this as a hint and the
/// operator confirms it; nothing is auto-applied.
#[must_use]
pub fn suggest_label(payee: &str) -> Option<SeriesLabel> {
    let key = normalize_payee(payee);
    if key.is_empty() {
        return None;
    }
    KNOWN_MERCHANTS
        .iter()
        .find(|(needle, _)| key.contains(needle))
        .map(|(_, label)| *label)
}

/// Classify a run of inter-charge gaps (in days) into a cadence, or `None` if it
/// doesn't fit any band. Requires the median gap to land in a band AND a majority
/// of individual gaps to fall in that same band, so one regular streak plus a lot
/// of noise won't qualify.
fn classify(gaps: &[f64]) -> Option<Cadence> {
    if gaps.is_empty() {
        return None;
    }
    let mut sorted = gaps.to_vec();
    sorted.sort_by(f64::total_cmp); // gaps are finite (from unix timestamps): no NaN
    let median = sorted[sorted.len() / 2];
    for cad in Cadence::ALL {
        let (lo, hi) = cad.band();
        if median >= lo && median <= hi {
            let in_band = gaps.iter().filter(|g| **g >= lo && **g <= hi).count();
            return (in_band * 2 >= gaps.len()).then_some(cad);
        }
    }
    None
}

/// Detect recurring series in a set of transactions. `min_occurrences` is the
/// floor for calling a payee recurring (callers typically pass
/// [`MIN_OCCURRENCES`]). Output is sorted by `predicted_next` ascending
/// (soonest-due first), which is the order a "what's coming up" view wants.
#[must_use]
pub fn detect(txns: &[Txn], min_occurrences: usize) -> Vec<Series> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<String, Vec<&Txn>> = BTreeMap::new();
    for t in txns {
        let key = normalize_payee(&t.description);
        if key.is_empty() {
            continue;
        }
        groups.entry(key).or_default().push(t);
    }

    let mut out = Vec::new();
    for (key, mut items) in groups {
        if items.len() < min_occurrences.max(2) {
            continue;
        }
        items.sort_by_key(|t| t.posted_at);
        let gaps: Vec<f64> = items
            .windows(2)
            .map(|w| (w[1].posted_at - w[0].posted_at) as f64 / DAY_SECS as f64)
            .collect();
        let Some(cadence) = classify(&gaps) else {
            continue;
        };

        let count = items.len();
        let first = items[0];
        let last = items[count - 1];
        let amounts: Vec<i64> = items.iter().map(|t| t.amount_cents).collect();
        let min = *amounts
            .iter()
            .min()
            .expect("non-empty: count >= MIN_OCCURRENCES");
        let max = *amounts
            .iter()
            .max()
            .expect("non-empty: count >= MIN_OCCURRENCES");
        let avg = amounts.iter().sum::<i64>() / count as i64;

        out.push(Series {
            payee: key,
            display: last.description.clone(),
            cadence,
            count,
            first_seen: first.posted_at,
            last_seen: last.posted_at,
            last_amount: Cents(last.amount_cents),
            min_amount: Cents(min),
            max_amount: Cents(max),
            avg_amount: Cents(avg),
            predicted_next: last.posted_at + cadence.period_days() * DAY_SECS,
        });
    }

    out.sort_by_key(|s| s.predicted_next);
    out
}

/// Detect series from the `transactions` table (outflows only) and join the
/// operator's active labels by payee key. The single source of truth for
/// "recurring series with their labels" — used by `recurring.list`, the calendar
/// engine, and the monthly report so detection logic lives in exactly one place.
///
/// `since` bounds the history window; `min_occurrences` is the detector floor.
pub fn labeled_series(
    db: &Db,
    since: Option<i64>,
    min_occurrences: usize,
) -> Result<Vec<LabeledSeries>> {
    let (detected, labels) = detect_and_labels(db, since, min_occurrences)?;
    Ok(detected
        .into_iter()
        .filter_map(|s| {
            let lbl = labels.get(&s.payee);
            // An active `ignore` label means the operator marked this a false
            // positive — drop it entirely so it never reaches the list, the
            // calendar, or the report. (`recurring_labels()` already filters to
            // active rows, so dismissing the ignore label resurfaces the series.)
            if lbl.and_then(|l| l.label.as_deref()) == Some(SeriesLabel::Ignore.as_str()) {
                return None;
            }
            Some(join_label(s, lbl))
        })
        .collect())
}

/// The mirror of [`labeled_series`]: detect series and return ONLY the ones the
/// operator has actively ignored (the rows `labeled_series` drops). Each carries
/// `label = "ignore"`. Used by the `recurring.list` handler when the request sets
/// `include_ignored`, so a client (the TUI) can show a hidden series and offer to
/// restore it. Same detection thresholds as `labeled_series`.
pub fn ignored_series(
    db: &Db,
    since: Option<i64>,
    min_occurrences: usize,
) -> Result<Vec<LabeledSeries>> {
    let (detected, labels) = detect_and_labels(db, since, min_occurrences)?;
    Ok(detected
        .into_iter()
        .filter_map(|s| {
            let lbl = labels.get(&s.payee);
            if lbl.and_then(|l| l.label.as_deref()) != Some(SeriesLabel::Ignore.as_str()) {
                return None; // keep only the actively-ignored ones
            }
            Some(join_label(s, lbl))
        })
        .collect())
}

/// Shared by [`labeled_series`] / [`ignored_series`]: pull outflow charges since
/// `since`, detect the series, and load the operator's persisted (active) labels
/// keyed by payee.
fn detect_and_labels(
    db: &Db,
    since: Option<i64>,
    min_occurrences: usize,
) -> Result<(Vec<Series>, HashMap<String, crate::db::RecurringLabel>)> {
    // Detection needs every occurrence, not a page: LIMIT -1 = unbounded.
    let charges: Vec<Txn> = db
        .list_transactions(since, None, -1)?
        .into_iter()
        .filter(|t| t.amount_cents.as_i64() < 0) // outflows only
        .map(|t| Txn {
            posted_at: t.posted_at,
            amount_cents: t.amount_cents.as_i64(),
            description: t.description.unwrap_or_default(),
        })
        .collect();
    let detected = detect(&charges, min_occurrences);
    let labels = db
        .recurring_labels()?
        .into_iter()
        .map(|l| (l.match_key.clone(), l))
        .collect();
    Ok((detected, labels))
}

/// Build a [`LabeledSeries`] by joining a detected `series` with its persisted
/// label row (if any).
fn join_label(series: Series, lbl: Option<&crate::db::RecurringLabel>) -> LabeledSeries {
    LabeledSeries {
        label: lbl.and_then(|l| l.label.clone()),
        display_name: lbl.and_then(|l| l.display_name.clone()),
        confirmed_at: lbl.map(|l| l.confirmed_at),
        series,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txn(day: i64, cents: i64, desc: &str) -> Txn {
        Txn {
            posted_at: day * DAY_SECS,
            amount_cents: cents,
            description: desc.to_string(),
        }
    }

    #[test]
    fn normalize_strips_digits_and_punctuation() {
        assert_eq!(normalize_payee("SQ *COFFEE 1234"), "sq coffee");
        assert_eq!(normalize_payee("SQ *COFFEE 5678"), "sq coffee");
        assert_eq!(normalize_payee("Netflix.com"), "netflix com");
        assert_eq!(normalize_payee("  AT&T  "), "at t");
        assert_eq!(normalize_payee("12345"), ""); // all digits -> empty -> skipped
    }

    #[test]
    fn detects_monthly_subscription() {
        let txns = vec![
            txn(0, -1599, "Netflix"),
            txn(30, -1599, "Netflix"),
            txn(61, -1599, "Netflix"),
            txn(91, -1599, "Netflix"),
        ];
        let series = detect(&txns, MIN_OCCURRENCES);
        assert_eq!(series.len(), 1);
        let s = &series[0];
        assert_eq!(s.payee, "netflix");
        assert_eq!(s.cadence, Cadence::Monthly);
        assert_eq!(s.count, 4);
        assert_eq!(s.last_amount, Cents(-1599));
        assert_eq!(s.predicted_next, 91 * DAY_SECS + 30 * DAY_SECS);
    }

    #[test]
    fn detects_variable_amount_utility_bill() {
        // Monthly, but the amount drifts — we still detect it and report the spread.
        let txns = vec![
            txn(0, -10000, "Grid Energy"),
            txn(31, -12500, "Grid Energy"),
            txn(59, -9500, "Grid Energy"),
            txn(90, -11000, "Grid Energy"),
        ];
        let series = detect(&txns, MIN_OCCURRENCES);
        assert_eq!(series.len(), 1);
        let s = &series[0];
        assert_eq!(s.cadence, Cadence::Monthly);
        assert_eq!(s.min_amount, Cents(-12500));
        assert_eq!(s.max_amount, Cents(-9500));
        assert_eq!(s.last_amount, Cents(-11000)); // chronological last
        assert_eq!(s.avg_amount, Cents((-10000 - 12500 - 9500 - 11000) / 4));
    }

    #[test]
    fn detects_weekly_and_yearly() {
        let weekly: Vec<Txn> = (0..5).map(|i| txn(i * 7, -2000, "Gym")).collect();
        assert_eq!(detect(&weekly, MIN_OCCURRENCES)[0].cadence, Cadence::Weekly);

        let yearly = vec![
            txn(0, -9900, "Domain Renewal"),
            txn(365, -9900, "Domain Renewal"),
            txn(731, -9900, "Domain Renewal"),
        ];
        assert_eq!(detect(&yearly, MIN_OCCURRENCES)[0].cadence, Cadence::Yearly);
    }

    #[test]
    fn ignores_irregular_payee() {
        // Random gaps that fit no band -> not a series.
        let txns = vec![
            txn(0, -500, "Corner Store"),
            txn(3, -1200, "Corner Store"),
            txn(44, -800, "Corner Store"),
            txn(51, -300, "Corner Store"),
        ];
        assert!(detect(&txns, MIN_OCCURRENCES).is_empty());
    }

    #[test]
    fn ignores_too_few_occurrences() {
        let txns = vec![txn(0, -1599, "Spotify"), txn(30, -1599, "Spotify")];
        assert!(detect(&txns, MIN_OCCURRENCES).is_empty());
    }

    #[test]
    fn groups_payee_across_noisy_descriptors() {
        // Store/reference numbers vary per charge; stripping digits collapses them
        // to one payee. (Plaid's cleaned `merchant_name` is the happy path; this
        // covers the messier `name` fallback.)
        let txns = vec![
            txn(0, -1500, "WALMART #1234 SAN JUAN"),
            txn(30, -1500, "WALMART #5678 SAN JUAN"),
            txn(60, -1500, "WALMART #9012 SAN JUAN"),
        ];
        let series = detect(&txns, MIN_OCCURRENCES);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].payee, "walmart san juan");
        assert_eq!(series[0].count, 3);
    }

    #[test]
    fn labeled_series_joins_persisted_label() {
        use crate::db::Db;
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
        // Four monthly Netflix outflows -> one detected series.
        for (i, day) in [0_i64, 30, 61, 91].iter().enumerate() {
            db.with_conn(|c| {
                c.execute(
                    "INSERT INTO transactions
                       (account_id, posted_at, amount_cents, currency, description, source, external_id)
                     VALUES (1, ?1, -1599, 'USD', 'Netflix', 'manual', ?2)",
                    rusqlite::params![day * 86_400, format!("nf{i}")],
                )
                .map_err(Into::into)
            })
            .unwrap();
        }
        // Unlabeled: detected but label None.
        let series = labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
        let nf = series
            .iter()
            .find(|ls| ls.series.payee == "netflix")
            .unwrap();
        assert_eq!(nf.label, None);

        // After confirming, the label joins; dismissed labels drop out.
        db.upsert_recurring_label("netflix", Some("sub"), Some("Netflix"), None, true, 7)
            .unwrap();
        let series = labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
        let nf = series
            .iter()
            .find(|ls| ls.series.payee == "netflix")
            .unwrap();
        assert_eq!(nf.label.as_deref(), Some("sub"));
        assert_eq!(nf.display_name.as_deref(), Some("Netflix"));
        assert_eq!(nf.confirmed_at, Some(7));
    }

    #[test]
    fn rename_without_label_preserves_label() {
        use crate::db::Db;
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
        for (i, day) in [0_i64, 30, 61, 91].iter().enumerate() {
            db.with_conn(|c| {
                c.execute(
                    "INSERT INTO transactions
                       (account_id, posted_at, amount_cents, currency, description, source, external_id)
                     VALUES (1, ?1, -1599, 'USD', 'Netflix', 'manual', ?2)",
                    rusqlite::params![day * 86_400, format!("nf{i}")],
                )
                .map_err(Into::into)
            })
            .unwrap();
        }
        // Rename a series that has no label yet: row created, label stays NULL.
        db.upsert_recurring_label("netflix", None, Some("Netflix HD"), None, true, 5)
            .unwrap();
        let nf = labeled_series(&db, None, MIN_OCCURRENCES)
            .unwrap()
            .into_iter()
            .find(|ls| ls.series.payee == "netflix")
            .unwrap();
        assert_eq!(nf.label, None);
        assert_eq!(nf.display_name.as_deref(), Some("Netflix HD"));

        // Now label it — the rename must survive (COALESCE preserves display_name).
        db.upsert_recurring_label("netflix", Some("sub"), None, None, true, 6)
            .unwrap();
        let nf = labeled_series(&db, None, MIN_OCCURRENCES)
            .unwrap()
            .into_iter()
            .find(|ls| ls.series.payee == "netflix")
            .unwrap();
        assert_eq!(nf.label.as_deref(), Some("sub"));
        assert_eq!(nf.display_name.as_deref(), Some("Netflix HD"));

        // And a later rename must NOT wipe the label.
        db.upsert_recurring_label("netflix", None, Some("Netflix 4K"), None, true, 7)
            .unwrap();
        let nf = labeled_series(&db, None, MIN_OCCURRENCES)
            .unwrap()
            .into_iter()
            .find(|ls| ls.series.payee == "netflix")
            .unwrap();
        assert_eq!(nf.label.as_deref(), Some("sub"));
        assert_eq!(nf.display_name.as_deref(), Some("Netflix 4K"));
    }

    #[test]
    fn suggest_label_recognizes_known_merchants() {
        // Streaming / SaaS → sub; matches through normalize_payee (digits gone,
        // case-folded, punctuation collapsed).
        assert_eq!(suggest_label("Netflix"), Some(SeriesLabel::Sub));
        assert_eq!(suggest_label("SPOTIFY P0H7L9Q1X2"), Some(SeriesLabel::Sub));
        assert_eq!(suggest_label("Amazon Prime"), Some(SeriesLabel::Sub));
        assert_eq!(suggest_label("Amazon Prime Video"), Some(SeriesLabel::Sub));
        assert_eq!(suggest_label("Peacock"), Some(SeriesLabel::Sub));
        // Utilities / telecom → bill.
        assert_eq!(suggest_label("Volt Power"), Some(SeriesLabel::Bill));
        assert_eq!(suggest_label("Aqua"), Some(SeriesLabel::Bill));
        assert_eq!(suggest_label("Grid Energy"), Some(SeriesLabel::Bill));
        // Bare "amazon" (shopping) is deliberately NOT a sub.
        assert_eq!(suggest_label("AMAZON MKTP US"), None);
        // Unknown payee → operator picks; empty/normalizes-empty → None.
        assert_eq!(suggest_label("Corner Gym"), None);
        assert_eq!(suggest_label("Acme Bakery"), None);
        assert_eq!(suggest_label(""), None);
        assert_eq!(suggest_label("000123"), None);
    }

    #[test]
    fn series_label_parses_and_rejects() {
        for s in ["sub", "bill", "debt", "ignore"] {
            assert_eq!(SeriesLabel::parse(s).unwrap().as_str(), s);
        }
        assert!(SeriesLabel::parse("Subscription").is_none());
        assert!(SeriesLabel::parse("").is_none());
        assert!(SeriesLabel::parse("SUB").is_none());
    }

    #[test]
    fn ignore_label_hides_series() {
        use crate::db::Db;
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
        for (i, day) in [0_i64, 30, 61, 91].iter().enumerate() {
            db.with_conn(|c| {
                c.execute(
                    "INSERT INTO transactions
                       (account_id, posted_at, amount_cents, currency, description, source, external_id)
                     VALUES (1, ?1, -1599, 'USD', 'Netflix', 'manual', ?2)",
                    rusqlite::params![day * 86_400, format!("nf{i}")],
                )
                .map_err(Into::into)
            })
            .unwrap();
        }
        // Detected before any label.
        assert!(
            labeled_series(&db, None, MIN_OCCURRENCES)
                .unwrap()
                .iter()
                .any(|ls| ls.series.payee == "netflix")
        );
        // An active `ignore` label removes it from the list entirely.
        db.upsert_recurring_label("netflix", Some("ignore"), None, None, true, 7)
            .unwrap();
        assert!(
            labeled_series(&db, None, MIN_OCCURRENCES)
                .unwrap()
                .iter()
                .all(|ls| ls.series.payee != "netflix")
        );
        // Dismissing the ignore label (active=false) resurfaces it, unlabeled.
        db.upsert_recurring_label("netflix", Some("ignore"), None, None, false, 8)
            .unwrap();
        let after = labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
        let nf = after
            .iter()
            .find(|ls| ls.series.payee == "netflix")
            .unwrap();
        assert_eq!(nf.label, None);
    }

    #[test]
    fn ignored_series_surfaces_only_hidden() {
        use crate::db::Db;
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
        // Two monthly series: netflix and hulu.
        for (payee, ext) in [("Netflix", "nf"), ("Hulu", "hu")] {
            for (i, day) in [0_i64, 30, 61, 91].iter().enumerate() {
                db.with_conn(|c| {
                    c.execute(
                        "INSERT INTO transactions
                           (account_id, posted_at, amount_cents, currency, description, source, external_id)
                         VALUES (1, ?1, -999, 'USD', ?2, 'manual', ?3)",
                        rusqlite::params![day * 86_400, payee, format!("{ext}{i}")],
                    )
                    .map_err(Into::into)
                })
                .unwrap();
            }
        }
        // Ignore netflix only.
        db.upsert_recurring_label("netflix", Some("ignore"), None, None, true, 7)
            .unwrap();
        // labeled_series drops the ignored one; ignored_series carries only it.
        let visible = labeled_series(&db, None, MIN_OCCURRENCES).unwrap();
        assert!(visible.iter().any(|ls| ls.series.payee == "hulu"));
        assert!(visible.iter().all(|ls| ls.series.payee != "netflix"));
        let hidden = ignored_series(&db, None, MIN_OCCURRENCES).unwrap();
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].series.payee, "netflix");
        assert_eq!(hidden[0].label.as_deref(), Some("ignore"));
    }

    #[test]
    fn normalize_payee_is_idempotent_on_detected_key() {
        // A `payee` returned by detect() re-normalizes to itself — so the confirm
        // path can safely re-normalize a key copied from recurring.list.
        for raw in ["SQ *COFFEE 1234", "Netflix.com", "WALMART #99 SAN JUAN"] {
            let once = normalize_payee(raw);
            assert_eq!(normalize_payee(&once), once);
        }
    }

    #[test]
    fn soonest_due_sorted_first() {
        let mut txns: Vec<Txn> = (0..4).map(|i| txn(i * 30, -100, "Monthly A")).collect();
        // Yearly B last-seen day 0 -> predicted_next day 365; Monthly A last day 90 -> 120.
        txns.extend((0..3).map(|i| txn(i * 365, -100, "Yearly B")));
        let series = detect(&txns, MIN_OCCURRENCES);
        assert_eq!(series.len(), 2);
        assert!(series[0].predicted_next < series[1].predicted_next);
        assert_eq!(series[0].payee, "monthly a");
    }
}
