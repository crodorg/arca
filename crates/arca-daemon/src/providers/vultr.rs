//! Vultr provider — month-to-date outbound bandwidth for one instance.
//!
//! Tracks how close a VPS is to its monthly transfer quota so the
//! `bandwidth.high` alert rule can warn before overage. Handy for an instance
//! that self-hosts heavy assets (map tiles, media) and so has real egress worth
//! watching, but works for any Vultr instance.
//!
//! Required secrets:
//!   vultr_<label>_api_key   (Bearer; referenced by `providers.secret_ref`)
//!
//! `providers.config_json` shape:
//! ```json
//! { "instance_id": "00000000-0000-0000-0000-000000000000" }
//! ```
//!
//! The snapshot value is **month-to-date outbound gigabytes**, not USD.
//! `currency = "GB"` signals the unit to the display layer. The quota itself
//! isn't snapshotted — the operator sets the alert threshold in GB (see
//! `bandwidth.high`); the live quota is folded into the refresh message for
//! context.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::money::Cents;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.vultr.com/v2";

/// Vultr counts transfer in decimal gigabytes (1 GB = 1e9 bytes); `allowed_bandwidth`
/// on the instance object is in the same unit.
const BYTES_PER_GB: i64 = 1_000_000_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VultrConfig {
    pub instance_id: String,
}

pub struct VultrProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    api_key: SecretString,
    instance_id: String,
    db: Arc<Db>,
}

impl VultrProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let cfg: VultrConfig =
            serde_json::from_str(&row.config_json).context("vultr config_json")?;
        if cfg.instance_id.trim().is_empty() {
            return Err(anyhow!(
                "vultr provider {} config_json missing instance_id",
                row.label
            ));
        }
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("vultr provider {} has no secret_ref", row.label))?;
        let api_key = secrets.require_owned(secret_ref)?;
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            api_key,
            instance_id: cfg.instance_id,
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
            .header(
                "Authorization",
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Accept", "application/json")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("vultr body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "vultr {path} -> {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("vultr json")?;
        Ok((bytes.to_vec(), json))
    }

    fn local_account_name(&self) -> String {
        format!("Vultr — {} (egress GB)", self.label)
    }
}

/// Sum `outgoing_bytes` for the dates that fall in the current calendar month.
/// Vultr's `/bandwidth` response is a date-keyed object
/// (`{"2026-05-01": {"incoming_bytes":N,"outgoing_bytes":M}, ...}`); filtering on
/// the `YYYY-MM` prefix gives month-to-date egress. Pure so it unit-tests without
/// a live API.
///
/// Errors (rather than coercing to 0) when the `bandwidth` object is absent or a
/// current-month day's `outgoing_bytes` is missing/non-numeric: a malformed
/// response must trip `provider.stale`, not snapshot a fabricated low egress that
/// silently never fires the `bandwidth.high` alert (honest failure).
fn mtd_outgoing_bytes(bandwidth: &Value, month_prefix: &str) -> Result<i64> {
    let obj = bandwidth
        .get("bandwidth")
        .and_then(Value::as_object)
        .ok_or_else(|| CoreError::Rpc("vultr: response missing `bandwidth` object".into()))?;
    let mut total: i64 = 0;
    for (date, day) in obj
        .iter()
        .filter(|(date, _)| date.starts_with(month_prefix))
    {
        let bytes = day
            .get("outgoing_bytes")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                CoreError::Rpc(format!("vultr: {date} missing/non-numeric outgoing_bytes"))
            })?;
        total += bytes;
    }
    Ok(total)
}

#[async_trait]
impl Provider for VultrProvider {
    fn kind(&self) -> &'static str {
        "vultr"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        // Instance object → monthly quota, for the context line only.
        let inst_path = format!("/instances/{}", self.instance_id);
        let (raw_inst, inst_json) = self
            .get(&inst_path)
            .await
            .map_err(|e| CoreError::Rpc(format!("vultr instance: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "vultr:/instances/:id", &raw_inst)?;
        let allowed_gb = inst_json
            .get("instance")
            .and_then(|i| i.get("allowed_bandwidth"))
            .and_then(Value::as_i64)
            .unwrap_or(0);

        // Bandwidth usage → month-to-date outbound.
        let bw_path = format!("/instances/{}/bandwidth", self.instance_id);
        let (raw_bw, bw_json) = self
            .get(&bw_path)
            .await
            .map_err(|e| CoreError::Rpc(format!("vultr bandwidth: {e}")))?;
        self.db.insert_raw(
            self.provider_id,
            None,
            "vultr:/instances/:id/bandwidth",
            &raw_bw,
        )?;

        let now = Utc::now();
        let prefix = format!("{:04}-{:02}", now.year(), now.month());
        let egress_gb = mtd_outgoing_bytes(&bw_json, &prefix)? / BYTES_PER_GB;

        let acct = Account {
            id: None,
            name: self.local_account_name(),
            kind: "subscription".into(),
            asset_class: None,
            tier: None,
            currency: "GB".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: Some("mtd_egress".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db.insert_snapshot(aid, Cents(egress_gb), "vultr")?;

        let message = if allowed_gb > 0 {
            let pct = egress_gb * 100 / allowed_gb;
            Some(format!("MTD egress: {egress_gb}/{allowed_gb} GB ({pct}%)"))
        } else {
            Some(format!("MTD egress: {egress_gb} GB (quota unknown)"))
        };

        Ok(RefreshReport {
            provider_kind: "vultr".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message,
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        // No per-transaction feed — Vultr bandwidth is a single daily aggregate.
        Ok(RefreshReport {
            provider_kind: "vultr".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("vultr: no transaction feed".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_only_current_month_outgoing() {
        let bw = json!({
            "bandwidth": {
                "2026-04-30": { "incoming_bytes": 999, "outgoing_bytes": 5_000_000_000i64 },
                "2026-05-01": { "incoming_bytes": 10,  "outgoing_bytes": 2_000_000_000i64 },
                "2026-05-02": { "incoming_bytes": 20,  "outgoing_bytes": 3_000_000_000i64 }
            }
        });
        // May only: 2 GB + 3 GB worth of bytes = 5e9; April's 5e9 excluded.
        assert_eq!(mtd_outgoing_bytes(&bw, "2026-05").unwrap(), 5_000_000_000);
        assert_eq!(
            mtd_outgoing_bytes(&bw, "2026-05").unwrap() / BYTES_PER_GB,
            5
        );
    }

    #[test]
    fn missing_bandwidth_object_errors() {
        // A malformed response (no `bandwidth` object) must error, not report a
        // fake-zero egress that would silently never trip the bandwidth.high alert.
        assert!(mtd_outgoing_bytes(&json!({}), "2026-05").is_err());
    }

    #[test]
    fn non_numeric_outgoing_bytes_errors_in_window_only() {
        // A current-month day with a non-numeric value errors (honest failure)...
        let bad = json!({ "bandwidth": { "2026-05-01": { "outgoing_bytes": "lots" } } });
        assert!(mtd_outgoing_bytes(&bad, "2026-05").is_err());
        // ...but a bad value outside the month window is filtered out before the
        // numeric check, so it doesn't poison the current month's sum.
        let off_month = json!({ "bandwidth": { "2026-04-01": { "outgoing_bytes": "x" } } });
        assert_eq!(mtd_outgoing_bytes(&off_month, "2026-05").unwrap(), 0);
    }
}
