//! Postmark usage provider — outbound message count, month-to-date.
//!
//! Required secrets:
//!   postmark_<label>_account_token   (X-Postmark-Account-Token)
//!
//! Postmark's account API exposes servers and account info, but per-server
//! stats need a server token. v1 flow:
//!   1. GET /servers           (account token)
//!   2. For each server, GET /stats/outbound (using each server's ApiTokens[0])
//!   3. Sum Sent across servers for the calendar month.
//!
//! The snapshot value is a **message count**, not USD. `currency = "MESSAGES"`
//! signals the unit to the display layer.

use std::sync::Arc;

use anyhow::{Context, Result as AnyResult, anyhow};
use async_trait::async_trait;
use chrono::{Datelike, Utc};
use serde_json::Value;

use arca_core::db::{Account, Db, ProviderRow};
use arca_core::error::{CoreError, Result};
use arca_core::ids::ProviderId;
use arca_core::money::Cents;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};
use secrecy::{ExposeSecret, SecretString};

use crate::http::client;
use crate::secrets::Secrets;

const BASE_URL: &str = "https://api.postmarkapp.com";

pub struct PostmarkProvider {
    provider_id: ProviderId,
    label: String,
    base_url: String,
    account_token: SecretString,
    db: Arc<Db>,
}

impl PostmarkProvider {
    pub fn build(row: &ProviderRow, secrets: &Secrets, db: Arc<Db>) -> AnyResult<Self> {
        let secret_ref = row
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("postmark provider {} has no secret_ref", row.label))?;
        let account_token = secrets.require_owned(secret_ref)?;
        Ok(Self {
            provider_id: row.id,
            label: row.label.clone(),
            base_url: BASE_URL.into(),
            account_token,
            db,
        })
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn get_with(
        &self,
        path: &str,
        token_header: &str,
        token: &str,
    ) -> AnyResult<(Vec<u8>, Value)> {
        let url = format!("{}{}", self.base_url, path);
        let resp = client()
            .get(&url)
            .header(token_header, token)
            .header("Accept", "application/json")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("postmark body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "postmark {path} -> {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let json: Value = serde_json::from_slice(&bytes).context("postmark json")?;
        Ok((bytes.to_vec(), json))
    }

    fn local_account_name(&self) -> String {
        format!("Postmark — {} (msgs)", self.label)
    }
}

#[async_trait]
impl Provider for PostmarkProvider {
    fn kind(&self) -> &'static str {
        "postmark"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        // List servers (account-level).
        let (raw_servers, servers_json) = self
            .get_with(
                "/servers?count=200&offset=0",
                "X-Postmark-Account-Token",
                self.account_token.expose_secret(),
            )
            .await
            .map_err(|e| CoreError::Rpc(format!("postmark servers: {e}")))?;
        self.db
            .insert_raw(self.provider_id, None, "postmark:/servers", &raw_servers)?;

        let servers = servers_json
            .get("Servers")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::Rpc("postmark: no Servers array".into()))?;

        // First day of current month (UTC) → today.
        let now = Utc::now();
        let from = format!("{:04}-{:02}-01", now.year(), now.month());
        let to = now.format("%Y-%m-%d").to_string();

        let mut total_sent: i64 = 0;
        let mut failed: u32 = 0;
        for s in servers {
            let token = s
                .get("ApiTokens")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str);
            let Some(token) = token else { continue };
            let path = format!("/stats/outbound?fromdate={from}&todate={to}");
            let (raw, stats) = match self.get_with(&path, "X-Postmark-Server-Token", token).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "postmark server stats");
                    failed += 1;
                    continue;
                }
            };
            let server_name = s.get("Name").and_then(Value::as_str).unwrap_or("");
            self.db.insert_raw(
                self.provider_id,
                Some(server_name),
                "postmark:/stats/outbound",
                &raw,
            )?;
            if let Some(sent) = stats.get("Sent").and_then(Value::as_i64) {
                total_sent += sent;
            }
        }

        let acct = Account {
            id: None,
            name: self.local_account_name(),
            kind: "subscription".into(),
            asset_class: None,
            tier: None,
            currency: "MESSAGES".into(),
            provider_id: Some(self.provider_id),
            business_id: None,
            external_id: Some("mtd_sent".into()),
            active: true,
        };
        let aid = self.db.upsert_account(&acct)?;
        self.db
            .insert_snapshot(aid, Cents(total_sent), "postmark")?;

        Ok(RefreshReport {
            provider_kind: "postmark".into(),
            rows_written: 1,
            cursor: Cursor::default(),
            message: Some(if failed > 0 {
                format!(
                    "MTD sent: {total_sent} messages ({failed} server(s) errored — count understated)"
                )
            } else {
                format!("MTD sent: {total_sent} messages")
            }),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        // Per-message rows are intentionally not pulled — too many for v1.
        Ok(RefreshReport {
            provider_kind: "postmark".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("postmark: per-message feed not enabled in v1".into()),
        })
    }
}
