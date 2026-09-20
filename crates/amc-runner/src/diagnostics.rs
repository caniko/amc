//! Truthful, privacy-preserving admission diagnostics.
//!
//! Libraries never install global subscribers. All events here are emitted
//! via `tracing` with stable names and sanitized reason codes; a downstream
//! binary installs its own subscriber (see `examples/`).
//!
//! Rules:
//! - `ProviderError::Source` text is never logged or exported. Use
//!   [`sanitize_provider_error`] reason codes instead.
//! - Diagnostic sequence/state is assigned under the admission lock; formatting
//!   and sink work happen outside the lock.
//! - Per-request detail is opt-in and cardinality-bounded; exporters stay out
//!   of this milestone (no Prometheus/OTLP).

use crate::provider::ProviderError;

/// Stable event names for admission decisions.
pub mod event {
    /// Gate-global pressure threshold entered.
    pub const THROTTLE_ENTERED: &str = "amc.admission.throttle_entered";
    /// Gate-global pressure cleared and a request admitted.
    pub const RESUMED: &str = "amc.admission.resumed";
    /// A request was admitted.
    pub const ADMITTED: &str = "amc.admission.admitted";
    /// A request is waiting with a specific reason.
    pub const WAIT: &str = "amc.admission.wait";
    /// A memory provider probe failed.
    pub const PROVIDER_FAILED: &str = "amc.admission.provider_failed";
    /// Gate is running degraded (fail-open, thread-cap-only).
    pub const DEGRADED: &str = "amc.admission.degraded";
    /// An admission attempt timed out.
    pub const TIMEOUT: &str = "amc.admission.timeout";
    /// A weight cannot fit even alone.
    pub const IMPOSSIBLE: &str = "amc.admission.impossible";
}

/// Why a request is waiting. Gate-global hysteresis (`Pressure`) is distinct
/// from request-specific budget/exclusivity blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    /// Global used-fraction above max (or above resume while throttled).
    Pressure,
    /// Projected usage with this weight would exceed max.
    Budget,
    /// Exclusive (oversized) job in flight or requested while busy.
    Exclusivity,
    /// Page-cache fraction above threshold.
    PageCache,
    /// Provider failed; running degraded (fail-open).
    ProviderDegraded,
    /// Cache accounting unavailable; policy check skipped.
    CacheUnknown,
}

/// Bounded parked-request counts by wait reason. Fixed shape: no
/// allocation, no cardinality risk. Totals equal the `waiters` count in
/// the snapshot; per-reason splits stay explainable when several
/// requests block for different reasons at once.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WaitersByReason {
    /// Parked on gate-global pressure.
    pub pressure: usize,
    /// Parked on request byte budget.
    pub budget: usize,
    /// Parked on oversized exclusivity.
    pub exclusivity: usize,
    /// Parked on page-cache pressure.
    pub page_cache: usize,
}

impl WaitersByReason {
    /// Total parked requests across reasons.
    #[must_use]
    pub fn total(self) -> usize {
        self.pressure
            .saturating_add(self.budget)
            .saturating_add(self.exclusivity)
            .saturating_add(self.page_cache)
    }

    /// Increment the bucket for `reason`. Reasons without a bucket
    /// (`ProviderDegraded`, `CacheUnknown`) are never parked states.
    pub fn inc(&mut self, reason: WaitReason) {
        match reason {
            WaitReason::Pressure => self.pressure = self.pressure.saturating_add(1),
            WaitReason::Budget => self.budget = self.budget.saturating_add(1),
            WaitReason::Exclusivity => self.exclusivity = self.exclusivity.saturating_add(1),
            WaitReason::PageCache => self.page_cache = self.page_cache.saturating_add(1),
            WaitReason::ProviderDegraded | WaitReason::CacheUnknown => {}
        }
    }

    /// Decrement the bucket for `reason`, saturating.
    pub fn dec(&mut self, reason: WaitReason) {
        match reason {
            WaitReason::Pressure => self.pressure = self.pressure.saturating_sub(1),
            WaitReason::Budget => self.budget = self.budget.saturating_sub(1),
            WaitReason::Exclusivity => {
                self.exclusivity = self.exclusivity.saturating_sub(1);
            }
            WaitReason::PageCache => self.page_cache = self.page_cache.saturating_sub(1),
            WaitReason::ProviderDegraded | WaitReason::CacheUnknown => {}
        }
    }
}

impl WaitReason {
    /// Stable reason code for logs and snapshots.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pressure => "pressure",
            Self::Budget => "budget",
            Self::Exclusivity => "exclusivity",
            Self::PageCache => "page-cache",
            Self::ProviderDegraded => "provider-degraded",
            Self::CacheUnknown => "cache-unknown",
        }
    }
}

/// Scheduler mode for snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerMode {
    /// Memory provider drives admission.
    Provider,
    /// Scheduler disabled at construction.
    Disabled,
    /// Provider failed; thread-cap-only fallback.
    ThreadCapOnly,
}

impl SchedulerMode {
    /// Stable mode code for snapshots.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Disabled => "disabled",
            Self::ThreadCapOnly => "thread-cap-only",
        }
    }
}

/// Consistent diagnostics snapshot: permits, committed bytes, mode, current
/// wait reasons, transition/error counts, wait-duration accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticsSnapshot {
    /// Permits currently in flight.
    pub active_permits: usize,
    /// Committed byte reservations.
    pub committed_bytes: u64,
    /// Current scheduler mode.
    pub scheduler_mode: SchedulerMode,
    /// Current gate-global throttled state (not per-request admission).
    pub throttled: bool,
    /// Last request-specific wait reason, if any waiter is blocked.
    /// `None` whenever no request is parked: the reason describes live
    /// waiters, never a stale outcome cleared by another admission.
    pub wait_reason: Option<WaitReason>,
    /// Requests currently parked in a wait loop.
    pub waiters: usize,
    /// Parked requests split by wait reason. Totals equal `waiters`.
    pub waiters_by_reason: WaitersByReason,
    /// The last probe skipped the page-cache check because cache
    /// accounting was unavailable. The observation stayed usable; the
    /// cache policy simply did not run.
    pub cache_unknown: bool,
    /// Times the gate entered throttled state.
    pub throttle_entries: u64,
    /// Times pressure cleared with an actual admission.
    pub resume_count: u64,
    /// Provider probe failures observed.
    pub provider_failures: u64,
    /// Admission timeouts observed.
    pub timeouts: u64,
    /// Accumulated waiter milliseconds (when tracked).
    pub total_wait_ms: u64,
    /// Bounded-buffer drops (when buffering is used).
    pub dropped_events: u64,
}

/// Bounded counters for transitions/errors. Assigned under the lock.
#[derive(Debug, Default)]
pub struct Counters {
    /// Times the gate entered throttled state.
    pub throttle_entries: u64,
    /// Times pressure cleared with an actual admission.
    pub resume_count: u64,
    /// Provider probe failures observed.
    pub provider_failures: u64,
    /// Admission timeouts observed.
    pub timeouts: u64,
    /// Accumulated waiter milliseconds (when tracked).
    pub total_wait_ms: u64,
    /// Bounded-buffer drops (when buffering is used).
    pub dropped_events: u64,
    /// Diagnostic sequence number.
    pub sequence: u64,
}

impl Counters {
    /// Next diagnostic sequence number (assigned under the lock).
    pub fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    /// Record a throttle entry.
    pub fn record_throttle_entered(&mut self) {
        self.throttle_entries = self.throttle_entries.saturating_add(1);
    }

    /// Record a pressure-cleared admission.
    pub fn record_resumed(&mut self) {
        self.resume_count = self.resume_count.saturating_add(1);
    }

    /// Record a provider failure.
    pub fn record_provider_failure(&mut self) {
        self.provider_failures = self.provider_failures.saturating_add(1);
    }

    /// Record an admission timeout.
    pub fn record_timeout(&mut self) {
        self.timeouts = self.timeouts.saturating_add(1);
    }

    /// Record a bounded-buffer drop.
    pub fn record_dropped(&mut self) {
        self.dropped_events = self.dropped_events.saturating_add(1);
    }
}

/// Sanitize a provider failure into a stable reason code. Never returns the
/// inner `Source` text, which may contain paths, PIDs, or custom provider
/// messages from downstream integrators.
#[must_use]
pub fn sanitize_provider_error(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::Unsupported => "provider-unsupported",
        ProviderError::Source(_) => "provider-source-unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_source_text_never_leaks_through_reason_code() {
        let sensitive = ProviderError::new("SENSITIVE /proc pid=1234 token=abc");
        assert_eq!(
            sanitize_provider_error(&sensitive),
            "provider-source-unavailable"
        );
        assert!(!sanitize_provider_error(&sensitive).contains("SENSITIVE"));
        assert_eq!(
            sanitize_provider_error(&ProviderError::Unsupported),
            "provider-unsupported"
        );
    }

    #[test]
    fn wait_reasons_distinguish_global_from_request_specific() {
        assert_ne!(WaitReason::Pressure, WaitReason::Budget);
        assert_ne!(WaitReason::Budget, WaitReason::Exclusivity);
        assert_eq!(WaitReason::Pressure.as_str(), "pressure");
        assert_eq!(WaitReason::Budget.as_str(), "budget");
    }

    #[test]
    fn parked_counts_by_reason_converge_to_zero() {
        let mut parked = WaitersByReason::default();
        parked.inc(WaitReason::Budget);
        parked.inc(WaitReason::Budget);
        parked.inc(WaitReason::Pressure);
        assert_eq!(parked.total(), 3);
        assert_eq!(parked.budget, 2);
        parked.dec(WaitReason::Budget);
        // Non-parked reasons never move a bucket.
        parked.inc(WaitReason::CacheUnknown);
        parked.dec(WaitReason::ProviderDegraded);
        assert_eq!(parked.total(), 2);
        parked.dec(WaitReason::Budget);
        parked.dec(WaitReason::Pressure);
        assert_eq!(parked.total(), 0);
        // Saturating: no underflow.
        parked.dec(WaitReason::Budget);
        assert_eq!(parked.total(), 0);
    }
}
