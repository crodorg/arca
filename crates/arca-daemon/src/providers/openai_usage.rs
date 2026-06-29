//! OpenAI usage provider — month-to-date API spend in USD.
//!
//! Required secrets:
//!   openai_<label>_admin_key   (Admin key, `sk-admin...`)
//!
//! `providers.config_json`: `{}` (no fields needed).
//!
//! Endpoint: `GET /v1/organization/costs?start_time=<unix>&bucket_width=1d`
//! with header `Authorization: Bearer <admin key>`. Response:
//! `{ data: [ { object:"bucket", start_time, end_time,
//!              results: [ { amount: { value, currency }, line_item } ] } ],
//!    has_more, next_page }`.
//!
//! `amount.value` is a **number in dollars** (e.g. `0.06` = 6¢), so we sum and
//! ×100 to integer cents. We snapshot the summed month-to-date cost as the
//! balance of a `subscription` account.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::money::Cents;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.openai.com";
const ENDPOINT: &str = "openai:/v1/organization/costs";
/// Hard cap on pagination follows so a misbehaving cursor can't loop forever.
const MAX_PAGES: usize = 40;

pub struct OpenAiUsageProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    key: SecretString,
    db: Arc<Db>,
}

impl OpenAiUsageProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("openai_usage provider {} has no secret_ref", row.label))?;
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

    /// Sum month-to-date cost (in cents) across all buckets/results, following
    /// pagination. Returns the total and each page's raw body (for audit).
    async fn fetch_mtd(&self) -> AnyResult<(i64, Vec<Vec<u8>>)> {
        let start_time = month_start_unix(Utc::now()).to_string();
        let url = format!("{}/v1/organization/costs", self.base_url);
        let mut total_dollars = 0.0_f64;
        let mut raws: Vec<Vec<u8>> = Vec::new();
        let mut page: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let mut req = client()
                .get(&url)
                .bearer_auth(self.key.expose_secret())
                .query(&[("start_time", start_time.as_str()), ("bucket_width", "1d")]);
            if let Some(p) = &page {
                req = req.query(&[("page", p.as_str())]);
            }
            let resp = req.send().await.with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            let bytes = resp.bytes().await.context("openai body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "openai {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            let json: Value = serde_json::from_slice(&bytes).context("openai json")?;
            raws.push(bytes.to_vec());

            for bucket in json["data"].as_array().into_iter().flatten() {
                for r in bucket["results"].as_array().into_iter().flatten() {
                    // A result with no `amount` at all is skippable, but a
                    // present-but-non-numeric value must fail the refresh (so the
                    // stale tripwire fires) rather than silently undercount MTD.
                    let Some(val) = r.get("amount").and_then(|a| a.get("value")) else {
                        continue;
                    };
                    let n = val.as_f64().ok_or_else(|| {
                        anyhow!("openai cost bucket has non-numeric amount.value")
                    })?;
                    total_dollars += n;
                }
            }

            if json["has_more"].as_bool().unwrap_or(false) {
                match json["next_page"].as_str() {
                    Some(p) => page = Some(p.to_string()),
                    None => break,
                }
            } else {
                break;
            }
        }
        // Guard the float→cents cast (NaN/Inf/overflow) instead of a bare
        // saturating `as i64`, same as the other float providers.
        let cents = crate::providers::convert::dollars_to_cents("openai cost", total_dollars)
            .map_err(|e| anyhow!("{e}"))?;
        Ok((cents.0, raws))
    }

    fn local_account_name(&self) -> String {
        format!("OpenAI API — {}", self.label)
    }
}

#[async_trait]
impl Provider for OpenAiUsageProvider {
    fn kind(&self) -> &'static str {
        "openai_usage"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let (cents, raws) = self
            .fetch_mtd()
            .await
            .map_err(|e| CoreError::Rpc(format!("openai costs: {e}")))?;
        for raw in &raws {
            self.db.insert_raw(self.provider_id, None, ENDPOINT, raw)?;
        }

        let acct = Account {
            id: None,
            name: self.local_account_name(),
            kind: "subscription".into(),
            asset_class: None,
            tier: None,
            currency: "USD".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: Some("mtd_cost".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db.insert_snapshot(aid, Cents(cents), "openai_usage")?;

        Ok(RefreshReport {
            provider_kind: "openai_usage".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: Some(format!("MTD cost ${:.2}", cents as f64 / 100.0)),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        // Costs endpoint is an aggregate; no per-call transaction feed.
        Ok(RefreshReport {
            provider_kind: "openai_usage".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("openai_usage: aggregate only, no per-tx feed".into()),
        })
    }
}

/// First instant of the current calendar month in UTC, as unix seconds. The
/// costs endpoint snaps buckets to UTC days, so month-to-date is computed in UTC
/// for consistency with the API.
fn month_start_unix(now: DateTime<Utc>) -> i64 {
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .expect("invariant: month start is unambiguous in UTC")
        .timestamp()
}
