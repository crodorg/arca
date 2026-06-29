//! Registry: build provider impls from the `providers` table at startup.
//!
//! Strategy:
//! - The `manual` provider always exists (seeded if missing, no secrets needed).
//! - For every other row, look up its kind in `FACTORIES` and call the builder.
//! - A row with an unknown kind, or with missing secrets, is logged and skipped —
//!   one bad row doesn't take the daemon down.

use std::sync::Arc;

use arca_core::db::{Db, ProviderRow};
use arca_core::provider::Provider;

use super::gold_spot::GoldSpotProvider;
use super::manual::ManualProvider;
use super::mercury::MercuryProvider;
use super::openai_usage::OpenAiUsageProvider;
use super::openrouter::OpenRouterProvider;
use super::plaid::PlaidProvider;
use super::postmark::PostmarkProvider;
use super::scrapecreators::ScrapeCreatorsProvider;
use super::stripe::StripeProvider;
use super::vultr::VultrProvider;
use super::vultr_cost::VultrCostProvider;
use super::xmr_spot::XmrSpotProvider;
use crate::secrets::Secrets;

pub struct LoadedProvider {
    pub row: ProviderRow,
    pub impl_: Arc<dyn Provider>,
}

/// Provider kinds the registry can build. Used to reject a typo'd `kind` at
/// `manual.upsert_provider` time rather than silently skipping the row at load.
/// `xai` is intentionally absent (deferred — no usage API), so a row for it is
/// rejected until the provider lands.
pub const KNOWN_KINDS: &[&str] = &[
    "manual",
    "plaid",
    "mercury",
    "stripe",
    "openrouter",
    "openai_usage",
    "scrapecreators",
    "postmark",
    "gold_spot",
    "vultr",
    "vultr_cost",
    "xmr_spot",
];

pub fn load(db: &Arc<Db>, secrets: &Secrets) -> anyhow::Result<Vec<LoadedProvider>> {
    // Make sure a `manual` row exists.
    db.upsert_provider(&ProviderRow::registry_stub("manual", "manual"))?;

    let rows = db.list_providers()?;
    let mut out: Vec<LoadedProvider> = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = row.kind.clone();
        let label = row.label.clone();
        let built: anyhow::Result<Arc<dyn Provider>> = match kind.as_str() {
            "manual" => Ok(Arc::new(ManualProvider::new())),
            "plaid" => PlaidProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "mercury" => MercuryProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "stripe" => StripeProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "openrouter" => OpenRouterProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "openai_usage" => OpenAiUsageProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "scrapecreators" => ScrapeCreatorsProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "postmark" => PostmarkProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "gold_spot" => GoldSpotProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "vultr" => VultrProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "vultr_cost" => VultrCostProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            "xmr_spot" => XmrSpotProvider::build(&row, secrets, Arc::clone(db))
                .map(|p| Arc::new(p) as Arc<dyn Provider>),
            other => Err(anyhow::anyhow!("unknown provider kind: {other}")),
        };
        match built {
            Ok(p) => {
                tracing::info!(kind = %kind, label = %label, "provider loaded");
                out.push(LoadedProvider { row, impl_: p });
            }
            Err(e) => {
                tracing::warn!(kind = %kind, label = %label, error = %e, "provider skipped");
            }
        }
    }
    Ok(out)
}
