//! Library surface for arca-daemon. The binary entrypoint lives in `main.rs`
//! and re-uses these modules; integration tests under `tests/` import directly.

pub mod alerts;
pub mod calendar;
pub mod config;
pub mod http;
pub mod log_writer;
pub mod peercred;
pub mod pledge;
pub mod providers;
pub mod reports;
pub mod rpc;
pub mod scheduler;
pub mod secrets;
