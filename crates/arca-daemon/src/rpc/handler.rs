//! RPC request dispatcher. One function that maps Request -> Response.

use std::sync::Arc;
use std::time::Instant;

use arca_core::db::{Account, Db, ProviderRow, Transaction};
use arca_core::debt::{debt_view, recurring_obligations};
use arca_core::ids::AccountId;
use arca_core::money::Cents;
use arca_core::pp::{allocation, any_band_breach, backbone, drift};
use arca_core::recurring::{SeriesLabel, normalize_payee};
use arca_core::rpc::{
    AccountLine, AlertRow, AlertRuleRow, AlertsPage, BusinessSnapshot, CategoriesSnapshot,
    CategorySpend, ChartsSnapshot, DebtSnapshot, HealthInfo, KindTotal, MoneySnapshot, MonthFlow,
    PpSnapshot, ProviderStatus, RecurringPage, Request, Response, RpcError, Scope, SubscriptionRow,
    TimePoint, TxListPage, TxRow,
};
use arca_core::time::now_secs;

pub struct State {
    pub db: Arc<Db>,
    pub started_at: Instant,
    pub version: &'static str,
    pub providers: Arc<Vec<crate::providers::registry::LoadedProvider>>,
}

pub fn handle(state: &State, req: Request) -> Response {
    match try_handle(state, req) {
        Ok(r) => r,
        Err(e) => Response::Error(RpcError {
            code: "core".into(),
            msg: e.to_string(),
        }),
    }
}

fn try_handle(state: &State, req: Request) -> arca_core::error::Result<Response> {
    match req {
        Request::Health => Ok(Response::Health(health(state)?)),

        Request::SnapshotMoney => Ok(Response::Money(money(&state.db)?)),

        Request::SnapshotBusiness { tag, scope } => Ok(Response::Business(business(
            &state.db,
            &tag,
            scope.unwrap_or(Scope::Ytd),
        )?)),

        Request::SnapshotPp => {
            let alloc = allocation(&state.db)?;
            let rows = drift(&alloc);
            let band_breach = any_band_breach(&rows);
            let bb = backbone(&state.db)?;
            Ok(Response::Pp(PpSnapshot {
                rows,
                total: alloc.total,
                backbone: bb,
                band_breach,
            }))
        }

        Request::SnapshotDebt { scope } => {
            let v = debt_view(&state.db, scope)?;
            Ok(Response::Debt(DebtSnapshot {
                scope,
                open_balances: v.open,
                scheduled: v.scheduled,
                fixed: recurring_obligations(&state.db)?,
                total_open: v.total_open,
            }))
        }

        Request::TxList { since, tag, limit } => {
            let rows = state
                .db
                .list_transactions(since, tag.as_deref(), limit.unwrap_or(100))?;
            let accounts = state.db.list_active_accounts()?;
            let name_for = |aid: AccountId| {
                accounts
                    .iter()
                    .find(|a| a.id == Some(aid))
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| format!("#{aid}"))
            };
            let mapped = rows
                .into_iter()
                .map(|t| TxRow {
                    id: t.id.map(|i| i.0).unwrap_or(0),
                    posted_at: t.posted_at,
                    account: name_for(t.account_id),
                    amount: t.amount_cents,
                    description: t.description,
                    category: t.category,
                    tag: t.tag,
                })
                .collect();
            Ok(Response::TxList(TxListPage { rows: mapped }))
        }

        Request::RecurringList {
            since,
            min_occurrences,
            include_ignored,
        } => {
            let min = min_occurrences.unwrap_or(arca_core::recurring::MIN_OCCURRENCES);
            let series = arca_core::recurring::labeled_series(&state.db, since, min)?;
            // Ignored series are returned only on request (the TUI's "ignored" zone),
            // in a separate field so `series` stays ignore-free for every other caller.
            let ignored = if include_ignored.unwrap_or(false) {
                arca_core::recurring::ignored_series(&state.db, since, min)?
            } else {
                Vec::new()
            };
            Ok(Response::Recurring(RecurringPage { series, ignored }))
        }

        Request::SnapshotCharts { months } => {
            let months = months.unwrap_or(12).clamp(1, 60);
            let net_worth = state
                .db
                .networth_series(None)?
                .into_iter()
                .map(|(at_secs, amount)| TimePoint { at_secs, amount })
                .collect();
            let (since, until) = months_back_bounds(months);
            let cash_flow = state
                .db
                .cash_flow_monthly(since, until)?
                .into_iter()
                .map(|(label, income, expenses)| MonthFlow {
                    net: income + expenses,
                    label,
                    income,
                    expenses,
                })
                .collect();
            Ok(Response::Charts(ChartsSnapshot {
                net_worth,
                cash_flow,
            }))
        }

        Request::SnapshotCategories { scope, limit } => {
            let scope = scope.unwrap_or(Scope::Month);
            let (since, until) = scope_bounds(scope);
            let raw = state
                .db
                .expenses_by_category(since, until, limit.unwrap_or(20))?;
            let total = raw.iter().map(|(_, c)| *c).sum();
            let rows = raw
                .into_iter()
                .map(|(category, amount)| CategorySpend { category, amount })
                .collect();
            Ok(Response::Categories(CategoriesSnapshot {
                scope,
                since,
                until,
                rows,
                total,
            }))
        }

        Request::ProviderRefresh { .. } => {
            // Async path — handled by handle_refresh in the server dispatcher,
            // never reaches the sync handler.
            unreachable!("ProviderRefresh is routed through handle_refresh");
        }

        Request::ManualUpsertAccount {
            name,
            account_kind,
            asset_class,
            tier,
            currency,
            business_tag,
        } => {
            let business_id = match business_tag {
                Some(t) => Some(state.db.business_by_tag(&t)?.id),
                None => None,
            };
            let a = Account {
                id: None,
                name,
                kind: account_kind,
                asset_class,
                tier,
                currency: currency.unwrap_or_else(|| "USD".into()),
                provider_id: None,
                business_id,
                external_id: None,
                active: true,
            };
            state.db.upsert_account(&a)?;
            Ok(Response::Ack)
        }

        Request::ManualInsertTransaction {
            account_name,
            posted_at,
            amount,
            description,
            tag,
            business_tag,
            external_id,
        } => {
            let aid = account_id_by_name(&state.db, &account_name)?;
            let business_id = match business_tag {
                Some(t) => Some(state.db.business_by_tag(&t)?.id),
                None => None,
            };
            let cents = Cents::from_dollars_str(&amount)?;
            let t = Transaction {
                id: None,
                account_id: aid,
                posted_at,
                amount_cents: cents,
                currency: "USD".into(),
                description,
                category: None,
                tag,
                business_id,
                external_id,
                source: "manual".into(),
            };
            state.db.upsert_transaction(&t)?;
            Ok(Response::Ack)
        }

        Request::ManualSnapshot {
            account_name,
            amount,
        } => {
            let aid = account_id_by_name(&state.db, &account_name)?;
            let cents = Cents::from_dollars_str(&amount)?;
            state.db.insert_snapshot(aid, cents, "manual")?;
            Ok(Response::Ack)
        }

        Request::ManualUpsertProvider {
            provider_kind,
            label,
            config_json,
            secret_ref,
            poll_cadence,
        } => {
            // Reject a typo'd kind now (honest failure) — otherwise the row would
            // load-skip silently and the provider would just never poll.
            if !crate::providers::registry::KNOWN_KINDS.contains(&provider_kind.as_str()) {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: format!(
                        "unknown provider kind '{provider_kind}'; known: {}",
                        crate::providers::registry::KNOWN_KINDS.join(", ")
                    ),
                }));
            }
            let cfg = config_json.unwrap_or_else(|| "{}".into());
            match serde_json::from_str::<serde_json::Value>(&cfg) {
                Ok(v) if v.is_object() => {}
                Ok(_) => {
                    return Ok(Response::Error(RpcError {
                        code: "invalid".into(),
                        msg: "config_json must be a JSON object".into(),
                    }));
                }
                Err(e) => {
                    return Ok(Response::Error(RpcError {
                        code: "invalid".into(),
                        msg: format!("config_json parse: {e}"),
                    }));
                }
            }
            let mut row = ProviderRow::registry_stub(&provider_kind, &label);
            row.config_json = cfg;
            row.secret_ref = secret_ref;
            row.poll_cadence = poll_cadence.unwrap_or_else(|| "daily".into());
            state.db.upsert_provider(&row)?;
            Ok(Response::Ack)
        }

        Request::ManualUpsertBusiness {
            tag,
            display_name,
            active,
        } => {
            let tag = tag.trim();
            if tag.is_empty() {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "business tag must be non-empty".into(),
                }));
            }
            state
                .db
                .upsert_business(tag, display_name.as_deref(), active)?;
            Ok(Response::Ack)
        }

        Request::ManualUpsertSubscription {
            name,
            amount,
            cadence,
            next_charge_at,
            business_tag,
            active,
        } => {
            let name = name.trim();
            if name.is_empty() {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "subscription name must be non-empty".into(),
                }));
            }
            if cadence.trim().is_empty() {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "cadence must be non-empty (monthly|yearly|quarterly|weekly|biweekly)"
                        .into(),
                }));
            }
            let cents = Cents::from_dollars_str(&amount)?;
            // Convention: a recurring obligation is an outflow, stored negative.
            // A positive amount would render as a positive "renewal" in Bills /
            // upcoming / the .ics digest. Reject it rather than silently invert.
            if cents.0 >= 0 {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "subscription amount must be negative (an outflow); e.g. -2000".into(),
                }));
            }
            let business_id = match business_tag {
                Some(t) => Some(state.db.business_by_tag(&t)?.id),
                None => None,
            };
            state.db.upsert_subscription(
                name,
                "recurring",
                Some(cents),
                Some(cadence.trim()),
                Some(next_charge_at),
                business_id,
                active.unwrap_or(true),
            )?;
            Ok(Response::Ack)
        }

        Request::ManualUpdateSubscription {
            name,
            new_name,
            active,
        } => {
            let name = name.trim();
            if name.is_empty() {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "subscription name must be non-empty".into(),
                }));
            }
            // Rename first (it's the identity), then toggle active under the new
            // name. A rename to a blank name is rejected. "Not found" is honest.
            let mut changed = 0usize;
            let effective = match new_name.as_deref().map(str::trim) {
                Some("") => {
                    return Ok(Response::Error(RpcError {
                        code: "invalid".into(),
                        msg: "new subscription name must be non-empty".into(),
                    }));
                }
                Some(nn) => {
                    changed += state.db.rename_subscription(name, nn)?;
                    nn.to_string()
                }
                None => name.to_string(),
            };
            if let Some(a) = active {
                changed += state.db.set_subscription_active(&effective, a)?;
            }
            if changed == 0 {
                return Ok(Response::Error(RpcError {
                    code: "not_found".into(),
                    msg: format!("no subscription named {name:?}"),
                }));
            }
            Ok(Response::Ack)
        }

        Request::AlertPending {
            limit,
            include_delivered,
        } => {
            let rows = state
                .db
                .list_recent_alerts(limit.unwrap_or(50), include_delivered.unwrap_or(false))?;
            let mapped = rows
                .into_iter()
                .map(|h| {
                    let kind = parse_rule_kind(&h.rule_json);
                    let summary = crate::alerts::summarize_alert(kind.as_deref(), &h.payload_json);
                    AlertRow {
                        id: h.id,
                        rule_name: h.rule_name,
                        rule_kind: kind,
                        fired_at: h.fired_at,
                        delivered: h.delivered,
                        summary,
                    }
                })
                .collect();
            // Armed rules, so the view shows what's configured even when nothing
            // has fired (empty history is the common, correct case).
            let rules = state
                .db
                .list_active_alert_rules()?
                .into_iter()
                .map(|r| AlertRuleRow {
                    kind: parse_rule_kind(&r.rule_json),
                    name: r.name,
                    channel: r.channel,
                })
                .collect();
            Ok(Response::Alerts(AlertsPage {
                rows: mapped,
                rules,
            }))
        }

        Request::AlertUpsert {
            name,
            rule_json,
            channel,
            active,
        } => {
            // Reject a rule that names no known predicate — it would be stored
            // but never fire. Honest failure: the operator hears it now.
            if let Err(msg) = crate::alerts::validate_rule_json(&rule_json) {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg,
                }));
            }
            state.db.set_alert_rule(
                &name,
                &rule_json,
                channel.as_deref().unwrap_or("xmpp"),
                active.unwrap_or(true),
            )?;
            Ok(Response::Ack)
        }

        Request::RecurringConfirm {
            match_key,
            label,
            display_name,
            business_tag,
            active,
        } => {
            // Validate the label only when present. A bare confirm with a
            // display_name and no label is a rename (label stays NULL / preserved).
            let label = match label {
                Some(l) => match SeriesLabel::parse(&l) {
                    Some(s) => Some(s),
                    None => {
                        return Ok(Response::Error(RpcError {
                            code: "invalid".into(),
                            msg: format!("unknown label {l:?}; expected sub|bill|debt|ignore"),
                        }));
                    }
                },
                None => None,
            };
            // Nothing to persist (no label, no rename) is a no-op the operator
            // didn't mean — fail honestly rather than write an empty row.
            if label.is_none() && display_name.is_none() {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "nothing to set: provide a label (sub|bill|debt|ignore) or a name".into(),
                }));
            }
            // Re-normalize defensively so a hand-typed key matches the detector's
            // grouping (idempotent on a key copied from recurring.list).
            let key = normalize_payee(&match_key);
            if key.is_empty() {
                return Ok(Response::Error(RpcError {
                    code: "invalid".into(),
                    msg: "match_key normalizes to empty".into(),
                }));
            }
            let business_id = match business_tag {
                Some(t) => Some(state.db.business_by_tag(&t)?.id.as_i64()),
                None => None,
            };
            state.db.upsert_recurring_label(
                &key,
                label.map(SeriesLabel::as_str),
                display_name.as_deref(),
                business_id,
                active.unwrap_or(true),
                now_secs(),
            )?;
            Ok(Response::Ack)
        }
    }
}

fn parse_rule_kind(rule_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(rule_json)
        .ok()?
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn account_id_by_name(db: &Db, name: &str) -> arca_core::error::Result<AccountId> {
    db.list_active_accounts()?
        .into_iter()
        .find(|a| a.name == name)
        .and_then(|a| a.id)
        .ok_or_else(|| arca_core::error::CoreError::NotFound(format!("account name={name}")))
}

fn health(state: &State) -> arca_core::error::Result<HealthInfo> {
    let providers = state
        .db
        .list_providers()?
        .into_iter()
        .map(|p| ProviderStatus {
            kind: p.kind,
            label: p.label,
            last_poll_at: p.last_poll_at,
            last_status: p.last_status,
        })
        .collect();
    Ok(HealthInfo {
        version: state.version.into(),
        uptime_secs: state.started_at.elapsed().as_secs() as i64,
        providers,
    })
}

fn money(db: &Db) -> arca_core::error::Result<MoneySnapshot> {
    let accounts = db.list_active_accounts()?;
    let balances = db.latest_balances()?;
    let bal_for = |aid: AccountId| {
        balances
            .iter()
            .find(|(a, _)| *a == aid)
            .map(|(_, c)| *c)
            .unwrap_or(Cents::ZERO)
    };

    // Subscriptions (API usage, recurring services) get their own section and
    // are excluded from net_worth + by_kind. Their amounts may be in mixed
    // units (USD cents, credits, message counts) signaled by `currency`.
    let mut by_kind: std::collections::BTreeMap<String, (Cents, u32)> = Default::default();
    let mut net = Cents::ZERO;
    let mut subscriptions: Vec<SubscriptionRow> = Vec::new();
    let mut lines: Vec<AccountLine> = Vec::new();
    for a in &accounts {
        let aid = a.id.expect("active account has id");
        let bal = bal_for(aid);
        if a.kind == "subscription" {
            subscriptions.push(SubscriptionRow {
                name: a.name.clone(),
                currency: a.currency.clone(),
                latest: bal,
                source: a.provider_id.map(|p| p.to_string()),
            });
            continue;
        }
        // net_worth + by_kind are USD-cents totals. A non-USD account (foreign
        // currency, or a unit account promoted off `subscription`) must not be
        // summed into a USD total — exclude and log rather than silently corrupt
        // net worth. No FX conversion in v1 (honest failure / single money unit).
        // It still gets listed (with its native currency, flagged excluded) so the
        // operator sees every account in the Accounts view.
        let excluded = a.currency != "USD";
        lines.push(AccountLine {
            name: a.name.clone(),
            kind: a.kind.clone(),
            balance: bal,
            currency: a.currency.clone(),
            excluded_from_nw: excluded,
        });
        if excluded {
            tracing::warn!(
                account = %a.name, currency = %a.currency,
                "excluded non-USD account from net worth (no FX conversion in v1)"
            );
            continue;
        }
        let entry = by_kind.entry(a.kind.clone()).or_insert((Cents::ZERO, 0));
        entry.0 += bal;
        entry.1 += 1;
        match a.kind.as_str() {
            "debt" => net -= bal,
            _ => net += bal,
        }
    }
    subscriptions.sort_by(|a, b| a.name.cmp(&b.name));
    // Group by kind, then largest balance first within a kind — same order the
    // Accounts view renders, so each asset sits under its kind header.
    lines.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(b.balance.as_i64().cmp(&a.balance.as_i64()))
    });

    Ok(MoneySnapshot {
        net_worth: net,
        by_kind: by_kind
            .into_iter()
            .map(|(k, (total, count))| KindTotal {
                kind: k,
                total,
                account_count: count,
            })
            .collect(),
        accounts: lines,
        subscriptions,
        asof_secs: now_secs(),
    })
}

fn business(db: &Db, tag: &str, scope: Scope) -> arca_core::error::Result<BusinessSnapshot> {
    let biz = db.business_by_tag(tag)?;
    let (since, until) = scope_bounds(scope);
    // Income = positive amounts, expenses = negative amounts, both summed via the
    // business filter.
    let net = db.business_pnl(biz.id, since, until)?;
    // We don't have a fast split; recompute by listing rows.
    let rows = db.list_transactions(Some(since), None, 1_000_000)?;
    let mut income = Cents::ZERO;
    let mut expenses = Cents::ZERO;
    for r in rows {
        if r.business_id != Some(biz.id) {
            continue;
        }
        if r.posted_at >= until {
            continue;
        }
        if r.amount_cents.0 >= 0 {
            income += r.amount_cents;
        } else {
            expenses += r.amount_cents;
        }
    }
    Ok(BusinessSnapshot {
        tag: biz.tag,
        display_name: biz.display_name,
        scope,
        income,
        expenses,
        net,
    })
}

fn scope_bounds(scope: Scope) -> (i64, i64) {
    use chrono::{Datelike, Months, TimeZone, Utc};
    let now = Utc::now();
    // `until` = start of the next calendar month/year, not a fixed 31/366-day
    // count, so short months and non-leap years don't bleed into the next
    // period (would skew business P&L + category spend at boundaries).
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

/// `[since, until)` spanning the last `months` UTC calendar months, inclusive of
/// the current (partial) month. `since` = first day of the month `months-1` back;
/// `until` = now.
fn months_back_bounds(months: u32) -> (i64, i64) {
    use chrono::{Datelike, TimeZone, Utc};
    let now = Utc::now();
    let mut year = now.year();
    let mut month = now.month() as i32 - (months as i32 - 1);
    while month <= 0 {
        month += 12;
        year -= 1;
    }
    let since = Utc
        .with_ymd_and_hms(year, month as u32, 1, 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
        .unwrap_or(0);
    (since, now.timestamp())
}

/// Async handler for `provider.refresh`. Iterates State.providers, optionally
/// filtered by `kind`, awaits each provider's poll, and aggregates the
/// per-provider reports into a single Response::RefreshReport (whose message
/// field summarizes "<kind>:<label> <status>; ..." across all polled providers).
pub async fn handle_refresh(state: &Arc<State>, kind_filter: Option<String>) -> Response {
    let providers: Vec<_> = state
        .providers
        .iter()
        .filter(|lp| kind_filter.as_deref().is_none_or(|f| lp.impl_.kind() == f))
        .collect();

    if providers.is_empty() {
        return Response::Error(RpcError {
            code: "no_match".into(),
            msg: format!(
                "no providers match kind filter {:?}",
                kind_filter.as_deref().unwrap_or("<none>")
            ),
        });
    }

    let mut total_rows: u32 = 0;
    let mut summaries = Vec::new();
    for lp in providers {
        let report = crate::scheduler::poll_once(&state.db, lp).await;
        total_rows = total_rows.saturating_add(report.rows_written);
        let msg = report.message.unwrap_or_default();
        summaries.push(format!("{} ({msg})", report.provider_kind));
    }

    Response::RefreshReport(arca_core::provider::RefreshReport {
        provider_kind: kind_filter.unwrap_or_else(|| "*".into()),
        rows_written: total_rows,
        cursor: arca_core::provider::Cursor::default(),
        message: Some(summaries.join("; ")),
    })
}
