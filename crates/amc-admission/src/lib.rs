//! Durable, per-user admission. Native systemd units own workload lifetimes.
#![forbid(unsafe_code)]

pub mod ledger;
pub mod native;
pub mod protocol;
pub mod server;
pub mod store;
