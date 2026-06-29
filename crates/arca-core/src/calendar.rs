//! Hand-rolled VCALENDAR (RFC 5545) builder.
//!
//! We don't take the `icalendar` crate dependency — VCALENDAR is plain text and
//! all we need is `VEVENT` rows for upcoming expenses and subscription renewals.
//! The output is one VCALENDAR per call, intended to be attached to the weekly
//! digest email with MIME type `text/calendar; charset=utf-8; method=PUBLISH`.
//!
//! Apple Mail recognizes the attachment via the `.ics` filename and offers
//! "Add to Calendar"; Apple Calendar imports into the default calendar on
//! double-click.

use std::fmt::Write as _;

use chrono::{TimeZone, Utc};
#[cfg(test)]
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::{Db, PlannedExpense, SubscriptionRecord};
use crate::error::Result;
use crate::money::Cents;
use crate::rpc::LabeledSeries;

const PRODID: &str = "-//arca//finance daemon//EN";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalEvent {
    /// Stable UID — required by RFC 5545. Form: `<source>-<id>@arca`.
    pub uid: String,
    /// Unix seconds UTC of the event start.
    pub start_secs: i64,
    /// Single-line summary that shows in Calendar.
    pub summary: String,
    /// Long-form body. Newlines escaped to `\n` per RFC 5545.
    pub description: Option<String>,
}

/// Convert active planned expenses (due in `[since, until)`) to VEVENT rows.
pub fn events_from_planned(rows: &[PlannedExpense]) -> Vec<CalEvent> {
    rows.iter()
        .map(|p| {
            let amt = Cents(p.amount_cents.0).to_string();
            CalEvent {
                uid: format!("planned-{}@arca", p.id),
                start_secs: p.due_at,
                summary: format!("[arca] {} — {}", p.description, amt),
                description: Some(format!("Planned expense ({}): {}", p.status, amt)),
            }
        })
        .collect()
}

/// Convert active subscriptions with a `next_charge_at` falling in window to VEVENTs.
pub fn events_from_subscriptions(
    rows: &[SubscriptionRecord],
    since: i64,
    until: i64,
) -> Vec<CalEvent> {
    rows.iter()
        .filter_map(|s| {
            let when = s.next_charge_at?;
            if when < since || when >= until {
                return None;
            }
            let amt = s
                .amount_cents
                .map(|c| c.to_string())
                .unwrap_or_else(|| "(usage-based)".into());
            let cadence = s.cadence.as_deref().unwrap_or("?");
            Some(CalEvent {
                uid: format!("sub-{}@arca", s.id),
                start_secs: when,
                summary: format!("[arca] {} renews — {}", s.name, amt),
                description: Some(format!(
                    "Subscription renewal: {} ({}), cadence={cadence}",
                    s.name, s.provider_kind
                )),
            })
        })
        .collect()
}

/// Convert confirmed (labeled) recurring series whose `predicted_next` falls in
/// the window to VEVENTs. Unlabeled detected series are excluded — only series
/// the operator confirmed reach the calendar. The amount is the observed average,
/// flagged `~…est`: we never claim the next charge's exact amount.
pub fn events_from_recurring(series: &[LabeledSeries], since: i64, until: i64) -> Vec<CalEvent> {
    series
        .iter()
        .filter_map(|ls| {
            let label = ls.label.as_deref()?; // confirmed only
            let s = &ls.series;
            if s.predicted_next < since || s.predicted_next >= until {
                return None;
            }
            let name = ls.display_name.as_deref().unwrap_or(&s.display);
            Some(CalEvent {
                uid: format!("recurring-{}@arca", s.payee.replace(' ', "_")),
                start_secs: s.predicted_next,
                summary: format!("[arca] {name} ({label}) ~{} est", s.avg_amount),
                description: Some(format!(
                    "Confirmed recurring {label} ({}). Estimated from history: \
                     avg {}, last {}, over {} charges. Next amount unknown until it posts.",
                    s.cadence.as_str(),
                    s.avg_amount,
                    s.last_amount,
                    s.count
                )),
            })
        })
        .collect()
}

/// Gather events for the upcoming `days` from `as_of_secs`.
pub fn upcoming_events(db: &Db, as_of_secs: i64, days: i64) -> Result<Vec<CalEvent>> {
    let until = as_of_secs + days * 86_400;
    let planned = db.list_planned_expenses_due(as_of_secs, until)?;
    let subs = db.list_active_subscriptions()?;
    let recurring = crate::recurring::labeled_series(db, None, crate::recurring::MIN_OCCURRENCES)?;
    let mut out = events_from_planned(&planned);
    out.extend(events_from_subscriptions(&subs, as_of_secs, until));
    out.extend(events_from_recurring(&recurring, as_of_secs, until));
    out.sort_by_key(|e| e.start_secs);
    Ok(out)
}

/// Render a VCALENDAR with the given events. Always uses CRLF per RFC 5545.
/// Events are emitted as `DTSTART;VALUE=DATE` for all-day; that's the right
/// fit for "this bill is due on day X" without picking an arbitrary time.
#[must_use]
pub fn build_vcalendar(events: &[CalEvent]) -> String {
    let mut s = String::new();
    push_crlf(&mut s, "BEGIN:VCALENDAR");
    push_crlf(&mut s, "VERSION:2.0");
    push_crlf(&mut s, &format!("PRODID:{PRODID}"));
    push_crlf(&mut s, "CALSCALE:GREGORIAN");
    push_crlf(&mut s, "METHOD:PUBLISH");
    let now = ical_utc_stamp(Utc::now().timestamp());
    for e in events {
        push_crlf(&mut s, "BEGIN:VEVENT");
        push_crlf(&mut s, &format!("UID:{}", e.uid));
        push_crlf(&mut s, &format!("DTSTAMP:{now}"));
        push_crlf(
            &mut s,
            &format!("DTSTART;VALUE=DATE:{}", ical_date(e.start_secs)),
        );
        push_crlf(&mut s, &format!("SUMMARY:{}", escape_text(&e.summary)));
        if let Some(d) = &e.description {
            push_crlf(&mut s, &format!("DESCRIPTION:{}", escape_text(d)));
        }
        push_crlf(&mut s, "TRANSP:TRANSPARENT");
        push_crlf(&mut s, "END:VEVENT");
    }
    push_crlf(&mut s, "END:VCALENDAR");
    s
}

/// Convenience used by the daemon engine — combines fetch + render.
pub fn build_weekly_digest(db: &Db, as_of_secs: i64, days: i64) -> Result<String> {
    let events = upcoming_events(db, as_of_secs, days)?;
    Ok(build_vcalendar(&events))
}

fn push_crlf(buf: &mut String, line: &str) {
    let _ = write!(buf, "{line}\r\n");
}

fn ical_date(ts_secs: i64) -> String {
    Utc.timestamp_opt(ts_secs, 0)
        .single()
        .map(|d| d.format("%Y%m%d").to_string())
        .unwrap_or_else(|| "19700101".into())
}

fn ical_utc_stamp(ts_secs: i64) -> String {
    Utc.timestamp_opt(ts_secs, 0)
        .single()
        .map(|d| d.format("%Y%m%dT%H%M%SZ").to_string())
        .unwrap_or_else(|| "19700101T000000Z".into())
}

/// RFC 5545 §3.3.11 TEXT escapes: `\`, `;`, `,`, and newline.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Default ICS filename for the weekly digest: `arca-YYYYMMDD.ics`.
#[must_use]
pub fn weekly_filename(ts_secs: i64) -> String {
    format!("arca-{}.ics", ical_date(ts_secs))
}

/// Bookkeeping helper: how many events would land in the next `days`. Used by
/// the daemon to skip sending empty digests. Must count exactly what
/// [`upcoming_events`] renders — planned expenses + subscriptions **and**
/// confirmed/labeled recurring series — or a week with only recurring-series
/// events would be silently skipped (the rendered set is a strict superset of
/// planned+subscriptions).
pub fn count_upcoming(db: &Db, as_of_secs: i64, days: i64) -> Result<usize> {
    Ok(upcoming_events(db, as_of_secs, days)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_text_handles_specials() {
        assert_eq!(escape_text("a; b, c\\d"), "a\\; b\\, c\\\\d");
        assert_eq!(escape_text("line1\nline2\r"), "line1\\nline2");
    }

    #[test]
    fn vcalendar_envelope_and_event_present() {
        let e = CalEvent {
            uid: "x@arca".into(),
            start_secs: 1_780_000_000,
            summary: "Rent due — $1,500.00".into(),
            description: Some("Apt; building A".into()),
        };
        let s = build_vcalendar(&[e]);
        assert!(s.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(s.ends_with("END:VCALENDAR\r\n"));
        assert!(s.contains("UID:x@arca\r\n"));
        assert!(s.contains("SUMMARY:Rent due — $1\\,500.00\r\n"));
        assert!(s.contains("DESCRIPTION:Apt\\; building A\r\n"));
        assert!(s.contains("DTSTART;VALUE=DATE:"));
        assert!(s.contains("METHOD:PUBLISH"));
    }

    #[test]
    fn empty_events_still_valid_envelope() {
        let s = build_vcalendar(&[]);
        assert!(s.contains("BEGIN:VCALENDAR"));
        assert!(s.contains("END:VCALENDAR"));
        assert!(!s.contains("BEGIN:VEVENT"));
    }

    fn series(payee: &str, label: Option<&str>, predicted_next: i64) -> LabeledSeries {
        use crate::recurring::{Cadence, Series};
        LabeledSeries {
            series: Series {
                payee: payee.into(),
                display: payee.into(),
                cadence: Cadence::Monthly,
                count: 4,
                first_seen: 0,
                last_seen: predicted_next - 30 * 86_400,
                last_amount: Cents(-1599),
                min_amount: Cents(-1599),
                max_amount: Cents(-1599),
                avg_amount: Cents(-1599),
                predicted_next,
            },
            label: label.map(str::to_string),
            display_name: None,
            confirmed_at: label.map(|_| 1),
        }
    }

    #[test]
    fn recurring_events_only_labeled_and_in_window() {
        let since = 1_780_000_000;
        let until = since + 30 * 86_400;
        let s = vec![
            series("netflix", Some("sub"), since + 5 * 86_400), // labeled, in window -> in
            series("peacock", None, since + 6 * 86_400),        // unlabeled -> out
            series("gym", Some("bill"), until + 86_400),        // labeled, past window -> out
            series("loan", Some("debt"), since - 86_400),       // labeled, before window -> out
        ];
        let events = events_from_recurring(&s, since, until);
        assert_eq!(events.len(), 1);
        assert!(events[0].summary.contains("netflix"));
        assert!(events[0].summary.contains("(sub)"));
        assert!(events[0].summary.contains("est"));
        assert_eq!(events[0].uid, "recurring-netflix@arca");
    }

    #[test]
    fn upcoming_picks_subscriptions_and_planned() {
        let db = Db::open_memory().unwrap();
        let card = db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO accounts (name, kind, currency, active, created_at)
                     VALUES ('CC', 'debt', 'USD', 1, 0)",
                    [],
                )
                .map_err(Into::into)
            })
            .unwrap();
        let _ = card;
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO planned_expenses (due_at, amount_cents, description, account_id, status)
                 VALUES (?1, ?2, 'Rent', 1, 'planned')",
                params![1_780_000_000_i64, -150_000_i64],
            )
            .map_err(Into::into)
        })
        .unwrap();
        db.with_conn(|c| {
            c.execute(
                "INSERT INTO subscriptions (name, provider_kind, amount_cents, cadence, next_charge_at, account_id, active)
                 VALUES ('Anthropic', 'usage_based', NULL, NULL, ?1, NULL, 1)",
                params![1_780_500_000_i64],
            )
            .map_err(Into::into)
        })
        .unwrap();
        let events = upcoming_events(&db, 1_779_000_000, 30).unwrap();
        // Both fall in window.
        assert_eq!(events.len(), 2);
        let uids: Vec<&str> = events.iter().map(|e| e.uid.as_str()).collect();
        assert!(uids.iter().any(|u| u.starts_with("planned-")));
        assert!(uids.iter().any(|u| u.starts_with("sub-")));
    }
}
