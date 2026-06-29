//! Plaid provider — /transactions/sync + /accounts/balance/get.
//! All raw responses go to `provider_raw` for audit + bug recovery.
//!
//! Required secrets (in `secrets.age`):
//!   plaid_client_id                         (env-agnostic)
//!   plaid_<env>_secret                      (`plaid_sandbox_secret` / `plaid_production_secret`)
//!   plaid_<label>_access_token              (referenced by `providers.secret_ref`)
//!
//! `providers.config_json` shape:
//! ```json
//! { "plaid_env": "sandbox" | "production",
//!   "institution_name": "First Bank",
//!   "sync_cursor": null | "..." }
//! ```

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use arca_core::db::{Account, Db, ProviderRow, Transaction};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlaidConfig {
    #[serde(default = "default_env")]
    pub plaid_env: String,
    #[serde(default)]
    pub institution_name: Option<String>,
    #[serde(default)]
    pub sync_cursor: Option<String>,
}

fn default_env() -> String {
    "sandbox".into()
}

pub struct PlaidProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    plaid_env: String,
    client_id: String,
    secret: SecretString,
    access_token: SecretString,
    db: Arc<Db>,
}

impl PlaidProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let cfg: PlaidConfig =
            serde_json::from_str(&row.config_json).context("plaid config_json")?;
        let base_url = match cfg.plaid_env.as_str() {
            "sandbox" => "https://sandbox.plaid.com",
            "production" => "https://production.plaid.com",
            other => return Err(anyhow!("unknown plaid_env: {other}")),
        }
        .to_string();
        let client_id = secrets.require("plaid_client_id")?.to_string();
        let secret_key = format!("plaid_{}_secret", cfg.plaid_env);
        let secret = secrets.require_owned(&secret_key)?;
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("plaid provider {} has no secret_ref", row.label))?;
        let access_token = secrets.require_owned(secret_ref)?;

        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url,
            plaid_env: cfg.plaid_env,
            client_id,
            secret,
            access_token,
            db,
        })
    }

    /// Override base_url (used by integration tests + future regional Plaid envs).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn post(&self, path: &str, body: Value) -> AnyResult<(Vec<u8>, Value)> {
        let url = format!("{}{}", self.base_url, path);
        let resp = client()
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("read plaid body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "plaid {path} -> {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("plaid json")?;
        Ok((bytes.to_vec(), json))
    }

    fn auth(&self) -> Value {
        json!({
            "client_id": self.client_id,
            "secret": self.secret.expose_secret(),
            "access_token": self.access_token.expose_secret(),
        })
    }

    fn persist_cursor(&self, new_cursor: Option<&str>) -> Result<()> {
        let cfg = PlaidConfig {
            plaid_env: self.plaid_env.clone(),
            institution_name: None,
            sync_cursor: new_cursor.map(str::to_string),
        };
        let s = serde_json::to_string(&cfg).map_err(CoreError::Json)?;
        self.db.update_provider_config(self.provider_id, &s)
    }

    fn load_cursor(&self) -> Result<Option<String>> {
        // Re-read providers row for current cursor (someone else may have updated it).
        for row in self.db.list_providers()? {
            if row.id == self.provider_id {
                let cfg: PlaidConfig =
                    serde_json::from_str(&row.config_json).map_err(CoreError::Json)?;
                return Ok(cfg.sync_cursor);
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl Provider for PlaidProvider {
    fn kind(&self) -> &'static str {
        "plaid"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        let body = self.auth();
        let (raw, json) = self
            .post("/accounts/balance/get", body)
            .await
            .map_err(|e| CoreError::Rpc(format!("plaid balance: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "plaid:/accounts/balance/get", &raw)?;

        let accounts = json
            .get("accounts")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::Rpc("plaid: no accounts in balance response".into()))?;
        let mut rows = 0u32;
        for a in accounts {
            let ext = a.get("account_id").and_then(Value::as_str).unwrap_or("");
            let name = a.get("name").and_then(Value::as_str).unwrap_or("plaid");
            let plaid_type = a.get("type").and_then(Value::as_str);
            let kind = plaid_kind(plaid_type);
            let asset_class =
                plaid_asset_class(plaid_type, a.get("subtype").and_then(Value::as_str));
            let cur = a
                .get("balances")
                .and_then(|b| b.get("iso_currency_code"))
                .and_then(Value::as_str)
                .unwrap_or("USD")
                .to_string();
            // Plaid returns balances in dollars. A missing/non-numeric balance
            // (Plaid sends null for some items) must error, not snapshot a fake
            // $0 that would corrupt net worth / trip balance.low.
            let balance = pick_balance(plaid_type, a.get("balances")).ok_or_else(|| {
                CoreError::Rpc(format!("plaid account {ext}: missing/non-numeric balance"))
            })?;
            let cents = crate::providers::convert::dollars_to_cents("plaid balance", balance)?;

            let acct = Account {
                id: None,
                name: name.into(),
                kind: kind.into(),
                asset_class: asset_class.map(str::to_string),
                tier: None,
                currency: cur,
                provider_id: Some(self.provider_id),
                business_id: None,
                external_id: Some(ext.into()),
                active: true,
            };
            let aid = self.db.upsert_account(&acct)?;
            self.db.insert_snapshot(aid, cents, "plaid")?;
            rows += 1;
        }
        Ok(RefreshReport {
            provider_kind: "plaid".into(),
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
        let mut cursor = self.load_cursor()?;
        let mut total = 0u32;
        let mut deleted = 0usize;
        loop {
            let mut body = self.auth();
            if let Some(c) = &cursor {
                body["cursor"] = json!(c);
            }
            body["count"] = json!(500);

            let (raw, json) = self
                .post("/transactions/sync", body)
                .await
                .map_err(|e| CoreError::Rpc(format!("plaid sync: {e}")))?;
            self.db
                .insert_raw(self.provider_id, None, "plaid:/transactions/sync", &raw)?;

            let added = json
                .get("added")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let modified = json
                .get("modified")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next_cursor = json
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            let removed = json
                .get("removed")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let has_more = json
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            for tx in added.iter().chain(modified.iter()) {
                if self.upsert_txn(tx)? {
                    total += 1;
                }
            }
            // Plaid reports cancelled/superseded transactions (pending→posted
            // churn, recategorization) in `removed[]`; delete them so stale rows
            // don't double-count against recurring detection and reports.
            for tx in &removed {
                if let Some(id) = tx.get("transaction_id").and_then(Value::as_str) {
                    deleted += self.db.delete_transaction_by_external(id)?;
                }
            }

            cursor = next_cursor;
            // Checkpoint the cursor right after this page's adds/modifies/removes
            // commit, before fetching the next page — so a mid-pagination failure
            // resumes from the last fully-applied page instead of re-walking from
            // the stale stored cursor (Plaid's per-page-persist contract).
            // Idempotent: a re-applied page upserts (dedup) and deletes by
            // external_id, so no double-count.
            if let Some(c) = &cursor {
                self.persist_cursor(Some(c))?;
            }
            if !has_more {
                break;
            }
        }
        Ok(RefreshReport {
            provider_kind: "plaid".into(),
            rows_written: total,
            cursor: Cursor(cursor),
            message: (deleted > 0).then(|| format!("removed {deleted} stale transaction(s)")),
        })
    }
}

impl PlaidProvider {
    fn upsert_txn(&self, tx: &Value) -> Result<bool> {
        let plaid_acct = tx
            .get("account_id")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Rpc("plaid txn missing account_id".into()))?;
        let account = self
            .db
            .find_account_by_external(self.provider_id, plaid_acct)?
            .ok_or_else(|| {
                CoreError::NotFound(format!(
                    "plaid account {plaid_acct} — run refresh_balances first"
                ))
            })?;
        let aid = account.id.expect("account from DB has id");

        let txn_id = tx
            .get("transaction_id")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Rpc("plaid txn missing transaction_id".into()))?;
        // Mandatory fields error rather than coerce: a missing amount must not
        // become a $0 row, and a missing/bad date must not become a 1970 row
        // (either would poison cash-flow buckets and recurring detection).
        let amount = tx.get("amount").and_then(Value::as_f64).ok_or_else(|| {
            CoreError::Rpc(format!("plaid txn {txn_id}: missing/non-numeric amount"))
        })?;
        // Plaid amount is positive for outflows; invert to match bank convention.
        let cents = crate::providers::convert::dollars_to_cents("plaid txn", -amount)?;
        let date = tx
            .get("date")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Rpc(format!("plaid txn {txn_id}: missing date")))?;
        let posted_at = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|e| CoreError::Rpc(format!("plaid txn {txn_id}: bad date {date}: {e}")))?
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| CoreError::Rpc(format!("plaid txn {txn_id}: bad date {date}")))?
            .and_utc()
            .timestamp();
        let description = tx
            .get("merchant_name")
            .or_else(|| tx.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // Prefer the current taxonomy (`personal_finance_category.primary`, e.g.
        // "FOOD_AND_DRINK"); the legacy top-level `category` array is deprecated and
        // lands null on `/transactions/sync` for newly-linked items.
        let category = tx
            .get("personal_finance_category")
            .and_then(|p| p.get("primary"))
            .and_then(Value::as_str)
            .map(normalize_pfc)
            .or_else(|| {
                tx.get("category")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let currency = tx
            .get("iso_currency_code")
            .and_then(Value::as_str)
            .unwrap_or("USD")
            .to_string();

        let trow = Transaction {
            id: None,
            account_id: aid,
            posted_at,
            amount_cents: cents,
            currency,
            description,
            category,
            tag: None,
            business_id: account.business_id,
            external_id: Some(txn_id.into()),
            source: "plaid".into(),
        };
        self.db.upsert_transaction(&trow)?;
        Ok(true)
    }
}

/// The dollar balance arca records for a Plaid account. Depository accounts use
/// `available` (spendable, net of pending holds — matches the bank UI and keeps
/// net worth from overstating cash by holds that haven't posted), falling back
/// to `current` when Plaid omits `available`. Credit/loan/investment use
/// `current` (amount owed / position value; `available` on a credit line is
/// remaining credit, not a balance).
fn pick_balance(plaid_type: Option<&str>, balances: Option<&Value>) -> Option<f64> {
    let current = || {
        balances
            .and_then(|b| b.get("current"))
            .and_then(Value::as_f64)
    };
    if plaid_type == Some("depository") {
        balances
            .and_then(|b| b.get("available"))
            .and_then(Value::as_f64)
            .or_else(current)
    } else {
        current()
    }
}

fn plaid_kind(plaid_type: Option<&str>) -> &'static str {
    match plaid_type {
        Some("credit" | "loan") => "debt",
        Some("investment" | "brokerage") => "brokerage",
        _ => "asset",
    }
}

fn plaid_asset_class(plaid_type: Option<&str>, subtype: Option<&str>) -> Option<&'static str> {
    match (plaid_type, subtype) {
        (Some("depository"), Some("checking" | "savings" | "cd" | "money market")) => Some("cash"),
        (Some("investment" | "brokerage"), _) => Some("stocks"),
        _ => None,
    }
}

/// Turn a Plaid `personal_finance_category.primary` enum
/// (`FOOD_AND_DRINK`) into a display string (`Food and drink`). Deterministic so
/// the same primary always groups under one label in the categories view.
fn normalize_pfc(primary: &str) -> String {
    let lower = primary.trim().to_lowercase().replace('_', " ");
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => lower,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pfc_humanizes_enum() {
        assert_eq!(normalize_pfc("FOOD_AND_DRINK"), "Food and drink");
        assert_eq!(normalize_pfc("TRANSPORTATION"), "Transportation");
        assert_eq!(normalize_pfc("RENT_AND_UTILITIES"), "Rent and utilities");
        assert_eq!(normalize_pfc(""), "");
    }

    #[test]
    fn category_prefers_pfc_over_legacy_array() {
        let tx = json!({
            "personal_finance_category": { "primary": "GENERAL_MERCHANDISE" },
            "category": ["Shops", "Supermarkets"]
        });
        let cat = tx
            .get("personal_finance_category")
            .and_then(|p| p.get("primary"))
            .and_then(Value::as_str)
            .map(normalize_pfc)
            .or_else(|| {
                tx.get("category")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        assert_eq!(cat.as_deref(), Some("General merchandise"));
    }

    #[test]
    fn category_falls_back_to_legacy_array_when_pfc_absent() {
        let tx = json!({ "category": ["Travel", "Airlines"] });
        let cat = tx
            .get("personal_finance_category")
            .and_then(|p| p.get("primary"))
            .and_then(Value::as_str)
            .map(normalize_pfc)
            .or_else(|| {
                tx.get("category")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        assert_eq!(cat.as_deref(), Some("Travel"));
    }

    #[test]
    fn depository_prefers_available_over_current() {
        let b = json!({ "available": 900.00, "current": 1000.00 });
        assert_eq!(pick_balance(Some("depository"), Some(&b)), Some(900.00));
    }

    #[test]
    fn depository_falls_back_to_current_when_available_null() {
        let b = json!({ "available": null, "current": 1000.00 });
        assert_eq!(pick_balance(Some("depository"), Some(&b)), Some(1000.00));
    }

    #[test]
    fn credit_uses_current_ignoring_available() {
        let b = json!({ "available": 4750.00, "current": 250.00 });
        assert_eq!(pick_balance(Some("credit"), Some(&b)), Some(250.00));
    }

    #[test]
    fn missing_balance_yields_none() {
        let b = json!({});
        assert_eq!(pick_balance(Some("depository"), Some(&b)), None);
        assert_eq!(pick_balance(Some("credit"), Some(&b)), None);
    }
}
