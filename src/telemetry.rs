//! CLI compatibility shim over [`amc_telemetry`].
//!
//! The canonical readers, parsers, identity, and comparison logic live in
//! the `amc-telemetry` workspace crate so inspection, the Rust observer,
//! and runner providers share one implementation. This module preserves the
//! exact legacy JSON shape (`path`, `observedUnixMs`, `files.{value,unknown}`)
//! plus additive versioning; new code should use `amc_telemetry` directly.

pub use amc_telemetry::{Snapshot, now_unix_ms as now, snapshot_legacy as snapshot};
