//! Memory admission and managed execution for AMC.
//!
//! Pipelines that fan out a large number of memory-hungry tasks (archive
//! extraction, video decode, scientific batch jobs) can drive the host into
//! swap thrashing or OOM territory long before they exhaust thread pools.
//! `amc-runner` provides an admission gate that pauses *new* task
//! admissions when host RAM usage crosses a configurable threshold, then
//! resumes once usage falls back below a hysteresis band.
//!
//! Two flavours are provided:
//!
//! - `sync::AdmissionGate` — backed by `std::sync::Condvar`. Designed to wrap
//!   Rayon parallel iteration so blocking acquisition costs zero scheduler
//!   wakeups.
//! - `async::AdmissionGate` — backed by `tokio::sync::Notify`. Designed for
//!   `futures::stream::buffer_unordered` consumers and other Tokio task fanout.
//!
//! Both gates share the same configuration and the same memory provider
//! abstraction, so a single project can mix the two.
//!
//! ## Memory providers
//!
//! A [`MemoryProvider`] returns the current "used" fraction of host RAM as a
//! `f64` in the inclusive range `[0.0, 1.0]`. The crate ships:
//!
//! - [`providers::ProcMeminfoProvider`] — Linux-only, reads `/proc/meminfo`.
//!   Cheap and always-current.
//! - `providers::SysinfoProvider` — cross-platform, gated by the `sysinfo`
//!   feature.
//! - [`providers::FixedProvider`] — for tests.
//!
//! A custom provider can be supplied for any other source (cgroup memory peak,
//! NUMA-node specific stats, mocks).
//! Use [`providers::default_provider`] for the crate's platform-aware default.
//!
//! ## Hysteresis
//!
//! When usage exceeds [`Config::max_ram_fraction`] the gate enters the
//! throttled state. It only leaves throttled once usage has dropped to
//! `(max_ram_fraction - resume_hysteresis)`. This prevents oscillation when
//! task release frees just enough memory to admit another task that would
//! immediately push usage back over the line.
//!
//! ## Provider failure
//!
//! By default (`fail_open_on_provider_error = false`) a provider error
//! returns [`AdmitError::Provider`] and does not admit work. Set the flag
//! to restore advisory fail-open: the gate logs once and proceeds without
//! memory throttling.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
#[cfg(feature = "sync")]
pub mod coordinator;
pub mod provider;
pub mod providers;
pub mod weighted;

#[cfg(all(feature = "systemd", target_os = "linux"))]
pub mod systemd;

#[cfg(feature = "sync")]
pub mod sync;

#[cfg(feature = "async")]
pub mod r#async;

pub use config::{Config, ConfigError};
pub use provider::{MemoryDomain, MemoryProvider, MemoryStats, ProviderError, finite_fraction};

/// Why an admission attempt did not grant a permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitError {
    /// Declared weight cannot fit even with an empty gate.
    Impossible {
        /// Requested reservation in bytes.
        weight: u64,
        /// Bytes that could be granted right now (may be zero).
        budget: u64,
    },
    /// Memory provider failed and fail-open is disabled.
    Provider(ProviderError),
    /// Timed out while waiting for capacity.
    TimedOut,
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Impossible { weight, budget } => {
                write!(f, "reservation {weight} exceeds available budget {budget}")
            }
            Self::Provider(error) => write!(f, "memory provider failed: {error}"),
            Self::TimedOut => f.write_str("timed out waiting for memory admission"),
        }
    }
}

impl std::error::Error for AdmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error),
            Self::Impossible { .. } | Self::TimedOut => None,
        }
    }
}
