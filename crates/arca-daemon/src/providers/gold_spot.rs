//! Gold spot-price provider.
//!
//! Source: `https://api.gold-api.com/price/XAU` (keyless, no signup). Returns:
//!   `{ "name": "Gold", "price": 2348.71, "symbol": "XAU", ... }`
//! `price` is USD per troy ounce.
//!
//! `providers.config_json`: `{}` (no fields).
//! `secret_ref`: unused — endpoint is keyless. Future-proof for swapping in
//! a paid source (metals.dev, goldapi.io) by reading the key when present.
//!
//! Writes to `price_snapshots`, not `balance_snapshots` — gold spot is a
//! market reference price, not an account balance. PP allocation continues
//! to read user-maintained USD values in `accounts.asset_class='gold'`.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use serde_json::Value;

use arca_core::db::{Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::money::Cents;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.gold-api.com";

pub struct GoldSpotProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    db: Arc<Db>,
}

impl GoldSpotProvider {
    pub fn build(row: &ProviderRow, _secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            db,
        })
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn fetch_xau_usd(&self) -> AnyResult<(Vec<u8>, f64)> {
        let url = format!("{}/price/XAU", self.base_url);
        let resp = client()
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("gold_spot body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "gold_spot {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("gold_spot json")?;
        let price = json
            .get("price")
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("gold_spot: no numeric price field in response"))?;
        Ok((bytes.to_vec(), price))
    }
}

#[async_trait]
impl Provider for GoldSpotProvider {
    fn kind(&self) -> &'static str {
        "gold_spot"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (raw, usd_per_oz) = self
            .fetch_xau_usd()
            .await
            .map_err(|e| CoreError::Rpc(format!("gold_spot: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "gold_spot:/price/XAU", &raw)?;

        let cents = Cents((usd_per_oz * 100.0).round() as i64);
        self.db.insert_price_snapshot("XAU", cents, "gold_spot")?;

        Ok(RefreshReport {
            provider_kind: "gold_spot".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: Some(format!("XAU/USD ${usd_per_oz:.2}/oz")),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        Ok(RefreshReport {
            provider_kind: "gold_spot".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("gold_spot: reference price, no per-tx feed".into()),
        })
    }
}
