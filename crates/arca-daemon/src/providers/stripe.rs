//! Stripe provider — restricted key per business.
//!
//! Required secrets:
//!   stripe_<label>_key   (restricted key; referenced by `providers.secret_ref`)
//!
//! `providers.config_json` shape:
//! ```json
//! { "business_tag": "main" }
//! ```
//!
//! Stripe returns amounts already in integer minor units (cents). No float math.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow, Transaction};
use arca_core::error::{CoreError, Result};
use arca_core::ids::{BusinessId, ProviderId};
use arca_core::money::Cents;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.stripe.com";
/// Pagination backstop against a misbehaving cursor (~20k balance txns).
const MAX_PAGES: u32 = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StripeConfig {
    #[serde(default)]
    pub business_tag: Option<String>,
}

pub struct StripeProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    key: SecretString,
    business_id: Option<BusinessId>,
    db: Arc<Db>,
}

impl StripeProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let cfg: StripeConfig =
            serde_json::from_str(&row.config_json).context("stripe config_json")?;
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("stripe provider {} has no secret_ref", row.label))?;
        let key = secrets.require_owned(secret_ref)?;
        let business_id = match cfg.business_tag.as_deref() {
            Some(tag) => Some(db.business_by_tag(tag)?.id),
            None => None,
        };
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            key,
            business_id,
            db,
        })
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn get(&self, path: &str) -> AnyResult<(Vec<u8>, Value)> {
        let url = format!("{}{}", self.base_url, path);
        let resp = client()
            .get(&url)
            .bearer_auth(self.key.expose_secret())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("read stripe body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "stripe {path} -> {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("stripe json")?;
        Ok((bytes.to_vec(), json))
    }

    fn local_account_name(&self) -> String {
        format!("Stripe — {}", self.label)
    }
}

#[async_trait]
impl Provider for StripeProvider {
    fn kind(&self) -> &'static str {
        "stripe"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (raw, json) = self
            .get("/v1/balance")
            .await
            .map_err(|e| CoreError::Rpc(format!("stripe balance: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "stripe:/v1/balance", &raw)?;

        // Sum available + pending across currencies (USD only for v1).
        let mut total_cents = 0i64;
        let mut currency = "USD".to_string();
        for arr_key in ["available", "pending"] {
            if let Some(arr) = json.get(arr_key).and_then(Value::as_array) {
                for entry in arr {
                    let amt = entry.get("amount").and_then(Value::as_i64).unwrap_or(0);
                    total_cents += amt;
                    if let Some(c) = entry.get("currency").and_then(Value::as_str) {
                        currency = c.to_uppercase();
                    }
                }
            }
        }

        let acct = Account {
            id: None,
            name: self.local_account_name(),
            kind: "business".into(),
            asset_class: Some("cash".into()),
            tier: None,
            currency,
            provider_id: Some(self.provider_id),
            business_id: self.business_id,
            external_id: Some("balance".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db.insert_snapshot(aid, Cents(total_cents), "stripe")?;
        Ok(RefreshReport {
            provider_kind: "stripe".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: None,
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        let aid = self
            .db
            .find_account_by_external(self.provider_id, "balance")?
            .and_then(|a| a.id)
            .ok_or_else(|| {
                CoreError::NotFound(
                    "stripe local account missing — run refresh_balances first".into(),
                )
            })?;

        // Page through ALL balance_transactions (newest-first), following
        // `has_more` via `starting_after`, so history past the first 100 rows is
        // never silently dropped. Idempotent: the `external_id` UNIQUE upsert
        // dedups rows already seen on prior polls. (A `created[gte]` watermark to
        // avoid re-fetching old pages is a future efficiency optimization; at
        // single-operator volume full pagination is cheap and correct.)
        let mut total = 0u32;
        let mut starting_after: Option<String> = None;
        let mut pages = 0u32;
        loop {
            let mut path = String::from("/v1/balance_transactions?limit=100");
            if let Some(after) = &starting_after {
                path.push_str("&starting_after=");
                path.push_str(after);
            }
            let (raw, json) = self
                .get(&path)
                .await
                .map_err(|e| CoreError::Rpc(format!("stripe bal txns: {e}")))?;
            self.db.insert_raw(
                self.provider_id,
                None,
                "stripe:/v1/balance_transactions",
                &raw,
            )?;

            let Some(txns) = json.get("data").and_then(Value::as_array) else {
                break;
            };
            if txns.is_empty() {
                break;
            }
            let mut last_id: Option<String> = None;
            for t in txns {
                let id = t.get("id").and_then(Value::as_str).unwrap_or("");
                last_id = Some(id.to_string());
                let amount = t.get("amount").and_then(Value::as_i64).unwrap_or(0);
                let posted = t.get("created").and_then(Value::as_i64).unwrap_or(0);
                let stype = t.get("type").and_then(Value::as_str).map(str::to_string);
                let desc = t
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let cur = t
                    .get("currency")
                    .and_then(Value::as_str)
                    .unwrap_or("USD")
                    .to_uppercase();
                let tag = match stype.as_deref() {
                    Some("charge" | "payment") => Some("income".into()),
                    Some("payout" | "refund") => Some("business".into()),
                    _ => stype.clone(),
                };
                let trow = Transaction {
                    id: None,
                    account_id: aid,
                    posted_at: posted,
                    amount_cents: Cents(amount),
                    currency: cur,
                    description: desc,
                    category: stype,
                    tag,
                    business_id: self.business_id,
                    external_id: Some(id.into()),
                    source: "stripe".into(),
                };
                self.db.upsert_transaction(&trow)?;
                total += 1;
            }

            let has_more = json
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            pages += 1;
            match last_id {
                Some(id) if has_more && pages < MAX_PAGES => starting_after = Some(id),
                _ => break,
            }
        }
        Ok(RefreshReport {
            provider_kind: "stripe".into(),
            rows_written: total,
            cursor: Cursor::default(),
            message: None,
        })
    }
}
