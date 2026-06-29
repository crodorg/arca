//! Vultr cost provider — monthly USD run-rate across the whole Vultr account.
//!
//! Answers "how much am I spending on VPSs?" account-wide, so adding another
//! instance is tracked automatically — no per-instance config to touch. Two
//! endpoints, joined on the plan id (the model is lifted from the helm Vultr
//! overlay at `a sibling project overlay`, but transport is `reqwest`, not a
//! shelled-out `curl`: the daemon pledges without `proc`/`exec`):
//!
//!   GET /v2/instances?per_page=500  → the fleet (each carries a `plan` id)
//!   GET /v2/plans?per_page=500      → plan catalog (plan id → monthly_cost USD)
//!
//! Sum each live instance's plan `monthly_cost` → total monthly run-rate, written
//! as a USD `balance_snapshot` on a `subscription`-kind account. `subscription`
//! kind keeps it out of net worth (it's a recurring cost, not an asset) while
//! still surfacing it in the money snapshot's subscriptions section and the
//! monthly report's API/subscription table — same treatment as openrouter et al.
//!
//! This is a forward run-rate (what the current fleet costs per month), not
//! month-to-date actuals; for VPS burn that is the more useful, stable figure and
//! it's what the existing helm overlay already computed. The separate `vultr`
//! provider tracks one instance's egress GB for `bandwidth.high` — orthogonal.
//!
//! Required secrets:
//!   vultr_<label>_api_key   (Bearer; referenced by `providers.secret_ref`)
//!
//! `providers.config_json` shape (account_name optional, defaults below):
//! ```json
//! { "account_name": "Vultr VPS hosting" }
//! ```

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
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
const DEFAULT_ACCOUNT_NAME: &str = "Vultr VPS hosting";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VultrCostConfig {
    /// Local account the run-rate snapshot lands on. Defaults to
    /// `Vultr VPS hosting` so the operator need not set it.
    #[serde(default)]
    pub account_name: Option<String>,
}

/// One compute instance from `GET /v2/instances`. Only the fields we join on are
/// deserialized; the rest of Vultr's object is dropped.
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Instance {
    plan: String,
    #[serde(default)]
    label: String,
}

/// One plan (compute SKU) from `GET /v2/plans`. `monthly_cost` is USD/month.
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Plan {
    id: String,
    monthly_cost: f64,
}

pub struct VultrCostProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    api_key: SecretString,
    account_name: String,
    db: Arc<Db>,
}

impl VultrCostProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let cfg: VultrCostConfig =
            serde_json::from_str(&row.config_json).context("vultr_cost config_json")?;
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("vultr_cost provider {} has no secret_ref", row.label))?;
        let api_key = secrets.require_owned(secret_ref)?;
        let account_name = cfg
            .account_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            api_key,
            account_name,
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
}

/// Parse the `instances` array out of `GET /v2/instances`.
fn parse_instances(json: &Value) -> Result<Vec<Instance>> {
    serde_json::from_value(
        json.get("instances")
            .cloned()
            .ok_or_else(|| CoreError::Rpc("vultr: response missing `instances` array".into()))?,
    )
    .map_err(|e| CoreError::Rpc(format!("vultr instances: {e}")))
}

/// Parse the `plans` array out of `GET /v2/plans`.
fn parse_plans(json: &Value) -> Result<Vec<Plan>> {
    serde_json::from_value(
        json.get("plans")
            .cloned()
            .ok_or_else(|| CoreError::Rpc("vultr: response missing `plans` array".into()))?,
    )
    .map_err(|e| CoreError::Rpc(format!("vultr plans: {e}")))
}

/// Convert a USD dollar amount to integer cents, rejecting non-finite or
/// negative values rather than coercing them — a garbage `monthly_cost` must
/// trip `provider.stale`, not silently land a fabricated cost (honest failure).
fn dollars_to_cents(dollars: f64) -> Result<i64> {
    if !dollars.is_finite() || dollars < 0.0 {
        return Err(CoreError::Rpc(format!(
            "vultr: implausible monthly_cost {dollars}"
        )));
    }
    Ok((dollars * 100.0).round() as i64)
}

/// Join the fleet against the plan catalog → total monthly run-rate in cents.
/// Errors (rather than skipping) when an instance's `plan` id is absent from the
/// catalog: a silently-dropped instance would understate the bill and is exactly
/// the kind of fabricated-low number `bandwidth.high`/the report must never show.
/// Pure, so it unit-tests without a live API. Returns `(total_cents, count)`.
fn monthly_cost_cents(instances: &[Instance], plans: &[Plan]) -> Result<(i64, usize)> {
    let mut total: i64 = 0;
    for inst in instances {
        let plan = plans.iter().find(|p| p.id == inst.plan).ok_or_else(|| {
            CoreError::Rpc(format!(
                "vultr: instance {} on unknown plan {}",
                if inst.label.is_empty() {
                    "<unlabeled>"
                } else {
                    &inst.label
                },
                inst.plan
            ))
        })?;
        total += dollars_to_cents(plan.monthly_cost)?;
    }
    Ok((total, instances.len()))
}

#[async_trait]
impl Provider for VultrCostProvider {
    fn kind(&self) -> &'static str {
        "vultr_cost"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        // per_page=500 pulls the whole fleet + plan catalog in one request each
        // (well under Vultr's 500 max for both). If the catalog were ever
        // truncated past 500, a referenced-but-missing plan surfaces as an error
        // in the join below rather than an undercounted bill.
        let (raw_inst, inst_json) = self
            .get("/instances?per_page=500")
            .await
            .map_err(|e| CoreError::Rpc(format!("vultr instances: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "vultr:/instances", &raw_inst)?;

        let (raw_plans, plans_json) = self
            .get("/plans?per_page=500")
            .await
            .map_err(|e| CoreError::Rpc(format!("vultr plans: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "vultr:/plans", &raw_plans)?;

        let instances = parse_instances(&inst_json)?;
        let plans = parse_plans(&plans_json)?;
        let (total_cents, count) = monthly_cost_cents(&instances, &plans)?;

        let acct = Account {
            id: None,
            name: self.account_name.clone(),
            kind: "subscription".into(),
            asset_class: None,
            tier: None,
            currency: "USD".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: Some("monthly_cost".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db
            .insert_snapshot(aid, Cents(total_cents), "vultr_cost")?;

        let message = Some(format!(
            "{count} instance{} · ${:.2}/mo run-rate",
            if count == 1 { "" } else { "s" },
            total_cents as f64 / 100.0
        ));

        Ok(RefreshReport {
            provider_kind: "vultr_cost".into(),
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
        // Run-rate is a single derived aggregate — no per-transaction feed.
        Ok(RefreshReport {
            provider_kind: "vultr_cost".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("vultr_cost: no transaction feed".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fleet() -> Value {
        json!({
            "instances": [
                { "id": "a", "plan": "vc2-1c-1gb", "label": "saas" },
                { "id": "b", "plan": "vc2-2c-4gb", "label": "trader" },
                { "id": "c", "plan": "vc2-1c-1gb", "label": "hermes" }
            ],
            "meta": { "total": 3 }
        })
    }

    fn catalog() -> Value {
        json!({
            "plans": [
                { "id": "vc2-1c-1gb", "monthly_cost": 6.0 },
                { "id": "vc2-2c-4gb", "monthly_cost": 20.0 },
                { "id": "vhf-1c-2gb", "monthly_cost": 12.0 }
            ],
            "meta": { "total": 3 }
        })
    }

    #[test]
    fn sums_run_rate_across_fleet() {
        let inst = parse_instances(&fleet()).unwrap();
        let plans = parse_plans(&catalog()).unwrap();
        let (cents, count) = monthly_cost_cents(&inst, &plans).unwrap();
        // 6 + 20 + 6 = $32.00
        assert_eq!(cents, 3_200);
        assert_eq!(count, 3);
    }

    #[test]
    fn unknown_plan_errors_not_undercounts() {
        // An instance on a plan absent from the catalog must error (honest
        // failure → provider.stale), never silently drop and understate the bill.
        let inst = parse_instances(&json!({
            "instances": [{ "id": "a", "plan": "ghost-plan", "label": "x" }]
        }))
        .unwrap();
        let plans = parse_plans(&catalog()).unwrap();
        assert!(monthly_cost_cents(&inst, &plans).is_err());
    }

    #[test]
    fn empty_fleet_is_zero() {
        let inst = parse_instances(&json!({ "instances": [] })).unwrap();
        let plans = parse_plans(&catalog()).unwrap();
        assert_eq!(monthly_cost_cents(&inst, &plans).unwrap(), (0, 0));
    }

    #[test]
    fn fractional_dollars_round_to_cents() {
        assert_eq!(dollars_to_cents(3.50).unwrap(), 350);
        assert_eq!(dollars_to_cents(5.005).unwrap(), 501);
        assert_eq!(dollars_to_cents(0.0).unwrap(), 0);
    }

    #[test]
    fn implausible_cost_errors() {
        assert!(dollars_to_cents(f64::NAN).is_err());
        assert!(dollars_to_cents(f64::INFINITY).is_err());
        assert!(dollars_to_cents(-1.0).is_err());
    }

    #[test]
    fn missing_arrays_error() {
        assert!(parse_instances(&json!({})).is_err());
        assert!(parse_plans(&json!({})).is_err());
    }
}
