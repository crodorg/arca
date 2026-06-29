//! OpenRouter usage provider — cumulative spend in USD.
//!
//! Required secrets:
//!   openrouter_<label>_key   (Bearer key, e.g. `sk-or-v1-...`)
//!
//! `providers.config_json`: `{}` (no fields needed).
//!
//! Endpoint: `GET /api/v1/credits` returns
//!   `{ "data": { "total_credits": <float>, "total_usage": <float> } }`
//!
//! Both values are dollar amounts. We snapshot `total_usage` (cumulative spend)
//! as the account balance; MTD is derived in the UI by diffing against the
//! first snapshot of the calendar month.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://openrouter.ai";

pub struct OpenRouterProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    key: SecretString,
    db: Arc<Db>,
}

impl OpenRouterProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("openrouter provider {} has no secret_ref", row.label))?;
        let key = secrets.require_owned(secret_ref)?;
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            key,
            db,
        })
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn get_credits(&self) -> AnyResult<(Vec<u8>, Value)> {
        let url = format!("{}/api/v1/credits", self.base_url);
        let resp = client()
            .get(&url)
            .bearer_auth(self.key.expose_secret())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("openrouter body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "openrouter {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("openrouter json")?;
        Ok((bytes.to_vec(), json))
    }

    fn local_account_name(&self) -> String {
        format!("OpenRouter — {}", self.label)
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn kind(&self) -> &'static str {
        "openrouter"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (raw, json) = self
            .get_credits()
            .await
            .map_err(|e| CoreError::Rpc(format!("openrouter credits: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "openrouter:/api/v1/credits", &raw)?;

        let data = json
            .get("data")
            .ok_or_else(|| CoreError::Rpc("openrouter: no data field".into()))?;
        // total_usage is cumulative spend; MTD is derived by diffing snapshots.
        // A missing/non-numeric value must error (honest failure) — coercing to
        // 0 would fabricate a giant spurious negative MTD against the prior real
        // snapshot.
        let usage_dollars = data
            .get("total_usage")
            .and_then(Value::as_f64)
            .ok_or_else(|| CoreError::Rpc("openrouter: missing/non-numeric total_usage".into()))?;
        let cents = crate::providers::convert::dollars_to_cents("openrouter", usage_dollars)?;

        let acct = Account {
            id: None,
            name: self.local_account_name(),
            kind: "subscription".into(),
            asset_class: None,
            tier: None,
            currency: "USD".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: Some("credits".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db.insert_snapshot(aid, cents, "openrouter")?;

        Ok(RefreshReport {
            provider_kind: "openrouter".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: Some(format!("cumulative usage ${usage_dollars:.2}")),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        // OpenRouter exposes credits totals, not per-call rows in v1.
        Ok(RefreshReport {
            provider_kind: "openrouter".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("openrouter: no per-tx feed in v1".into()),
        })
    }
}
