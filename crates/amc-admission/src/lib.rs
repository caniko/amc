//! Durable, per-user admission. Native systemd units own workload lifetimes.
#![forbid(unsafe_code)]

pub mod clock;
pub mod continuation;
pub mod health;
pub mod host;
pub mod host_native;
pub mod host_server;
mod host_transaction;
pub mod ledger;
pub mod native;
pub mod page_discovery;
pub mod page_return;
pub mod preparation;
pub mod protocol;
pub mod recovery;
mod root_pool;
pub mod server;
pub mod store;
pub mod swap;
