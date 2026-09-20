//! Shared state machine for the unweighted sync/async admission gates.
//!
//! `sync::AdmissionGate` and `async::AdmissionGate` differ only in how they
//! wait (`Condvar` vs `Notify`); the throttle/hysteresis/fail-open decisions
//! are identical and live here so the two wrappers cannot drift.
//!
//! ponytail: the twins still mirror constructors/getters/`AdmissionPermit`
//! (~50 lines); a macro unifies them if a third twin ever appears.
//!
//! Locking discipline: [`decide`] assigns sequence/state under the caller's
//! admission lock and returns a deferred [`GateEvent`]. Callers must drop
//! the lock before invoking [`emit_gate_event`], so formatting and
//! subscriber work never run under admission mutexes.

use std::sync::Mutex;

use crate::config::Config;
use crate::provider::{ProviderError, SharedMemoryProvider};

/// What the memory scheduler is currently using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemorySource {
    Disabled,
    Provider,
    ThreadCapOnly,
}

impl MemorySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Provider => "provider",
            Self::ThreadCapOnly => "thread_cap_only",
        }
    }
}

/// Mutable admission bookkeeping, shared by both gates.
#[derive(Debug)]
pub(crate) struct GateState {
    pub(crate) active_tasks: usize,
    pub(crate) throttled: bool,
    pub(crate) throttle_logged: bool,
    pub(crate) memory_scheduler_active: bool,
    pub(crate) memory_source: MemorySource,
    pub(crate) provider_failure_logged: bool,
    /// Last wait reason (unweighted gates only block on pressure).
    /// Overwritten by wait outcomes only, never cleared by another
    /// request's admission; the snapshot reports it solely while requests
    /// are parked.
    pub(crate) last_wait: Option<crate::diagnostics::WaitReason>,
    /// Requests currently parked in a wait loop, split by reason (see
    /// [`WaiterGuard`]).
    pub(crate) parked: crate::diagnostics::WaitersByReason,
    pub(crate) timeouts: u64,
    pub(crate) total_wait_ms: u64,
    pub(crate) throttle_entries: u64,
    pub(crate) resume_count: u64,
    pub(crate) provider_failures: u64,
}

/// RAII parked-request registration shared by both gates. Counts one
/// parked wait cycle by reason and folds the parked duration into
/// `total_wait_ms` on drop, so async task cancellation converges both
/// instead of losing them. Uses `lock()` rather than `expect()` so a
/// poisoned mutex still converges the counts instead of panicking inside
/// a destructor.
pub(crate) struct WaiterGuard<'a> {
    state: &'a Mutex<GateState>,
    reason: crate::diagnostics::WaitReason,
    parked_at: std::time::Instant,
}

impl<'a> WaiterGuard<'a> {
    pub(crate) fn enter(
        state: &'a Mutex<GateState>,
        reason: crate::diagnostics::WaitReason,
    ) -> Self {
        if let Ok(mut guard) = state.lock() {
            guard.parked.inc(reason);
        }
        Self {
            state,
            reason,
            parked_at: std::time::Instant::now(),
        }
    }
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.state.lock() {
            guard.parked.dec(self.reason);
            guard.total_wait_ms = guard.total_wait_ms.saturating_add(
                self.parked_at
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            );
        }
    }
}

/// Deferred subscriber work. All fields are copied under the lock; the
/// caller emits after unlocking. No provider/source text is carried.
#[derive(Debug, Clone, Copy)]
pub(crate) enum GateEvent {
    ProviderFailedAtInit {
        fail_open: bool,
    },
    ThrottleEntered {
        usage: f64,
        max: f64,
        active_tasks: usize,
    },
    Resumed {
        usage: f64,
        resume: f64,
        active_tasks: usize,
    },
    Degraded,
}

/// Emit one deferred event. Must be called without holding admission locks.
pub(crate) fn emit_gate_event(event: GateEvent) {
    match event {
        GateEvent::ProviderFailedAtInit { fail_open } => {
            tracing::warn!(
                reason = "provider-source-unavailable",
                fail_open,
                event = crate::diagnostics::event::PROVIDER_FAILED,
                "memory provider failed at init"
            );
        }
        GateEvent::ThrottleEntered {
            usage,
            max,
            active_tasks,
        } => {
            if active_tasks > 0 {
                tracing::warn!(
                    event = crate::diagnostics::event::THROTTLE_ENTERED,
                    reason = crate::diagnostics::WaitReason::Pressure.as_str(),
                    current_ram_fraction = usage,
                    max_ram_fraction = max,
                    active_tasks,
                    "RAM usage above threshold while tasks are in flight; throttling new admissions"
                );
            } else {
                tracing::info!(
                    event = crate::diagnostics::event::THROTTLE_ENTERED,
                    reason = crate::diagnostics::WaitReason::Pressure.as_str(),
                    current_ram_fraction = usage,
                    max_ram_fraction = max,
                    "RAM usage above threshold; waiting before admitting more work"
                );
            }
        }
        GateEvent::Resumed {
            usage,
            resume,
            active_tasks,
        } => {
            tracing::info!(
                event = crate::diagnostics::event::RESUMED,
                current_ram_fraction = usage,
                resume_ram_fraction = resume,
                active_tasks,
                "RAM usage back below resume threshold; resuming admissions"
            );
        }
        GateEvent::Degraded => {
            tracing::warn!(
                event = crate::diagnostics::event::DEGRADED,
                reason = "provider-source-unavailable",
                "memory provider failed at runtime; falling back to thread-cap-only scheduling"
            );
        }
    }
}

impl GateState {
    /// Initial state plus the init-time provider probe from both `new()`.
    /// Returns a deferred init-failure event when the probe fails; the
    /// constructor emits it after construction (no lock is held there).
    pub(crate) fn new(
        config: &Config,
        provider: &SharedMemoryProvider,
    ) -> (Self, Option<GateEvent>) {
        let mut memory_source = if config.memory_scheduler_enabled {
            MemorySource::Provider
        } else {
            MemorySource::Disabled
        };
        let mut scheduler_active = config.memory_scheduler_enabled;
        let mut provider_failure_logged = false;
        let mut event = None;

        if scheduler_active && provider.used_fraction().is_err() {
            event = Some(GateEvent::ProviderFailedAtInit {
                fail_open: config.fail_open_on_provider_error,
            });
            if config.fail_open_on_provider_error {
                scheduler_active = false;
                memory_source = MemorySource::ThreadCapOnly;
            }
            provider_failure_logged = true;
        }

        (
            Self {
                active_tasks: 0,
                throttled: false,
                throttle_logged: false,
                memory_scheduler_active: scheduler_active,
                memory_source,
                provider_failure_logged,
                last_wait: None,
                parked: crate::diagnostics::WaitersByReason::default(),
                timeouts: 0,
                total_wait_ms: 0,
                throttle_entries: 0,
                resume_count: 0,
                provider_failures: 0,
            },
            event,
        )
    }

    /// Record one admission timeout. Parked durations are accounted by
    /// the RAII guard drops; only the count is recorded here. Only the
    /// sync gate has a timeout path; async-only builds omit it.
    #[cfg(feature = "sync")]
    pub(crate) fn record_timeout(&mut self) {
        self.timeouts = self.timeouts.saturating_add(1);
    }

    /// Consistent diagnostics snapshot. Mode derives from the recorded
    /// source (never inferred from permit counts), so a deliberately
    /// disabled gate reports `Disabled`.
    pub(crate) fn snapshot(&self) -> crate::diagnostics::DiagnosticsSnapshot {
        let scheduler_mode = match self.memory_source {
            MemorySource::Disabled => crate::diagnostics::SchedulerMode::Disabled,
            MemorySource::Provider => crate::diagnostics::SchedulerMode::Provider,
            MemorySource::ThreadCapOnly => crate::diagnostics::SchedulerMode::ThreadCapOnly,
        };
        crate::diagnostics::DiagnosticsSnapshot {
            active_permits: self.active_tasks,
            committed_bytes: 0,
            scheduler_mode,
            throttled: self.throttled,
            // The reason describes live parked waiters only: once nobody
            // is parked, a stale outcome must not linger in diagnostics.
            wait_reason: if self.parked.total() > 0 {
                self.last_wait
            } else {
                None
            },
            waiters: self.parked.total(),
            waiters_by_reason: self.parked,
            cache_unknown: false,
            throttle_entries: self.throttle_entries,
            resume_count: self.resume_count,
            provider_failures: self.provider_failures,
            timeouts: self.timeouts,
            total_wait_ms: self.total_wait_ms,
            dropped_events: 0,
        }
    }

    /// Record a throttle entry under the lock; returns the deferred emit, if
    /// this transition has not been logged yet.
    pub(crate) fn log_throttle(&mut self, usage: f64, max_ram_fraction: f64) -> Option<GateEvent> {
        if self.throttle_logged {
            return None;
        }
        self.throttle_logged = true;
        self.throttle_entries = self.throttle_entries.saturating_add(1);
        Some(GateEvent::ThrottleEntered {
            usage,
            max: max_ram_fraction,
            active_tasks: self.active_tasks,
        })
    }

    /// Record fail-open degradation under the lock; returns the deferred
    /// emit only once. The raw error is sanitized to a reason code at emit
    /// time (never carried).
    pub(crate) fn disable_memory_scheduler(&mut self, _error: &ProviderError) -> Option<GateEvent> {
        self.provider_failures = self.provider_failures.saturating_add(1);
        let event = if !self.provider_failure_logged {
            self.provider_failure_logged = true;
            Some(GateEvent::Degraded)
        } else {
            None
        };
        self.memory_scheduler_active = false;
        self.throttled = false;
        self.throttle_logged = false;
        self.memory_source = MemorySource::ThreadCapOnly;
        event
    }
}

/// One probe decision shared by both `acquire` loops.
pub(crate) enum Decision {
    Admitted,
    Wait,
    ProviderFailed(ProviderError),
}

/// Pure state transition under the caller's lock. Returns the decision plus
/// at most one deferred event for the caller to emit after unlocking.
pub(crate) fn decide(
    state: &mut GateState,
    usage: Result<f64, ProviderError>,
    config: &Config,
) -> (Decision, Option<GateEvent>) {
    // `last_wait` is only overwritten by wait outcomes: another
    // request's admission must not clear the reason a still-parked request
    // is blocked on. The snapshot reports it only while requests are
    // actually parked.
    if !state.memory_scheduler_active {
        state.active_tasks += 1;
        return (Decision::Admitted, None);
    }
    match usage {
        Ok(usage) => {
            if state.throttled {
                if usage < config.resume_ram_fraction() {
                    state.throttled = false;
                    state.throttle_logged = false;
                    state.resume_count = state.resume_count.saturating_add(1);
                    state.active_tasks += 1;
                    (
                        Decision::Admitted,
                        Some(GateEvent::Resumed {
                            usage,
                            resume: config.resume_ram_fraction(),
                            active_tasks: state.active_tasks,
                        }),
                    )
                } else {
                    let max = config.max_ram_fraction;
                    let event = state.log_throttle(usage, max);
                    state.last_wait = Some(crate::diagnostics::WaitReason::Pressure);
                    (Decision::Wait, event)
                }
            } else if usage > config.max_ram_fraction {
                state.throttled = true;
                state.throttle_logged = false;
                let max = config.max_ram_fraction;
                let event = state.log_throttle(usage, max);
                state.last_wait = Some(crate::diagnostics::WaitReason::Pressure);
                (Decision::Wait, event)
            } else {
                state.active_tasks += 1;
                (Decision::Admitted, None)
            }
        }
        Err(e) => {
            if config.fail_open_on_provider_error {
                let event = state.disable_memory_scheduler(&e);
                state.active_tasks += 1;
                (Decision::Admitted, event)
            } else {
                (Decision::ProviderFailed(e), None)
            }
        }
    }
}
