//! Durable, per-user admission. Native systemd units own workload lifetimes.
#![forbid(unsafe_code)]

pub mod clock;
pub mod health;
pub mod host;
pub mod host_native;
pub mod host_server;
pub mod ledger;
pub mod native;
pub mod protocol;
pub mod server;
pub mod store;
