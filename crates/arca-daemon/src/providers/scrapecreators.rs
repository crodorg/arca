//! ScrapeCreators usage provider — credit balance.
//!
//! Required secrets:
//!   scrapecreators_<label>_key   (x-api-key header)
//!
//! Endpoint: `GET /v1/account/credit-balance` → `{ "creditCount": N }`
//! (some shapes also report `credits`; we read whichever is present).
//!
//! Note: the snapshot value here is **credits remaining**, not USD. The display
//! layer uses `source = "scrapecreators"` to render with the right unit.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::money::Cents;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.scrapecreators.com";

pub struct ScrapeCreatorsProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    key: SecretString,
    db: Arc<Db>,
}

impl ScrapeCreatorsProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("scrapecreators provider {} has no secret_ref", row.label))?;
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

    async fn get(&self, path: &str) -> AnyResult<(Vec<u8>, Value)> {
        let url = format!("{}{}", self.base_url, path);
        let resp = client()
            .get(&url)
            .header("x-api-key", self.key.expose_secret())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("scrapecreators body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "scrapecreators {path} -> {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("scrapecreators json")?;
        Ok((bytes.to_vec(), json))
    }

    fn local_account_name(&self) -> String {
        format!("ScrapeCreators — {} (credits)", self.label)
    }
}

#[async_trait]
impl Provider for ScrapeCreatorsProvider {
    fn kind(&self) -> &'static str {
        "scrapecreators"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (raw, json) = self
            .get("/v1/account/credit-balance")
            .await
            .map_err(|e| CoreError::Rpc(format!("scrapecreators credits: {e}")))?;
        self.db.insert_raw(
            self.provider_id,
            None,
            "scrapecreators:/v1/account/credit-balance",
            &raw,
        )?;

        let credits = json
            .get("creditCount")
            .or_else(|| json.get("credits"))
            .and_then(Value::as_i64)
            .ok_or_else(|| CoreError::Rpc("scrapecreators: no creditCount/credits field".into()))?;

        let acct = Account {
            id: None,
            name: self.local_account_name(),
            kind: "subscription".into(),
            asset_class: None,
            tier: None,
            currency: "CREDITS".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: Some("credit-balance".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        // Store credit count directly in the cents column. Currency=CREDITS
        // signals to the display layer that this isn't a USD amount.
        self.db
            .insert_snapshot(aid, Cents(credits), "scrapecreators")?;
        Ok(RefreshReport {
            provider_kind: "scrapecreators".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: Some(format!("{credits} credits remaining")),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        Ok(RefreshReport {
            provider_kind: "scrapecreators".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("scrapecreators: no per-call feed exposed".into()),
        })
    }
}
