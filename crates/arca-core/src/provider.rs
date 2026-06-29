//! Provider trait — sole contract for adding a new data source.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::Result;
use crate::money::Cents;

/// Opaque resumption marker. Providers define what this means internally.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor(pub Option<String>);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RefreshReport {
    pub provider_kind: String,
    pub rows_written: u32,
    pub cursor: Cursor,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageReport {
    pub provider_kind: String,
    pub mtd_cost: Cents,
    pub message: Option<String>,
}

/// Refresh context passed to every provider call.
pub struct Ctx {
    pub db: Arc<Db>,
    pub span: tracing::Span,
}

impl Ctx {
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            span: tracing::Span::current(),
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable identifier — matches `providers.kind` in DB.
    fn kind(&self) -> &'static str;

    /// Human-readable label for logs and TUI.
    fn label(&self) -> &str;

    /// Refresh balances. Idempotent. Writes to balance_snapshots.
    async fn refresh_balances(&self, ctx: &Ctx) -> Result<RefreshReport>;

    /// Pull transactions since `cursor`. Idempotent via external_id upserts.
    async fn refresh_transactions(
        &self,
        ctx: &Ctx,
        cursor: Option<Cursor>,
    ) -> Result<RefreshReport>;

    /// For usage-based providers (Anthropic, OpenAI): MTD spend in cents.
    /// Default: returns None.
    async fn refresh_usage(&self, _ctx: &Ctx) -> Result<Option<UsageReport>> {
        Ok(None)
    }
}
