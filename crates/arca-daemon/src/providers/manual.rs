//! `manual` provider — data flows in via RPC, not from any external source.
//! Polling is a no-op; this exists so the registry isn't empty in Phase 1.

use async_trait::async_trait;

use arca_core::error::Result;
use arca_core::provider::{Ctx, Cursor, Provider, RefreshReport};

pub struct ManualProvider {
    label: String,
}

impl ManualProvider {
    pub fn new() -> Self {
        Self {
            label: "manual".into(),
        }
    }
}

impl Default for ManualProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ManualProvider {
    fn kind(&self) -> &'static str {
        "manual"
    }

    fn label(&self) -> &str {
        &self.label
    }

    async fn refresh_balances(&self, _ctx: &Ctx) -> Result<RefreshReport> {
        Ok(RefreshReport {
            provider_kind: "manual".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("manual provider: no remote source".into()),
        })
    }

    async fn refresh_transactions(
        &self,
        _ctx: &Ctx,
        _cursor: Option<Cursor>,
    ) -> Result<RefreshReport> {
        Ok(RefreshReport {
            provider_kind: "manual".into(),
            rows_written: 0,
            cursor: Cursor::default(),
            message: Some("manual provider: no remote source".into()),
        })
    }
}
