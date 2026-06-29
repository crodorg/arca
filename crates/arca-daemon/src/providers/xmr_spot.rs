//! Monero (XMR) spot-price provider — values a held quantity of XMR in USD.
//!
//! Unlike `gold_spot` (which only records a reference price, because ounces held
//! aren't tracked), this provider knows the holding: it multiplies the live
//! XMR/USD spot by a configured `quantity` and writes the USD value as a balance
//! snapshot, so the operator's net worth reflects the coins they actually hold.
//!
//! Source: `https://api.kraken.com/0/public/Ticker?pair=XMRUSD` (keyless, no
//! signup). Returns `{ "error": [...], "result": { "XXMRZUSD": { "c": ["<last
//! trade price>", "<lot>"], ... } } }`. We take the single result entry's `c[0]`
//! (last trade price, a string). A non-empty `error` array fails the refresh
//! (honest failure — no stale/zero balance).
//!
//! `providers.config_json`:
//! ```json
//! { "quantity": 3.5, "account_name": "xmr_wallet" }
//! ```
//! `quantity` (required) = coins held; `account_name` (default "xmr_wallet") =
//! the account whose USD balance this updates. The account is upserted by name
//! (reusing an existing manual `xmr_wallet` row, no duplicate) and stamped as a
//! Tier-1 backbone asset (`asset_class = "xmr"`).
//!
//! `secret_ref`: unused — endpoint is keyless. Future-proof for swapping in a
//! keyed source by reading the key when present.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.kraken.com";

fn default_account_name() -> String {
    "xmr_wallet".to_string()
}

#[derive(Clone, Debug, Deserialize)]
pub struct XmrSpotConfig {
    /// Coins held. Required — a missing quantity fails the build (skipped + logged)
    /// rather than silently valuing zero coins.
    pub quantity: f64,
    #[serde(default = "default_account_name")]
    pub account_name: String,
}

pub struct XmrSpotProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    quantity: f64,
    account_name: String,
    db: Arc<Db>,
}

impl XmrSpotProvider {
    pub fn build(row: &ProviderRow, _secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let cfg: XmrSpotConfig =
            serde_json::from_str(&row.config_json).context("xmr_spot config_json")?;
        if !cfg.quantity.is_finite() || cfg.quantity < 0.0 {
            return Err(anyhow!(
                "xmr_spot {}: quantity must be a finite non-negative number, got {}",
                row.label,
                cfg.quantity
            ));
        }
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            quantity: cfg.quantity,
            account_name: cfg.account_name,
            db,
        })
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Fetch the last XMR/USD trade price (USD per coin) from Kraken's public
    /// ticker. Returns the raw body (for audit) and the price.
    async fn fetch_xmr_usd(&self) -> AnyResult<(Vec<u8>, f64)> {
        let url = format!("{}/0/public/Ticker?pair=XMRUSD", self.base_url);
        let resp = client()
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("xmr_spot body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "xmr_spot {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("xmr_spot json")?;
        if let Some(errs) = json.get("error").and_then(Value::as_array) {
            if !errs.is_empty() {
                return Err(anyhow!("xmr_spot kraken error: {errs:?}"));
            }
        }
        // Take the single entry under `result` (Kraken keys it "XXMRZUSD", but we
        // don't hardcode the key) and read `c[0]`, the last trade price string.
        let result = json
            .get("result")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("xmr_spot: response missing `result` object"))?;
        let entry = result
            .values()
            .next()
            .ok_or_else(|| anyhow!("xmr_spot: empty `result` object"))?;
        let price_str = entry
            .get("c")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("xmr_spot: no last-trade price (`c[0]`) in ticker"))?;
        let price: f64 = price_str
            .parse()
            .with_context(|| format!("xmr_spot: non-numeric price {price_str:?}"))?;
        if !price.is_finite() || price <= 0.0 {
            return Err(anyhow!("xmr_spot: implausible price {price}"));
        }
        Ok((bytes.to_vec(), price))
    }
}

#[async_trait]
impl Provider for XmrSpotProvider {
    fn kind(&self) -> &'static str {
        "xmr_spot"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (raw, usd_per_coin) = self
            .fetch_xmr_usd()
            .await
            .map_err(|e| CoreError::Rpc(format!("xmr_spot: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "xmr_spot:/0/public/Ticker", &raw)?;

        // Reference price series (USD/coin), parallel to gold_spot's XAU series.
        let price_cents = crate::providers::convert::dollars_to_cents("xmr price", usd_per_coin)
            .map_err(|e| CoreError::Rpc(format!("xmr_spot: {e}")))?;
        self.db
            .insert_price_snapshot("XMR", price_cents, "xmr_spot")?;

        // Holding value = quantity × price, written to the bound account.
        let value_cents = crate::providers::convert::dollars_to_cents(
            "xmr holding",
            self.quantity * usd_per_coin,
        )
        .map_err(|e| CoreError::Rpc(format!("xmr_spot: {e}")))?;

        // Upsert by name (external_id None → name-dedup), reusing the existing
        // `xmr_wallet` row rather than creating a duplicate, and stamping it as a
        // Tier-1 backbone XMR asset. provider_id attributes it without forcing the
        // (provider_id, external_id) dedup path.
        let acct = Account {
            id: None,
            name: self.account_name.clone(),
            kind: "asset".into(),
            asset_class: Some("xmr".into()),
            tier: Some("t1".into()),
            currency: "USD".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: None,
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db.insert_snapshot(aid, value_cents, "xmr_spot")?;

        Ok(RefreshReport {
            provider_kind: "xmr_spot".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: Some(format!(
                "{} XMR × ${:.2} = ${:.2}",
                self.quantity,
                usd_per_coin,
                value_cents.0 as f64 / 100.0
            )),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        Ok(RefreshReport {
            provider_kind: "xmr_spot".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("xmr_spot: spot-valued holding, no per-tx feed".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(config_json: &str) -> ProviderRow {
        ProviderRow {
            id: ProviderId(1),
            kind: "xmr_spot".into(),
            label: "Monero".into(),
            config_json: config_json.into(),
            secret_ref: None,
            poll_cadence: "daily".into(),
            last_poll_at: None,
            last_status: None,
        }
    }

    fn db() -> Arc<Db> {
        Arc::new(Db::open_memory().unwrap())
    }

    #[test]
    fn build_accepts_quantity_and_defaults_account() {
        let p = XmrSpotProvider::build(&row(r#"{"quantity":3.5}"#), &Secrets::for_test(&[]), db())
            .unwrap();
        assert!((p.quantity - 3.5).abs() < 1e-9);
        assert_eq!(p.account_name, "xmr_wallet");
    }

    #[test]
    fn build_rejects_missing_or_bad_quantity() {
        // Missing quantity → serde error (no silent zero holding).
        assert!(XmrSpotProvider::build(&row("{}"), &Secrets::for_test(&[]), db()).is_err());
        // Negative / non-finite quantity rejected.
        assert!(
            XmrSpotProvider::build(&row(r#"{"quantity":-1}"#), &Secrets::for_test(&[]), db())
                .is_err()
        );
    }
}
