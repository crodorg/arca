//! Mercury provider — read-only API token, one row per business.
//!
//! Required secrets:
//!   mercury_<label>_token   (Bearer token; referenced by `providers.secret_ref`)
//!
//! `providers.config_json` shape:
//! ```json
//! { "business_tag": "main" }   // optional; binds rows to a business
//! ```

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use chrono::DateTime;
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

const BASE_URL: &str = "https://api.mercury.com/api/v1";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MercuryConfig {
    #[serde(default)]
    pub business_tag: Option<String>,
}

pub struct MercuryProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    token: SecretString,
    business_id: Option<BusinessId>,
    db: Arc<Db>,
}

impl MercuryProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let cfg: MercuryConfig =
            serde_json::from_str(&row.config_json).context("mercury config_json")?;
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("mercury provider {} has no secret_ref", row.label))?;
        let token = secrets.require_owned(secret_ref)?;
        let business_id = match cfg.business_tag.as_deref() {
            Some(tag) => Some(db.business_by_tag(tag)?.id),
            None => None,
        };
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            token,
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
            .bearer_auth(self.token.expose_secret())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("read mercury body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "mercury {path} -> {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("mercury json")?;
        Ok((bytes.to_vec(), json))
    }
}

#[async_trait]
impl Provider for MercuryProvider {
    fn kind(&self) -> &'static str {
        "mercury"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (raw, json) = self
            .get("/accounts")
            .await
            .map_err(|e| CoreError::Rpc(format!("mercury accounts: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "mercury:/accounts", &raw)?;

        let accounts = json
            .get("accounts")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::Rpc("mercury: no accounts in response".into()))?;
        let mut rows = 0u32;
        for a in accounts {
            let ext = a.get("id").and_then(Value::as_str).unwrap_or("");
            let name = a.get("name").and_then(Value::as_str).unwrap_or("mercury");
            let cur = a
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("USD")
                .to_string();
            let current = a
                .get("currentBalance")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let cents = Cents((current * 100.0).round() as i64);
            let acct = Account {
                id: None,
                name: name.into(),
                kind: "business".into(),
                asset_class: Some("cash".into()),
                tier: None,
                currency: cur,
                provider_id: Some(self.provider_id),
                business_id: self.business_id,
                external_id: Some(ext.into()),
                active: true,
            };
            let aid = self.db.upsert_account(&acct)?;
            self.db.insert_snapshot(aid, cents, "mercury")?;
            rows += 1;
        }
        Ok(RefreshReport {
            provider_kind: "mercury".into(),
            rows_written: rows,
            cursor: Cursor::default(),
            message: None,
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        // List local Mercury accounts (those we just snapshotted) to iterate per-account.
        let mercury_accounts: Vec<Account> = self
            .db
            .list_active_accounts()?
            .into_iter()
            .filter(|a| a.provider_id == Some(self.provider_id))
            .collect();

        let mut total = 0u32;
        for a in mercury_accounts {
            let Some(ext) = a.external_id.as_deref() else {
                continue;
            };
            let aid = a.id.expect("active account");
            let path = format!("/account/{ext}/transactions?limit=500");
            let (raw, json) = self
                .get(&path)
                .await
                .map_err(|e| CoreError::Rpc(format!("mercury txns: {e}")))?;
            self.db.insert_raw(
                self.provider_id,
                Some(ext),
                "mercury:/account/{id}/transactions",
                &raw,
            )?;

            let txns = json.get("transactions").and_then(Value::as_array);
            let Some(txns) = txns else {
                continue;
            };
            for t in txns {
                let id = t.get("id").and_then(Value::as_str).unwrap_or("");
                let amount = t.get("amount").and_then(Value::as_f64).unwrap_or(0.0);
                let cents = Cents((amount * 100.0).round() as i64);
                let posted = t
                    .get("postedAt")
                    .or_else(|| t.get("createdAt"))
                    .and_then(Value::as_str)
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.timestamp())
                    .unwrap_or(0);
                let desc = t
                    .get("counterpartyName")
                    .or_else(|| t.get("counterpartyNickname"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let kind_tag = t.get("kind").and_then(Value::as_str).map(str::to_string);

                let trow = Transaction {
                    id: None,
                    account_id: aid,
                    posted_at: posted,
                    amount_cents: cents,
                    currency: "USD".into(),
                    description: desc,
                    category: kind_tag,
                    tag: if cents.0 > 0 {
                        Some("income".into())
                    } else {
                        Some("business".into())
                    },
                    business_id: self.business_id,
                    external_id: Some(id.into()),
                    source: "mercury".into(),
                };
                self.db.upsert_transaction(&trow)?;
                total += 1;
            }
        }

        Ok(RefreshReport {
            provider_kind: "mercury".into(),
            rows_written: total,
            cursor: Cursor::default(),
            message: None,
        })
    }
}
