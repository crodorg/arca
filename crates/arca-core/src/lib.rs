//! arca-core: shared types, DB layer, provider trait, and RPC schema.

pub mod calendar;
pub mod db;
pub mod debt;
pub mod demo;
pub mod error;
pub mod ids;
pub mod money;
pub mod pp;
pub mod provider;
pub mod recurring;
pub mod report;
pub mod rpc;
pub mod time;

pub use error::CoreError;
pub use ids::{AccountId, BusinessId, ProviderId, TransactionId};
pub use money::Cents;
