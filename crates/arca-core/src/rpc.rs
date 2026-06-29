//! RPC schema + length-prefix codec. JSON over Unix socket or TCP.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{CoreError, Result};
use crate::money::Cents;

/// Max accepted frame size — 4 MiB. Anything larger is a protocol error.
pub const MAX_FRAME_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Request {
    /// Liveness + last-poll status.
    Health,

    /// Snapshot for the `money` view.
    #[serde(rename = "snapshot.money")]
    SnapshotMoney,

    /// Per-business P&L.
    #[serde(rename = "snapshot.business")]
    SnapshotBusiness { tag: String, scope: Option<Scope> },

    /// Permanent portfolio drift.
    #[serde(rename = "snapshot.pp")]
    SnapshotPp,

    /// Debt status.
    #[serde(rename = "snapshot.debt")]
    SnapshotDebt { scope: Scope },

    /// Transaction list, filterable.
    #[serde(rename = "tx.list")]
    TxList {
        since: Option<i64>,
        tag: Option<String>,
        limit: Option<i64>,
    },

    /// Recurring payees derived from transaction history — subscriptions,
    /// bills, recurring debt payments. Read-only; recomputed each call from the
    /// `transactions` table (outflows), no stored state. `since` bounds the
    /// history window; `min_occurrences` overrides the detector default.
    #[serde(rename = "recurring.list")]
    RecurringList {
        since: Option<i64>,
        min_occurrences: Option<usize>,
        /// When true, the response also carries the operator-ignored series in
        /// `RecurringPage.ignored` (normally dropped) so a client can offer an
        /// "un-ignore" path. Default false — the bridge/reports/calendar never see
        /// ignored series.
        #[serde(default)]
        include_ignored: Option<bool>,
    },

    /// Time-series data for the charts view: a net-worth series (one point per
    /// distinct snapshot instant) plus monthly cash-flow buckets. Read-only;
    /// derived from `balance_snapshots` + `transactions`. `months` bounds the
    /// cash-flow window (default 12).
    #[serde(rename = "snapshot.charts")]
    SnapshotCharts { months: Option<u32> },

    /// Spending grouped by category over a scope window (expenses only, most
    /// spent first). Read-only; backs the expenses view + spending bar.
    #[serde(rename = "snapshot.categories")]
    SnapshotCategories {
        scope: Option<Scope>,
        limit: Option<i64>,
    },

    /// Trigger provider refresh.
    #[serde(rename = "provider.refresh")]
    ProviderRefresh { kind_filter: Option<String> },

    /// Recent alert firings. Defaults to undelivered only — the queue the
    /// arca-xmpp bridge would push and the TUI surfaces in its alerts panel.
    #[serde(rename = "alert.pending")]
    AlertPending {
        limit: Option<i64>,
        include_delivered: Option<bool>,
    },

    // ---- manual data entry (Phase 1; daemon-only verbs) ----
    /// Upsert an account.
    #[serde(rename = "manual.upsert_account")]
    ManualUpsertAccount {
        name: String,
        account_kind: String,
        asset_class: Option<String>,
        /// Capital-tier marker: `"t1"` | `"t2"` | `"t3"` | None. See
        /// `the investment-model spec`. Drift engine only reads `t2`.
        #[serde(default)]
        tier: Option<String>,
        currency: Option<String>,
        business_tag: Option<String>,
    },

    /// Insert a transaction.
    #[serde(rename = "manual.insert_transaction")]
    ManualInsertTransaction {
        account_name: String,
        posted_at: i64,
        amount: String, // dollars string; parsed server-side
        description: Option<String>,
        tag: Option<String>,
        business_tag: Option<String>,
        external_id: Option<String>,
    },

    /// Record a balance snapshot.
    #[serde(rename = "manual.snapshot")]
    ManualSnapshot {
        account_name: String,
        amount: String,
    },

    /// Create or update a provider row by (kind, label) — the only runtime way
    /// to register a credentialed provider (plaid/mercury/stripe/…). Write;
    /// operator-only. `config_json` must be a JSON object; `kind` must be one the
    /// registry can build (else the row would silently never load). Secrets
    /// themselves live in `secrets.age`, referenced by `secret_ref`.
    #[serde(rename = "manual.upsert_provider")]
    ManualUpsertProvider {
        provider_kind: String,
        label: String,
        #[serde(default)]
        config_json: Option<String>,
        #[serde(default)]
        secret_ref: Option<String>,
        #[serde(default)]
        poll_cadence: Option<String>,
    },

    /// Create or update a business (tag) row by `tag` — the runtime way to add a
    /// venture so providers/accounts can bind to it via `business_tag`. Write;
    /// operator-only. `tag` is the stable key (lowercase, no spaces); upserts on
    /// it, preserving `display_name`/`active` when omitted. Omitting `active`
    /// defaults to enabled.
    #[serde(rename = "manual.upsert_business")]
    ManualUpsertBusiness {
        tag: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        active: Option<bool>,
    },

    /// Declare a recurring obligation (rent, insurance, a fixed bill) by name —
    /// the runtime way to track a known recurring outflow that isn't detected
    /// from transactions. Write; operator-only. Stored in `subscriptions`
    /// (`provider_kind = "recurring"`); surfaced in Bills/upcoming projected
    /// forward by `cadence`, and on the `.ics` calendar + monthly report. This is
    /// NOT debt — it never touches net worth or the debt total. `amount` is a
    /// dollars string (negative = outflow); `cadence` is
    /// monthly|yearly|quarterly|weekly|biweekly; `next_charge_at` is the next due
    /// date (unix secs). Upserts on `name`; omitting `active` defaults to enabled.
    #[serde(rename = "manual.upsert_subscription")]
    ManualUpsertSubscription {
        name: String,
        amount: String, // dollars string; parsed server-side
        cadence: String,
        next_charge_at: i64,
        #[serde(default)]
        business_tag: Option<String>,
        #[serde(default)]
        active: Option<bool>,
    },

    /// Narrowly update a declared subscription by `name` (write; operator-only) —
    /// rename it and/or activate/deactivate it, without touching its amount/cadence/
    /// next-charge (unlike `manual.upsert_subscription`, which rewrites every
    /// column). `new_name` renames; `active=false` removes it from the schedule
    /// (soft — re-activate or re-declare to bring it back). At least one of
    /// `new_name`/`active` should be set. Backs the Bills schedule "Rename/Remove"
    /// menu for declared subs (Rent, etc.).
    #[serde(rename = "manual.update_subscription")]
    ManualUpdateSubscription {
        name: String,
        #[serde(default)]
        new_name: Option<String>,
        #[serde(default)]
        active: Option<bool>,
    },

    /// Create or update an alert rule by name. `rule_json` must carry a known
    /// `kind` (the daemon rejects a rule that could never fire). Omitting
    /// `active` defaults to enabled; omitting `channel` defaults to `xmpp`.
    #[serde(rename = "alert.upsert")]
    AlertUpsert {
        name: String,
        rule_json: String,
        channel: Option<String>,
        active: Option<bool>,
    },

    /// Confirm/label a detected recurring series (write; operator-only). Persists
    /// only the label keyed by `match_key` (the `payee` from `recurring.list`);
    /// the series stats stay derived. Upserts on `match_key`. `label` must be
    /// `label` is `sub` | `bill` | `debt` | `ignore` (else rejected), or omitted
    /// for a rename-only confirm (set `display_name` without a treatment label).
    /// At least one of `label` / `display_name` must be present. A NULL field
    /// preserves the stored value (idempotent merge); `active=false` soft-dismisses
    /// the row without losing it. Omitting `active` defaults to enabled.
    #[serde(rename = "recurring.confirm")]
    RecurringConfirm {
        match_key: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        business_tag: Option<String>,
        #[serde(default)]
        active: Option<bool>,
    },
}

impl Request {
    /// True for verbs that mutate state. Write verbs are rejected on the read
    /// socket and, on the write socket, additionally gated to the operator UID
    /// via `getpeereid(2)`. Read verbs are safe for the arca-xmpp bridge.
    #[must_use]
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Request::ProviderRefresh { .. }
                | Request::ManualUpsertAccount { .. }
                | Request::ManualInsertTransaction { .. }
                | Request::ManualSnapshot { .. }
                | Request::ManualUpsertProvider { .. }
                | Request::ManualUpsertBusiness { .. }
                | Request::ManualUpsertSubscription { .. }
                | Request::ManualUpdateSubscription { .. }
                | Request::AlertUpsert { .. }
                | Request::RecurringConfirm { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Month,
    Year,
    Ytd,
    All,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Health(HealthInfo),
    Money(MoneySnapshot),
    Business(BusinessSnapshot),
    Pp(PpSnapshot),
    Debt(DebtSnapshot),
    TxList(TxListPage),
    Recurring(RecurringPage),
    Alerts(AlertsPage),
    Charts(ChartsSnapshot),
    Categories(CategoriesSnapshot),
    RefreshReport(crate::provider::RefreshReport),
    Ack,
    Error(RpcError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub msg: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthInfo {
    pub version: String,
    pub uptime_secs: i64,
    pub providers: Vec<ProviderStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub kind: String,
    pub label: String,
    pub last_poll_at: Option<i64>,
    pub last_status: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoneySnapshot {
    pub net_worth: Cents,
    pub by_kind: Vec<KindTotal>,
    /// One row per (non-subscription) account, so the Accounts view can list each
    /// asset individually rather than only the per-kind aggregate. Grouped by
    /// `kind` at render. Non-USD accounts appear here (with their native
    /// `currency`) even though they're excluded from `net_worth`/`by_kind`.
    #[serde(default)]
    pub accounts: Vec<AccountLine>,
    pub subscriptions: Vec<SubscriptionRow>,
    pub asof_secs: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountLine {
    pub name: String,
    pub kind: String,
    pub balance: Cents,
    pub currency: String,
    /// True if excluded from net worth (non-USD, no FX in v1).
    pub excluded_from_nw: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionRow {
    pub name: String,
    pub currency: String, // "USD" / "CREDITS" / "MESSAGES"
    pub latest: Cents,    // raw value; currency tells the renderer how to format
    pub source: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KindTotal {
    pub kind: String,
    pub total: Cents,
    pub account_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusinessSnapshot {
    pub tag: String,
    pub display_name: String,
    pub scope: Scope,
    pub income: Cents,
    pub expenses: Cents,
    pub net: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PpSnapshot {
    /// T2 sleeve drift rows (equity / long_treasuries / cash, plus gold_etf
    /// and other if non-zero).
    pub rows: Vec<crate::pp::DriftRow>,
    /// T2 total across all sleeves.
    pub total: Cents,
    /// T1 hold-forever backbone summary (info-only).
    pub backbone: crate::pp::Backbone,
    /// True if any T2 sleeve has breached the 22/44 rebalance bands.
    pub band_breach: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebtSnapshot {
    pub scope: Scope,
    pub open_balances: Vec<DebtBalance>,
    pub scheduled: Vec<DebtScheduled>,
    /// Declared recurring obligations (rent, insurance, fixed bills) projected to
    /// their next occurrence by cadence — from the `subscriptions` table, NOT
    /// debt accounts. Surfaced beside debt service in Bills/upcoming; never part
    /// of `total_open` or net worth.
    #[serde(default)]
    pub fixed: Vec<DebtScheduled>,
    pub total_open: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebtBalance {
    pub account_name: String,
    pub balance: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebtScheduled {
    pub due_at: i64,
    pub amount: Cents,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxListPage {
    pub rows: Vec<TxRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxRow {
    pub id: i64,
    pub posted_at: i64,
    pub account: String,
    pub amount: Cents,
    pub description: Option<String>,
    pub category: Option<String>,
    pub tag: Option<String>,
}

/// Derived recurring series, each with any persisted operator label joined in.
/// The `Series` stats are recomputed from transactions every call (never
/// stored); only the label/display_name come from the `recurring_series` table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecurringPage {
    pub series: Vec<LabeledSeries>,
    /// Operator-ignored series, present only when the request set
    /// `include_ignored` (else empty). Kept separate so `series` stays ignore-free
    /// for every other consumer; the TUI uses this to populate its "ignored" zone.
    #[serde(default)]
    pub ignored: Vec<LabeledSeries>,
}

/// A detected series plus its persisted label, if the operator has confirmed it.
/// The series fields are flattened to the top level, so an unlabeled series is
/// wire-compatible with the bare `Series` shape plus three null fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabeledSeries {
    #[serde(flatten)]
    pub series: crate::recurring::Series,
    /// `sub` | `bill` | `debt`, or `None` if unconfirmed (or label dismissed).
    pub label: Option<String>,
    /// Operator-friendly name overriding the raw `display` descriptor.
    pub display_name: Option<String>,
    pub confirmed_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertsPage {
    /// Fired alerts (history); may be empty if nothing has triggered.
    pub rows: Vec<AlertRow>,
    /// Currently-armed (active) rules, so the view shows what's configured even
    /// when nothing has fired yet.
    #[serde(default)]
    pub rules: Vec<AlertRuleRow>,
}

/// An armed alert rule, for the "what's configured" section of the alerts view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertRuleRow {
    pub name: String,
    /// The rule's `kind` (e.g. `provider.stale`), or None if `rule_json` lacks one.
    pub kind: Option<String>,
    pub channel: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertRow {
    pub id: i64,
    pub rule_name: String,
    /// The rule's `kind` (e.g. `provider.stale`), or None if `rule_json` lacks one.
    pub rule_kind: Option<String>,
    pub fired_at: i64,
    pub delivered: bool,
    /// Human one-liner derived from the rule kind + payload, for TUI/bridge display.
    pub summary: String,
}

/// Chart-ready time series. All money in `Cents`; the client divides for axes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChartsSnapshot {
    /// Net worth at each distinct snapshot instant, oldest first.
    pub net_worth: Vec<TimePoint>,
    /// Monthly cash flow, oldest month first.
    pub cash_flow: Vec<MonthFlow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimePoint {
    pub at_secs: i64,
    pub amount: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonthFlow {
    /// `YYYY-MM` (UTC calendar month).
    pub label: String,
    pub income: Cents,   // >= 0
    pub expenses: Cents, // <= 0 (kept signed)
    pub net: Cents,
}

/// Spending grouped by category over `[since, until)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CategoriesSnapshot {
    pub scope: Scope,
    pub since: i64,
    pub until: i64,
    /// Most spent first; `amount` is negative (outflow).
    pub rows: Vec<CategorySpend>,
    /// Total spend across `rows` (negative).
    pub total: Cents,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CategorySpend {
    pub category: String,
    pub amount: Cents, // <= 0
}

// ---- codec ----

/// Read a length-prefixed JSON frame.
pub async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Request> {
    let bytes = read_frame(r).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn read_response<R: AsyncRead + Unpin>(r: &mut R) -> Result<Response> {
    let bytes = read_frame(r).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn write_request<W: AsyncWrite + Unpin>(w: &mut W, req: &Request) -> Result<()> {
    let bytes = serde_json::to_vec(req)?;
    write_frame(w, &bytes).await
}

pub async fn write_response<W: AsyncWrite + Unpin>(w: &mut W, resp: &Response) -> Result<()> {
    let bytes = serde_json::to_vec(resp)?;
    write_frame(w, &bytes).await
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let len = r.read_u32_le().await?;
    if len > MAX_FRAME_BYTES {
        return Err(CoreError::Rpc(format!("frame too large: {len}")));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| CoreError::Rpc(format!("payload too large: {}", bytes.len())))?;
    if len > MAX_FRAME_BYTES {
        return Err(CoreError::Rpc(format!("payload too large: {len}")));
    }
    w.write_u32_le(len).await?;
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

// ---- sync decoders (fuzz / offline) ----

/// Decode a `Request` from a complete length-prefixed frame buffer
/// (`[u32-LE len][JSON body]`) — the pure-sync mirror of `read_request`, with the
/// same `MAX_FRAME_BYTES` guard and the same JSON step, but over one buffer rather
/// than an async stream. Lets the fuzzer hammer the untrusted Unix-socket wire
/// boundary without a socket. Returns `Err` (never panics or over-allocates) on
/// any malformed input: missing/short prefix, oversized length, truncated body, or
/// invalid JSON.
pub fn decode_request(buf: &[u8]) -> Result<Request> {
    Ok(serde_json::from_slice(decode_frame(buf)?)?)
}

/// `decode_request`'s counterpart for the daemon→client `Response` frame.
pub fn decode_response(buf: &[u8]) -> Result<Response> {
    Ok(serde_json::from_slice(decode_frame(buf)?)?)
}

/// Validate the `[u32-LE len][body]` framing and return the body slice. Mirrors
/// `read_frame`'s length guard — the OOM backstop — without allocating: a length
/// past `MAX_FRAME_BYTES`, or a buffer too short for the declared body, is a
/// protocol error rather than a panic or a huge allocation.
fn decode_frame(buf: &[u8]) -> Result<&[u8]> {
    let prefix: [u8; 4] = buf
        .get(..4)
        .ok_or_else(|| CoreError::Rpc("frame truncated: missing length prefix".into()))?
        .try_into()
        .expect("invariant: sliced exactly 4 bytes");
    let len = u32::from_le_bytes(prefix);
    if len > MAX_FRAME_BYTES {
        return Err(CoreError::Rpc(format!("frame too large: {len}")));
    }
    buf.get(4..4 + len as usize)
        .ok_or_else(|| CoreError::Rpc(format!("frame truncated: need {len} body bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn round_trip_health() {
        let (mut a, mut b) = duplex(1024);
        write_request(&mut a, &Request::Health).await.unwrap();
        let req = read_request(&mut b).await.unwrap();
        assert!(matches!(req, Request::Health));
    }

    #[tokio::test]
    async fn round_trip_tx_list() {
        let (mut a, mut b) = duplex(1024);
        let r = Request::TxList {
            since: Some(1_000),
            tag: Some("income".into()),
            limit: Some(50),
        };
        write_request(&mut a, &r).await.unwrap();
        match read_request(&mut b).await.unwrap() {
            Request::TxList { since, tag, limit } => {
                assert_eq!(since, Some(1_000));
                assert_eq!(tag.as_deref(), Some("income"));
                assert_eq!(limit, Some(50));
            }
            other => panic!("expected TxList, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_length_prefix() {
        use tokio::io::AsyncWriteExt;
        // A 4-byte length prefix one past the cap must error at the guard,
        // before the `vec![0u8; len]` allocation (DoS backstop). We write only
        // the prefix — no body — so a guard that allocated/blocked would hang.
        let (mut a, mut b) = duplex(64);
        a.write_u32_le(MAX_FRAME_BYTES + 1).await.unwrap();
        a.flush().await.unwrap();
        let err = read_request(&mut b).await.unwrap_err();
        assert!(err.to_string().contains("frame too large"), "got: {err}");
    }

    /// Frame a body the way `write_frame` does: u32-LE length prefix + JSON body.
    fn frame(body: &[u8]) -> Vec<u8> {
        let mut f = u32::try_from(body.len()).unwrap().to_le_bytes().to_vec();
        f.extend_from_slice(body);
        f
    }

    #[test]
    fn decode_request_accepts_a_well_formed_frame() {
        let body = serde_json::to_vec(&Request::SnapshotMoney).unwrap();
        assert!(matches!(
            decode_request(&frame(&body)).unwrap(),
            Request::SnapshotMoney
        ));
    }

    #[test]
    fn decode_response_accepts_a_well_formed_frame() {
        let body = serde_json::to_vec(&Response::Ack).unwrap();
        assert!(matches!(
            decode_response(&frame(&body)).unwrap(),
            Response::Ack
        ));
    }

    #[test]
    fn decode_frame_rejects_short_prefix_truncated_and_oversized() {
        assert!(decode_request(&[]).is_err()); // no prefix
        assert!(decode_request(&[1, 0, 0]).is_err()); // 3-byte prefix
        // Declares a 10-byte body but supplies none.
        assert!(decode_request(&10u32.to_le_bytes()).is_err());
        // Oversized length must be rejected at the guard, before any allocation.
        let oversized = (MAX_FRAME_BYTES + 1).to_le_bytes();
        assert!(
            decode_request(&oversized)
                .unwrap_err()
                .to_string()
                .contains("frame too large")
        );
    }

    #[test]
    fn decode_request_rejects_invalid_json_body() {
        assert!(decode_request(&frame(b"not json at all")).is_err());
        assert!(decode_request(&frame(b"{}")).is_err()); // valid JSON, missing `kind`
    }

    #[test]
    fn write_verbs_classified() {
        // Reads.
        assert!(!Request::Health.is_write());
        assert!(!Request::SnapshotMoney.is_write());
        assert!(!Request::SnapshotPp.is_write());
        assert!(
            !Request::TxList {
                since: None,
                tag: None,
                limit: None
            }
            .is_write()
        );
        assert!(
            !Request::AlertPending {
                limit: None,
                include_delivered: None
            }
            .is_write()
        );
        assert!(
            !Request::RecurringList {
                since: None,
                min_occurrences: None,
                include_ignored: None,
            }
            .is_write()
        );
        // Writes.
        assert!(Request::ProviderRefresh { kind_filter: None }.is_write());
        assert!(
            Request::ManualUpsertProvider {
                provider_kind: "plaid".into(),
                label: "Plaid - First Bank".into(),
                config_json: None,
                secret_ref: None,
                poll_cadence: None,
            }
            .is_write()
        );
        assert!(
            Request::ManualSnapshot {
                account_name: "x".into(),
                amount: "1.00".into()
            }
            .is_write()
        );
        assert!(
            Request::AlertUpsert {
                name: "balance.low".into(),
                rule_json: r#"{"kind":"provider.stale"}"#.into(),
                channel: None,
                active: None,
            }
            .is_write()
        );
        assert!(
            Request::ManualUpsertBusiness {
                tag: "acme".into(),
                display_name: None,
                active: None,
            }
            .is_write()
        );
    }
}
