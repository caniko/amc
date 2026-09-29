//! Weighted (byte-budgeted) admission gates.
//!
//! Plain `sync::AdmissionGate` / `async::AdmissionGate`
//! treat every task as equal: as long as host RAM usage is below the threshold
//! they admit unboundedly many. That works when tasks have similar memory
//! cost. It breaks when a single pipeline mixes 5 KB metadata writes with
//! 10 GB archive extractions — the gate happily admits 64 large workers, the
//! kernel reports them as cache pressure on the next probe, by which point
//! several have already been spawned (and `7zz` subprocesses outside the
//! Tokio scheduler are immune to the gate's later throttling).
//!
//! The weighted admission gates solve this by:
//!
//! 1. Capturing the live `MemAvailable` at every probe.
//! 2. Tracking the *committed weight* — the sum of byte costs declared by
//!    permits currently in flight.
//! 3. Admitting only when `available_bytes` minus a configurable safety
//!    reserve, minus committed weight, is at least the new task's weight.
//!
//! The result is a hard byte budget that scales with the host instead of a
//! fixed worker count, and that accounts for subprocesses (since their
//! growing allocations show up as falling `MemAvailable` while their parent
//! still holds the permit).
//!
//! This module mirrors the API of `sync` / `async`: sync
//! and async admission gates plus a [`WeightedPermit`] that reserves its byte
//! count for the lifetime of the permit.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(feature = "async")]
use tokio::sync::Notify;

#[cfg(feature = "sync")]
use std::sync::Condvar;

#[cfg(any(feature = "sync", feature = "async"))]
use crate::AdmitError;
use crate::config::{Config, ConfigError};
#[cfg(any(feature = "sync", feature = "async"))]
use crate::provider::ProviderError;
use crate::provider::{MemoryStats, SharedMemoryProvider};

/// Configuration for a weighted admission gate.
#[derive(Debug, Clone)]
pub struct WeightedConfig {
    /// Inherited base config (max_ram_fraction, hysteresis, poll_interval).
    pub base: Config,

    /// Bytes deliberately left unallocated as a safety margin. The gate will
    /// not commit weight that drives `available_bytes` below this floor.
    /// Defaults to 1 GiB.
    pub safety_reserve_bytes: u64,

    /// Jobs heavier than this run exclusively. They still must fit the
    /// byte and fraction ceilings; oversized is not a bypass.
    /// Defaults to 8 GiB.
    pub max_single_weight_bytes: u64,

    /// Throttle when `Buffers + Cached` exceeds this fraction of total RAM,
    /// even if `MemAvailable` looks fine. Heavy I/O workloads (archive
    /// extractions, large file copies) build up readahead folios faster than
    /// the kernel reclaim path can drop them, leading to swap thrash even
    /// though `MemAvailable` reports headroom.
    ///
    /// Set to `1.0` to disable. Defaults to `0.6`.
    pub max_page_cache_fraction: f64,

    /// When `true`, unknown page-cache accounting is a policy failure
    /// instead of a skipped check: with the cache gate enabled
    /// (`max_page_cache_fraction < 1.0`) and no cache bytes reported, the
    /// attempt fails as a provider error (honoring the configured
    /// fail-open/fail-closed behavior). Defaults to `false`, which skips
    /// the cache check and records `cache_unknown` in diagnostics.
    pub require_page_cache_bytes: bool,

    /// How long a stats probe is considered fresh. Stats are re-read more
    /// often than this only when waiting for admission. Defaults to 100 ms.
    pub stats_max_age: Duration,
}

impl Default for WeightedConfig {
    fn default() -> Self {
        Self {
            base: Config::default(),
            safety_reserve_bytes: 1 << 30,
            max_single_weight_bytes: 8 * (1 << 30),
            max_page_cache_fraction: 0.6,
            require_page_cache_bytes: false,
            stats_max_age: Duration::from_millis(100),
        }
    }
}

impl WeightedConfig {
    /// Return the validated default configuration.
    #[must_use]
    pub fn validated_default() -> Self {
        Self::default()
            .validate()
            .expect("default weighted amc-runner config must be valid")
    }

    /// Validates the configuration. Returns `Ok(self)` if valid.
    ///
    /// # Errors
    /// Returns [`WeightedConfigError`] when any field is out of range.
    pub fn validate(self) -> Result<Self, WeightedConfigError> {
        self.base
            .clone()
            .validate()
            .map_err(WeightedConfigError::Base)?;

        if !(self.max_page_cache_fraction.is_finite()
            && self.max_page_cache_fraction > 0.0
            && self.max_page_cache_fraction <= 1.0)
        {
            return Err(WeightedConfigError::MaxPageCacheFractionOutOfRange(
                self.max_page_cache_fraction,
            ));
        }

        if self.stats_max_age.is_zero() {
            return Err(WeightedConfigError::StatsMaxAgeZero);
        }

        Ok(self)
    }
}

/// Errors produced when validating a [`WeightedConfig`].
#[derive(Debug, Clone, PartialEq)]
pub enum WeightedConfigError {
    /// Embedded base config is invalid.
    Base(ConfigError),
    /// `max_page_cache_fraction` must be finite and in `(0.0, 1.0]`.
    MaxPageCacheFractionOutOfRange(f64),
    /// `stats_max_age` must be non-zero.
    StatsMaxAgeZero,
}

impl std::fmt::Display for WeightedConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Base(e) => write!(f, "base config invalid: {e}"),
            Self::MaxPageCacheFractionOutOfRange(v) => write!(
                f,
                "max_page_cache_fraction must be finite and in (0.0, 1.0], got {v}"
            ),
            Self::StatsMaxAgeZero => f.write_str("stats_max_age must be > 0"),
        }
    }
}

impl std::error::Error for WeightedConfigError {}

#[cfg_attr(not(any(feature = "sync", feature = "async")), allow(dead_code))]
#[derive(Debug)]
struct GateState {
    committed_bytes: u64,
    active_permits: usize,
    oversized_in_flight: bool,
    scheduler_active: bool,
    /// Whether scheduling was disabled at construction (as opposed to a
    /// runtime fail-open degradation). Recorded once so diagnostics never
    /// report a deliberately disabled gate as provider-driven.
    scheduler_disabled: bool,
    failure_logged: bool,
    /// Edge-triggered: true between entering and leaving the throttled state.
    /// Each transition logs once, avoiding the per-poll log flood that
    /// otherwise drowns the application's own progress output.
    throttled: bool,
    /// Last request-specific wait reason (not logged per poll; exposed via
    /// diagnostics snapshot so repeated waits stay explainable). Only
    /// overwritten by wait outcomes, never cleared by another request's
    /// admission; the snapshot reports it only while requests are parked.
    last_wait: Option<crate::diagnostics::WaitReason>,
    /// Requests currently parked in a wait loop, split by reason.
    /// Incremented when a request parks and decremented by an RAII guard,
    /// so async task cancellation cannot leak the count.
    parked: crate::diagnostics::WaitersByReason,
    /// Whether the last probe skipped the page-cache check for lack of
    /// accounting. Explicit degraded coverage, not a silent pass.
    cache_unknown: bool,
    throttle_entries: u64,
    resume_count: u64,
    provider_failures: u64,
    timeouts: u64,
    total_wait_ms: u64,
}

/// Stats probe cache, locked independently of admission state so provider
/// I/O never runs under the admission mutex.
///
/// `epoch` orders publication: a release invalidates the cache by bumping
/// it, and a probe publishes only when the epoch is unchanged and no
/// newer observation landed during its I/O. Otherwise an older, slower
/// probe (or one started before a release) could overwrite fresher state
/// and admit work against stale headroom.
#[cfg_attr(not(any(feature = "sync", feature = "async")), allow(dead_code))]
#[derive(Debug, Default)]
struct StatsCache {
    stats: Option<MemoryStats>,
    at: Option<Instant>,
    epoch: u64,
}

#[cfg_attr(not(any(feature = "sync", feature = "async")), allow(dead_code))]
struct Inner {
    state: Mutex<GateState>,
    stats_cache: Mutex<StatsCache>,
    #[cfg(feature = "sync")]
    condvar: Condvar,
    #[cfg(feature = "async")]
    notify: Notify,
    provider: SharedMemoryProvider,
    config: WeightedConfig,
}

/// Decision returned by the internal admit-attempt step.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Debug)]
#[allow(dead_code)]
enum AttemptOutcome {
    /// Permit granted; caller increments accounting and returns.
    Admitted,
    /// Wait for a release / poll interval and retry, with the specific
    /// reason this request is blocked (global pressure vs request budget).
    Wait {
        reason: crate::diagnostics::WaitReason,
    },
    /// Provider failed; gate has fallen back to fraction-only and admitted.
    AdmittedFallback,
    /// Weight cannot fit even alone.
    Impossible { weight: u64, budget: u64 },
    /// Provider failed and fail-open is disabled.
    Provider(ProviderError),
}

impl Inner {
    #[cfg(any(feature = "sync", feature = "async"))]
    fn new(config: WeightedConfig, provider: SharedMemoryProvider) -> Self {
        let scheduler_active = config.base.memory_scheduler_enabled;
        Self {
            state: Mutex::new(GateState {
                committed_bytes: 0,
                active_permits: 0,
                oversized_in_flight: false,
                scheduler_active,
                scheduler_disabled: !config.base.memory_scheduler_enabled,
                failure_logged: false,
                throttled: false,
                last_wait: None,
                parked: crate::diagnostics::WaitersByReason::default(),
                cache_unknown: false,
                throttle_entries: 0,
                resume_count: 0,
                provider_failures: 0,
                timeouts: 0,
                total_wait_ms: 0,
            }),
            stats_cache: Mutex::new(StatsCache::default()),
            #[cfg(feature = "sync")]
            condvar: Condvar::new(),
            #[cfg(feature = "async")]
            notify: Notify::new(),
            provider,
            config,
        }
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    fn committed_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("weighted gate poisoned")
            .committed_bytes
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    fn active_permits(&self) -> usize {
        self.state
            .lock()
            .expect("weighted gate poisoned")
            .active_permits
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    fn scheduler_active(&self) -> bool {
        self.state
            .lock()
            .expect("weighted gate poisoned")
            .scheduler_active
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    fn diagnostics_snapshot(&self) -> crate::diagnostics::DiagnosticsSnapshot {
        let state = self.state.lock().expect("weighted gate poisoned");
        // Construction-time intent is recorded explicitly: a deliberately
        // disabled gate is Disabled even before any permit flows, and only
        // a runtime fail-open degradation reports ThreadCapOnly.
        let mode = if state.scheduler_disabled {
            crate::diagnostics::SchedulerMode::Disabled
        } else if state.scheduler_active {
            crate::diagnostics::SchedulerMode::Provider
        } else {
            crate::diagnostics::SchedulerMode::ThreadCapOnly
        };
        crate::diagnostics::DiagnosticsSnapshot {
            active_permits: state.active_permits,
            committed_bytes: state.committed_bytes,
            scheduler_mode: mode,
            throttled: state.throttled,
            // The reason describes live waiters only: once nobody is
            // parked, a stale outcome from an earlier generation must not
            // linger in diagnostics.
            wait_reason: if state.parked.total() > 0 {
                state.last_wait
            } else {
                None
            },
            waiters: state.parked.total(),
            waiters_by_reason: state.parked,
            cache_unknown: state.cache_unknown,
            throttle_entries: state.throttle_entries,
            resume_count: state.resume_count,
            provider_failures: state.provider_failures,
            timeouts: state.timeouts,
            total_wait_ms: state.total_wait_ms,
            dropped_events: 0,
        }
    }

    /// Record one admission timeout. Parked durations are accounted by
    /// the RAII guard drops; only the count is recorded here. Only the
    /// sync gate has a timeout path; async-only builds omit it.
    #[cfg(feature = "sync")]
    fn record_timeout(&self) {
        let mut state = self.state.lock().expect("weighted gate poisoned");
        state.timeouts = state.timeouts.saturating_add(1);
    }

    /// RAII parked-request registration for one wait cycle. The guard
    /// increments the per-reason parked count on entry; on drop it
    /// decrements the count and folds the parked duration into
    /// `total_wait_ms`, so async task cancellation converges both the
    /// count and the duration instead of losing them.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn parked_guard(self: &Arc<Self>, reason: crate::diagnostics::WaitReason) -> ParkedGuard {
        {
            let mut state = self.state.lock().expect("weighted gate poisoned");
            state.parked.inc(reason);
        }
        ParkedGuard {
            inner: Arc::clone(self),
            reason,
            parked_at: Instant::now(),
        }
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    fn permit_result(
        self: &Arc<Self>,
        outcome: AttemptOutcome,
        weight_bytes: u64,
    ) -> Result<WeightedPermit, AdmitError> {
        match outcome {
            AttemptOutcome::Admitted => {
                let oversized = weight_bytes > self.config.max_single_weight_bytes;
                Ok(WeightedPermit::new(self.clone(), weight_bytes, oversized))
            }
            AttemptOutcome::AdmittedFallback => {
                Ok(WeightedPermit::new(self.clone(), weight_bytes, false))
            }
            AttemptOutcome::Impossible { weight, budget } => {
                Err(AdmitError::Impossible { weight, budget })
            }
            AttemptOutcome::Provider(error) => Err(AdmitError::Provider(error)),
            AttemptOutcome::Wait { .. } => unreachable!("caller handles Wait"),
        }
    }

    /// One admission attempt. Returns the outcome plus deferred subscriber
    /// work; the caller constructs the RAII permit *before* emitting, so a
    /// panicking subscriber unwinds through a live permit whose destructor
    /// releases the reservation instead of leaking it.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn try_admit(&self, weight: u64) -> (AttemptOutcome, Vec<WeightedDeferred>) {
        // Fast path: an inactive scheduler never probes. This keeps a
        // deliberately disabled gate (and an already-degraded one) off
        // provider I/O entirely, including repeated failure events. The
        // locked recheck below covers the race with a concurrent
        // degradation.
        {
            let mut state = self.state.lock().expect("weighted gate poisoned");
            if !state.scheduler_active {
                state.committed_bytes = state.committed_bytes.saturating_add(weight);
                state.active_permits += 1;
                return (AttemptOutcome::AdmittedFallback, Vec::new());
            }
        }
        // Probe first without holding the admission lock, then decide under
        // it; the caller emits after the lock is dropped.
        let (stats_result, mut deferred, generation) = self.probe_stats();
        let outcome = {
            let mut state = self.state.lock().expect("weighted gate poisoned");
            // Same lock order as release. Keep publication/invalidation out
            // until this observation's admission decision is committed.
            let cache = self
                .stats_cache
                .lock()
                .expect("weighted stats cache poisoned");
            if state.scheduler_active && cache.epoch != generation {
                return (
                    AttemptOutcome::Wait {
                        reason: crate::diagnostics::WaitReason::Budget,
                    },
                    Vec::new(),
                );
            }
            self.try_admit_locked(&mut state, weight, stats_result, &mut deferred)
        };
        (outcome, deferred)
    }

    /// Refresh stats if stale. Only the probe cache lock is held during
    /// provider I/O; the admission lock is never held across a probe.
    /// Returns the stats (or failure) plus a deferred emit for a newly
    /// observed failure.
    ///
    /// Publication is epoch-ordered: the probe records the cache epoch
    /// before its I/O and publishes only when the epoch is unchanged and
    /// no newer observation landed meanwhile. The caller always decides
    /// on its own freshly probed stats, never on a concurrent thread's
    /// newer-or-older publication.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn probe_stats(
        &self,
    ) -> (
        Result<MemoryStats, ProviderError>,
        Vec<WeightedDeferred>,
        u64,
    ) {
        let mut deferred = Vec::new();
        let epoch = {
            let cache = self
                .stats_cache
                .lock()
                .expect("weighted stats cache poisoned");
            if let (Some(cached), Some(when)) = (&cache.stats, cache.at)
                && when.elapsed() < self.config.stats_max_age
            {
                return (Ok(cached.clone()), deferred, cache.epoch);
            }
            cache.epoch
        };
        let started = Instant::now();
        match self.provider.stats() {
            Ok(stats) => {
                let mut cache = self
                    .stats_cache
                    .lock()
                    .expect("weighted stats cache poisoned");
                // Publish only when nothing invalidated or superseded us:
                // a release bumps the epoch, a faster probe sets a newer
                // timestamp. Our own return value is unaffected either way.
                let superseded = cache.epoch != epoch || matches!(cache.at, Some(t) if t > started);
                if !superseded {
                    cache.stats = Some(stats.clone());
                    cache.at = Some(started);
                    cache.epoch = cache.epoch.wrapping_add(1);
                    return (Ok(stats), deferred, cache.epoch);
                }
                // Return the original generation: the admission-side check
                // rejects it, not merely its publication into the cache.
                (Ok(stats), deferred, epoch)
            }
            Err(error) => {
                // Failure accounting (counts, degradation, emit) happens in
                // `try_admit_locked` under the admission lock; the raw error
                // is carried, never formatted here.
                deferred.push(WeightedDeferred::ProviderFailed);
                (Err(error), deferred, epoch)
            }
        }
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    fn try_admit_locked(
        &self,
        state: &mut GateState,
        weight: u64,
        stats_result: Result<MemoryStats, ProviderError>,
        deferred: &mut Vec<WeightedDeferred>,
    ) -> AttemptOutcome {
        if !state.scheduler_active {
            state.committed_bytes = state.committed_bytes.saturating_add(weight);
            state.active_permits += 1;
            return AttemptOutcome::AdmittedFallback;
        }

        let stats = match stats_result {
            Ok(s) => s,
            Err(error) => {
                return self.provider_failed(state, deferred, weight, error);
            }
        };
        // Required-cache policy runs on the observation before any check:
        // unknown accounting with a required measurement is an explicit
        // policy failure with the same fail-open/closed handling as a
        // failed probe.
        if self.config.require_page_cache_bytes
            && self.config.max_page_cache_fraction < 1.0
            && (stats.page_cache_opt().is_none()
                || (stats.page_cache_total().is_none() && stats.domains().len() != 1))
        {
            state.cache_unknown = true;
            return self.provider_failed(
                state,
                deferred,
                weight,
                ProviderError::new("page-cache accounting unavailable but required"),
            );
        }

        let safety = self.config.safety_reserve_bytes;
        let max = self.config.base.max_ram_fraction;
        let hard_cap = domain_hard_cap(&stats, safety, max);
        if weight > hard_cap {
            return AttemptOutcome::Impossible {
                weight,
                budget: hard_cap,
            };
        }

        let current_pressure = projected_used_fraction(&stats, state.committed_bytes, 0);
        let projected_used = projected_used_fraction(&stats, state.committed_bytes, weight);
        let resume = self.config.base.resume_ram_fraction();
        // Whether the gate-global pressure condition cleared on this probe.
        // The resume event is only emitted when this request is actually
        // admitted below; a cleared pressure with a still-waiting request
        // must not log "resuming" and then return Wait.
        let mut pressure_cleared = false;
        if state.throttled {
            if current_pressure >= resume {
                record_throttle(
                    state,
                    deferred,
                    crate::diagnostics::WaitReason::Pressure,
                    current_pressure,
                    resume,
                );
                state.last_wait = Some(crate::diagnostics::WaitReason::Pressure);
                return AttemptOutcome::Wait {
                    reason: crate::diagnostics::WaitReason::Pressure,
                };
            }
            // Global pressure cleared; drop the edge silently and let the
            // request-specific checks below decide. Resume is logged on
            // admission, not here.
            state.throttled = false;
            pressure_cleared = true;
        } else if current_pressure > max {
            record_throttle(
                state,
                deferred,
                crate::diagnostics::WaitReason::Pressure,
                current_pressure,
                max,
            );
            state.last_wait = Some(crate::diagnostics::WaitReason::Pressure);
            return AttemptOutcome::Wait {
                reason: crate::diagnostics::WaitReason::Pressure,
            };
        }

        if projected_used > max {
            state.last_wait = Some(crate::diagnostics::WaitReason::Budget);
            return AttemptOutcome::Wait {
                reason: crate::diagnostics::WaitReason::Budget,
            };
        }

        // Page-cache pressure: even if `MemAvailable` looks healthy, a
        // ballooning `Buffers+Cached` is the canary for an I/O-induced
        // readahead/swap-thrash death spiral. Enabled by default
        // (`max_page_cache_fraction == 0.6`); set to `1.0` to disable when
        // a cgroup `MemoryHigh` provides kernel-enforced containment
        // without our help.
        //
        // The fraction divides by the cache's own domain total (the leaf
        // limit, or the host total for host accounting) — never by a
        // minimum aggregated across independently limited domains. Unknown
        // cache accounting skips this check without invalidating the
        // observation, and records the skip in diagnostics.
        state.cache_unknown = false;
        if self.config.max_page_cache_fraction < 1.0 {
            match stats.page_cache_opt() {
                None => {
                    // Optional measurement unavailable: skip the cache check,
                    // keep the observation usable, and record the skip.
                    state.cache_unknown = true;
                }
                Some(cache_bytes) => {
                    let denominator = stats.page_cache_total().or_else(|| {
                        (stats.domains().len() == 1).then(|| stats.domains()[0].total_bytes())
                    });
                    state.cache_unknown = denominator.is_none();
                    if let Some(denominator) = denominator.filter(|value| *value > 0) {
                        let cache_fraction = cache_bytes as f64 / denominator as f64;
                        if cache_fraction >= self.config.max_page_cache_fraction {
                            record_throttle(
                                state,
                                deferred,
                                crate::diagnostics::WaitReason::PageCache,
                                cache_fraction,
                                self.config.max_page_cache_fraction,
                            );
                            state.last_wait = Some(crate::diagnostics::WaitReason::PageCache);
                            return AttemptOutcome::Wait {
                                reason: crate::diagnostics::WaitReason::PageCache,
                            };
                        }
                    }
                }
            }
        }

        let oversized = weight > self.config.max_single_weight_bytes;
        if oversized && state.active_permits > 0 {
            state.last_wait = Some(crate::diagnostics::WaitReason::Exclusivity);
            return AttemptOutcome::Wait {
                reason: crate::diagnostics::WaitReason::Exclusivity,
            };
        }
        if state.oversized_in_flight {
            state.last_wait = Some(crate::diagnostics::WaitReason::Exclusivity);
            return AttemptOutcome::Wait {
                reason: crate::diagnostics::WaitReason::Exclusivity,
            };
        }

        let budget = stats
            .available_bytes()
            .saturating_sub(safety)
            .saturating_sub(state.committed_bytes);

        if budget >= weight {
            state.committed_bytes = state.committed_bytes.saturating_add(weight);
            state.active_permits += 1;
            if oversized {
                state.oversized_in_flight = true;
            }
            if pressure_cleared {
                state.resume_count = state.resume_count.saturating_add(1);
                deferred.push(WeightedDeferred::Resumed {
                    projected: projected_used,
                    committed: state.committed_bytes,
                });
            }
            // `last_wait` is only overwritten by wait outcomes: another
            // request's admission must not clear the reason a still-parked
            // request is blocked on. The snapshot reports it only while
            // requests are actually parked.
            AttemptOutcome::Admitted
        } else {
            state.last_wait = Some(crate::diagnostics::WaitReason::Budget);
            AttemptOutcome::Wait {
                reason: crate::diagnostics::WaitReason::Budget,
            }
        }
    }

    /// Shared failure handling for failed probes and required-but-missing
    /// measurements. Counts every failure, emits once, and degrades iff
    /// fail-open is configured; otherwise the attempt fails closed.
    #[cfg(any(feature = "sync", feature = "async"))]
    fn provider_failed(
        &self,
        state: &mut GateState,
        deferred: &mut Vec<WeightedDeferred>,
        weight: u64,
        error: ProviderError,
    ) -> AttemptOutcome {
        // Normalize to a single emit: a persistently failing provider logs
        // once, not per poll, and a synthesized policy failure emits like
        // a first probe failure.
        deferred.retain(|event| !matches!(event, WeightedDeferred::ProviderFailed));
        if !state.failure_logged {
            state.failure_logged = true;
            deferred.push(WeightedDeferred::ProviderFailed);
        }
        state.provider_failures = state.provider_failures.saturating_add(1);
        if self.config.base.fail_open_on_provider_error {
            state.scheduler_active = false;
            state.committed_bytes = state.committed_bytes.saturating_add(weight);
            state.active_permits += 1;
            AttemptOutcome::AdmittedFallback
        } else {
            AttemptOutcome::Provider(error)
        }
    }

    fn release(&self, weight: u64, was_oversized: bool) {
        let mut state = self.state.lock().expect("weighted gate poisoned");
        state.committed_bytes = state.committed_bytes.saturating_sub(weight);
        if state.active_permits > 0 {
            state.active_permits -= 1;
        }
        if was_oversized {
            state.oversized_in_flight = false;
        }
        // Invalidate cached stats so the next admit-attempt re-probes; freed
        // memory shows up in `MemAvailable` before the kernel updates async
        // counters, but `MemAvailable` reflects the freed budget within a
        // few milliseconds. The epoch bump orders publication: a probe
        // already in flight cannot resurrect pre-release observations.
        if let Ok(mut cache) = self.stats_cache.lock() {
            cache.at = None;
            cache.epoch = cache.epoch.wrapping_add(1);
        }
        #[cfg(feature = "sync")]
        self.condvar.notify_all();
        #[cfg(feature = "async")]
        self.notify.notify_waiters();
    }
}

#[cfg(any(feature = "sync", feature = "async"))]
fn exceeds_fraction(used: u64, total: u64, max_fraction: f64) -> bool {
    if total == 0 {
        return true;
    }
    (used as f64) / (total as f64) > max_fraction
}

#[cfg(any(feature = "sync", feature = "async"))]
fn max_admissible_weight(total: u64, max_fraction: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let mut low = 0;
    let mut high = total;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if exceeds_fraction(mid, total, max_fraction) {
            high = mid - 1;
        } else {
            low = mid;
        }
    }
    low
}

#[cfg(any(feature = "sync", feature = "async"))]
fn domain_hard_cap(stats: &MemoryStats, safety: u64, max_fraction: f64) -> u64 {
    stats
        .domains()
        .iter()
        .map(|domain| {
            domain
                .total_bytes()
                .saturating_sub(safety)
                .min(max_admissible_weight(domain.total_bytes(), max_fraction))
        })
        .min()
        .unwrap_or(0)
}

#[cfg(any(feature = "sync", feature = "async"))]
fn projected_used_fraction(stats: &MemoryStats, committed: u64, weight: u64) -> f64 {
    // ponytail: committed is added on top of live used; MemAvailable may already
    // include in-flight RSS. Conservative until measured worker RSS exists.
    stats
        .domains()
        .iter()
        .map(|domain| {
            if domain.total_bytes() == 0 {
                return 1.0;
            }
            (domain
                .used_bytes()
                .saturating_add(committed)
                .saturating_add(weight)) as f64
                / domain.total_bytes() as f64
        })
        .fold(0.0, f64::max)
}

/// Deferred subscriber work for the weighted gate. Copied under the lock,
/// emitted after it is dropped so formatting and subscriber work never run
/// under admission mutexes.
#[cfg(any(feature = "sync", feature = "async"))]
#[derive(Debug, Clone, Copy)]
enum WeightedDeferred {
    Throttle {
        reason: crate::diagnostics::WaitReason,
        current: f64,
        threshold: f64,
        active: usize,
        committed: u64,
    },
    Resumed {
        projected: f64,
        committed: u64,
    },
    ProviderFailed,
}

#[cfg(any(feature = "sync", feature = "async"))]
fn emit_weighted_deferred(event: WeightedDeferred) {
    match event {
        WeightedDeferred::Throttle {
            reason,
            current,
            threshold,
            active,
            committed,
        } => {
            tracing::warn!(
                event = crate::diagnostics::event::THROTTLE_ENTERED,
                reason = reason.as_str(),
                current_fraction = current,
                threshold,
                active_permits = active,
                committed_bytes = committed,
                "weighted gate: throttling"
            );
        }
        WeightedDeferred::Resumed {
            projected,
            committed,
        } => {
            tracing::info!(
                event = crate::diagnostics::event::RESUMED,
                projected_used = projected,
                committed_bytes = committed,
                "weighted gate: resuming admissions"
            );
        }
        WeightedDeferred::ProviderFailed => {
            tracing::warn!(
                event = crate::diagnostics::event::PROVIDER_FAILED,
                reason = "provider-source-unavailable",
                "weighted memory provider failed"
            );
        }
    }
}

/// RAII parked-request registration for weighted gates. Decrements the
/// per-reason parked count on drop and accounts the parked duration, so
/// async task cancellation converges both instead of leaking them.
#[cfg(any(feature = "sync", feature = "async"))]
struct ParkedGuard {
    inner: Arc<Inner>,
    reason: crate::diagnostics::WaitReason,
    parked_at: Instant,
}

#[cfg(any(feature = "sync", feature = "async"))]
impl Drop for ParkedGuard {
    fn drop(&mut self) {
        // `lock()` (not `expect`) so a poisoned mutex still converges the
        // counts instead of panicking inside a destructor.
        if let Ok(mut state) = self.inner.state.lock() {
            state.parked.dec(self.reason);
            state.total_wait_ms = state.total_wait_ms.saturating_add(
                self.parked_at
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            );
        }
    }
}

/// Record a throttle entry under the lock and queue the deferred emit.
/// Callers emit after dropping the admission lock.
#[cfg(any(feature = "sync", feature = "async"))]
fn record_throttle(
    state: &mut GateState,
    deferred: &mut Vec<WeightedDeferred>,
    reason: crate::diagnostics::WaitReason,
    current: f64,
    threshold: f64,
) {
    if state.throttled {
        return;
    }
    state.throttled = true;
    state.throttle_entries = state.throttle_entries.saturating_add(1);
    deferred.push(WeightedDeferred::Throttle {
        reason,
        current,
        threshold,
        active: state.active_permits,
        committed: state.committed_bytes,
    });
}

/// Synchronous weighted admission gate.
#[cfg(feature = "sync")]
#[derive(Clone)]
pub struct SyncWeightedAdmissionGate {
    inner: Arc<Inner>,
}

#[cfg(feature = "sync")]
impl SyncWeightedAdmissionGate {
    /// Build a gate using [`WeightedConfig::validated_default`] and
    /// [`providers::default_provider`].
    ///
    /// [`providers::default_provider`]: crate::providers::default_provider
    #[must_use]
    pub fn new_default() -> Self {
        Self::new(
            WeightedConfig::validated_default(),
            crate::providers::default_provider(),
        )
    }

    /// Build a gate from a validated config and provider.
    #[must_use]
    pub fn new(config: WeightedConfig, provider: SharedMemoryProvider) -> Self {
        Self {
            inner: Arc::new(Inner::new(config, provider)),
        }
    }

    /// Block until `weight_bytes` of budget can be committed, then return a permit.
    pub fn acquire(&self, weight_bytes: u64) -> Result<WeightedPermit, AdmitError> {
        self.acquire_timeout(weight_bytes, Duration::MAX)
    }

    /// Like [`Self::acquire`], but fail with [`AdmitError::TimedOut`] after `timeout`.
    ///
    /// The first probe always runs. The timeout bounds subsequent waits, not
    /// provider I/O.
    pub fn acquire_timeout(
        &self,
        weight_bytes: u64,
        timeout: Duration,
    ) -> Result<WeightedPermit, AdmitError> {
        let deadline = Instant::now().checked_add(timeout);
        let mut attempted = false;
        loop {
            if attempted && deadline.is_some_and(|d| Instant::now() >= d) {
                self.inner.record_timeout();
                return Err(AdmitError::TimedOut);
            }
            attempted = true;
            let (outcome, deferred) = self.inner.try_admit(weight_bytes);
            match outcome {
                AttemptOutcome::Wait { reason } => {
                    // No reservation is held on this path, so edge events
                    // (e.g. throttle entry) emit before parking.
                    for event in deferred {
                        emit_weighted_deferred(event);
                    }
                    // Park for this wait cycle only: the guard counts the
                    // parked request by reason and folds the parked
                    // duration into diagnostics on drop, including
                    // cancellation. Probing iterations are not waiters.
                    let _parked = self.inner.parked_guard(reason);
                    let state = self.inner.state.lock().expect("weighted gate poisoned");
                    let wait = deadline.map_or(self.inner.config.base.poll_interval, |d| {
                        d.saturating_duration_since(Instant::now())
                            .min(self.inner.config.base.poll_interval)
                    });
                    if wait.is_zero() {
                        drop(state);
                        drop(_parked);
                        self.inner.record_timeout();
                        return Err(AdmitError::TimedOut);
                    }
                    let _ = self
                        .inner
                        .condvar
                        .wait_timeout(state, wait)
                        .expect("weighted gate poisoned");
                }
                outcome => {
                    // RAII ownership first: the permit exists before any
                    // subscriber code runs, so a panicking subscriber
                    // unwinds through a live permit whose destructor
                    // releases the reservation.
                    let result = self.inner.permit_result(outcome, weight_bytes);
                    for event in deferred {
                        emit_weighted_deferred(event);
                    }
                    return result;
                }
            }
        }
    }

    /// Currently committed weight.
    pub fn committed_bytes(&self) -> u64 {
        self.inner.committed_bytes()
    }

    /// Number of weighted permits currently in flight.
    pub fn active_permits(&self) -> usize {
        self.inner.active_permits()
    }

    /// Whether memory-aware scheduling is currently active.
    pub fn memory_scheduler_active(&self) -> bool {
        self.inner.scheduler_active()
    }

    /// Consistent diagnostics snapshot: permits, committed bytes, mode,
    /// throttled state, last wait reason, and transition/error counts.
    /// Sequence/state is assigned under the lock; no formatting happens
    /// under the lock.
    pub fn diagnostics(&self) -> crate::diagnostics::DiagnosticsSnapshot {
        self.inner.diagnostics_snapshot()
    }

    /// Last request-specific wait reason, if any waiter is blocked.
    /// `None` means no waiter is currently blocked or the last attempt
    /// admitted.
    pub fn wait_reason(&self) -> Option<crate::diagnostics::WaitReason> {
        self.diagnostics().wait_reason
    }
}

/// Asynchronous weighted admission gate.
#[cfg(feature = "async")]
#[derive(Clone)]
pub struct AsyncWeightedAdmissionGate {
    inner: Arc<Inner>,
}

#[cfg(feature = "async")]
impl AsyncWeightedAdmissionGate {
    /// Build a gate using [`WeightedConfig::validated_default`] and
    /// [`providers::default_provider`].
    ///
    /// [`providers::default_provider`]: crate::providers::default_provider
    #[must_use]
    pub fn new_default() -> Self {
        Self::new(
            WeightedConfig::validated_default(),
            crate::providers::default_provider(),
        )
    }

    /// Yield until `weight_bytes` of budget can be committed, then return a permit.
    pub async fn acquire(&self, weight_bytes: u64) -> Result<WeightedPermit, AdmitError> {
        loop {
            let (outcome, deferred) = self.inner.try_admit(weight_bytes);
            match outcome {
                AttemptOutcome::Wait { reason } => {
                    for event in deferred {
                        emit_weighted_deferred(event);
                    }
                    // Parked guard spans the sleep only: dropping this
                    // future (cancellation) converges both the per-reason
                    // count and the parked duration.
                    let _parked = self.inner.parked_guard(reason);
                    let notified = self.inner.notify.notified();
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep(self.inner.config.base.poll_interval) => {}
                    }
                }
                outcome => {
                    let result = self.inner.permit_result(outcome, weight_bytes);
                    for event in deferred {
                        emit_weighted_deferred(event);
                    }
                    return result;
                }
            }
        }
    }

    /// Build a gate from a validated config and provider.
    #[must_use]
    pub fn new(config: WeightedConfig, provider: SharedMemoryProvider) -> Self {
        Self {
            inner: Arc::new(Inner::new(config, provider)),
        }
    }

    /// Whether memory-aware scheduling is currently active.
    pub fn memory_scheduler_active(&self) -> bool {
        self.inner.scheduler_active()
    }

    /// Number of weighted permits currently in flight.
    pub fn active_permits(&self) -> usize {
        self.inner.active_permits()
    }

    /// Currently committed weight.
    pub fn committed_bytes(&self) -> u64 {
        self.inner.committed_bytes()
    }

    /// Consistent diagnostics snapshot (see sync gate).
    pub fn diagnostics(&self) -> crate::diagnostics::DiagnosticsSnapshot {
        self.inner.diagnostics_snapshot()
    }

    /// Last request-specific wait reason, if any.
    pub fn wait_reason(&self) -> Option<crate::diagnostics::WaitReason> {
        self.diagnostics().wait_reason
    }
}

/// RAII permit returned by both sync and async weighted gates.
pub struct WeightedPermit {
    inner: Option<Arc<Inner>>,
    weight: u64,
    oversized: bool,
}

impl WeightedPermit {
    #[cfg(any(feature = "sync", feature = "async"))]
    fn new(inner: Arc<Inner>, weight: u64, oversized: bool) -> Self {
        Self {
            inner: Some(inner),
            weight,
            oversized,
        }
    }

    /// The byte weight this permit committed.
    #[must_use]
    pub fn weight(&self) -> u64 {
        self.weight
    }
}

impl Drop for WeightedPermit {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.release(self.weight, self.oversized);
        }
    }
}

/// Ergonomic alias for the synchronous weighted gate.
#[cfg(feature = "sync")]
pub type SyncAdmissionGate = SyncWeightedAdmissionGate;

/// Ergonomic alias for the asynchronous weighted gate.
#[cfg(feature = "async")]
pub type AsyncAdmissionGate = AsyncWeightedAdmissionGate;

/// Ergonomic alias for the weighted permit type.
pub type Permit = WeightedPermit;

#[cfg(test)]
mod tests {
    #[cfg(feature = "sync")]
    #[test]
    fn required_cache_needs_a_source_matched_denominator() {
        let stats = MemoryStats::from_domains(
            vec![
                crate::MemoryDomain::new(1000, 900).unwrap(),
                crate::MemoryDomain::new(100, 90).unwrap(),
            ],
            Some(60),
        )
        .unwrap();
        let gate = SyncWeightedAdmissionGate::new(
            WeightedConfig {
                safety_reserve_bytes: 0,
                require_page_cache_bytes: true,
                ..WeightedConfig::default()
            },
            stats_of(stats),
        );
        assert!(matches!(
            gate.acquire_timeout(1, Duration::ZERO),
            Err(AdmitError::Provider(_))
        ));
        assert_eq!(gate.active_permits(), 0);
    }
    #[cfg(feature = "sync")]
    #[test]
    fn superseded_probe_cannot_admit() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        struct RacingProvider {
            calls: AtomicUsize,
            entered: Barrier,
            resume: Barrier,
        }
        impl crate::provider::MemoryProvider for RacingProvider {
            fn used_fraction(&self) -> Result<f64, ProviderError> {
                Ok(0.1)
            }
            fn stats(&self) -> Result<MemoryStats, ProviderError> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.entered.wait();
                    self.resume.wait();
                    MemoryStats::new(1000, 900, 0)
                } else {
                    MemoryStats::new(1000, 10, 0)
                }
            }
        }
        let provider = Arc::new(RacingProvider {
            calls: AtomicUsize::new(0),
            entered: Barrier::new(2),
            resume: Barrier::new(2),
        });
        let gate = SyncWeightedAdmissionGate::new(
            WeightedConfig {
                safety_reserve_bytes: 0,
                max_page_cache_fraction: 1.0,
                ..WeightedConfig::default()
            },
            provider.clone(),
        );
        let slow_gate = gate.clone();
        let slow = std::thread::spawn(move || slow_gate.inner.try_admit(100).0);
        provider.entered.wait();
        assert!(matches!(
            gate.inner.try_admit(100).0,
            AttemptOutcome::Wait { .. }
        ));
        provider.resume.wait();
        assert!(matches!(slow.join().unwrap(), AttemptOutcome::Wait { .. }));
        assert_eq!(gate.committed_bytes(), 0);
        assert_eq!(gate.active_permits(), 0);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn release_during_probe_invalidates_the_in_flight_observation() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        // The second provider call blocks so the test can release a live
        // permit mid-probe. The woken probe returns admittable stats from
        // a stale generation and must not admit on them.
        struct BlockingProvider {
            calls: AtomicUsize,
            entered: Barrier,
            resume: Barrier,
        }
        impl crate::provider::MemoryProvider for BlockingProvider {
            fn used_fraction(&self) -> Result<f64, ProviderError> {
                Ok(0.1)
            }
            fn stats(&self) -> Result<MemoryStats, ProviderError> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    self.entered.wait();
                    self.resume.wait();
                }
                MemoryStats::new(1000, 900, 0)
            }
        }
        let provider = Arc::new(BlockingProvider {
            calls: AtomicUsize::new(0),
            entered: Barrier::new(2),
            resume: Barrier::new(2),
        });
        let gate = SyncWeightedAdmissionGate::new(
            WeightedConfig {
                safety_reserve_bytes: 0,
                max_single_weight_bytes: u64::MAX,
                stats_max_age: Duration::from_millis(1),
                max_page_cache_fraction: 1.0,
                ..WeightedConfig::default()
            },
            provider.clone(),
        );
        // Prime the cache and hold a permit; the sleep expires the
        // millisecond probe cache so the next attempt really probes.
        let holder = gate.acquire_timeout(100, Duration::from_secs(5)).unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(10));
        let slow_gate = gate.clone();
        let slow = std::thread::spawn(move || slow_gate.inner.try_admit(100).0);
        provider.entered.wait();
        drop(holder);
        provider.resume.wait();
        assert!(matches!(slow.join().unwrap(), AttemptOutcome::Wait { .. }));
        assert_eq!(gate.committed_bytes(), 0);
        assert_eq!(gate.active_permits(), 0);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn release_forces_a_fresh_probe_before_the_next_decision() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counting {
            calls: Arc<AtomicUsize>,
            stats: MemoryStats,
        }
        impl crate::provider::MemoryProvider for Counting {
            fn used_fraction(&self) -> Result<f64, ProviderError> {
                Ok(self.stats.used_fraction())
            }
            fn stats(&self) -> Result<MemoryStats, ProviderError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.stats.clone())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: SharedMemoryProvider = Arc::new(Counting {
            calls: Arc::clone(&calls),
            stats: MemoryStats::new(1000, 900, 0).unwrap(),
        });
        let gate = SyncWeightedAdmissionGate::new(
            WeightedConfig {
                safety_reserve_bytes: 0,
                max_single_weight_bytes: u64::MAX,
                // A minute-long cache: without release invalidation the
                // second attempt would reuse the first observation.
                stats_max_age: Duration::from_secs(60),
                max_page_cache_fraction: 1.0,
                ..WeightedConfig::default()
            },
            provider,
        );
        let holder = gate.acquire_timeout(100, Duration::from_secs(5)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(holder);
        let (outcome, _) = gate.inner.try_admit(100);
        assert!(matches!(outcome, AttemptOutcome::Admitted));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "invalidated cache must be re-probed, not reused"
        );
    }

    #[cfg(feature = "async")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    #[cfg(any(feature = "sync", feature = "async"))]
    use crate::providers::FixedProvider;

    #[cfg(any(feature = "sync", feature = "async"))]
    #[derive(Debug)]
    struct StatsProvider {
        stats: MemoryStats,
        fail_stats: bool,
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    impl crate::provider::MemoryProvider for StatsProvider {
        fn used_fraction(&self) -> Result<f64, ProviderError> {
            Ok(self.stats.used_fraction())
        }

        fn stats(&self) -> Result<MemoryStats, ProviderError> {
            if self.fail_stats {
                Err(ProviderError::new("stats unavailable"))
            } else {
                Ok(self.stats.clone())
            }
        }
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    #[allow(dead_code)]
    fn stats_of(stats: MemoryStats) -> SharedMemoryProvider {
        Arc::new(StatsProvider {
            stats,
            fail_stats: false,
        })
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    #[allow(dead_code)]
    fn stats_provider(available_bytes: u64) -> SharedMemoryProvider {
        stats_of(MemoryStats::new(1024, available_bytes, 0).unwrap())
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    #[allow(dead_code)]
    fn failing_stats() -> SharedMemoryProvider {
        Arc::new(StatsProvider {
            stats: MemoryStats::new(1, 1, 0).unwrap(),
            fail_stats: true,
        })
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    #[allow(dead_code)]
    fn open_cfg() -> WeightedConfig {
        WeightedConfig {
            base: Config::default().validate().unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            ..WeightedConfig::default()
        }
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    #[allow(dead_code)]
    fn fast_cfg(base: Config) -> WeightedConfig {
        WeightedConfig {
            base: base.validate().unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::from_millis(1),
            ..WeightedConfig::default()
        }
    }

    #[cfg(any(feature = "sync", feature = "async"))]
    #[allow(dead_code)]
    fn open_cfg_with_max(max_ram_fraction: f64) -> WeightedConfig {
        WeightedConfig {
            base: Config {
                max_ram_fraction,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            ..WeightedConfig::default()
        }
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn admits_under_budget() {
        let gate = AsyncWeightedAdmissionGate::new(open_cfg(), FixedProvider::shared(0.10));
        let p = gate.acquire(1024).await.unwrap();
        assert_eq!(p.weight(), 1024);
        assert_eq!(gate.committed_bytes(), 1024);
        assert_eq!(gate.active_permits(), 1);
        assert!(gate.memory_scheduler_active());
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn release_decrements_committed_bytes() {
        let gate = AsyncWeightedAdmissionGate::new(open_cfg(), FixedProvider::shared(0.10));
        {
            let _p = gate.acquire(2048).await.unwrap();
            assert_eq!(gate.committed_bytes(), 2048);
        }
        assert_eq!(gate.committed_bytes(), 0);
        assert_eq!(gate.active_permits(), 0);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn sync_gate_admits_under_budget() {
        let gate = SyncWeightedAdmissionGate::new(open_cfg(), FixedProvider::shared(0.10));
        let p = gate.acquire(1024).unwrap();
        assert_eq!(p.weight(), 1024);
        assert_eq!(gate.active_permits(), 1);
    }

    #[test]
    fn validates_weighted_config() {
        assert!(WeightedConfig::default().validate().is_ok());

        let bad_cache = WeightedConfig {
            max_page_cache_fraction: f64::NAN,
            ..WeightedConfig::default()
        };
        assert!(matches!(
            bad_cache.validate(),
            Err(WeightedConfigError::MaxPageCacheFractionOutOfRange(_))
        ));

        let bad_age = WeightedConfig {
            stats_max_age: Duration::ZERO,
            ..WeightedConfig::default()
        };
        assert!(matches!(
            bad_age.validate(),
            Err(WeightedConfigError::StatsMaxAgeZero)
        ));

        let bad_base = WeightedConfig {
            base: Config {
                max_ram_fraction: 0.0,
                ..Config::default()
            },
            ..WeightedConfig::default()
        };
        assert!(matches!(
            bad_base.validate(),
            Err(WeightedConfigError::Base(_))
        ));
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_gate_blocks_until_budget_released() {
        let cfg = fast_cfg(Config {
            max_ram_fraction: 1.0,
            poll_interval: Duration::from_millis(10),
            ..Config::default()
        });
        let gate = AsyncWeightedAdmissionGate::new(cfg, stats_provider(700));
        let first = gate.acquire(600).await.unwrap();

        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move { waiter_gate.acquire(200).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished());

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("weighted waiter should resume")
            .expect("weighted waiter task should not panic");
        assert_eq!(second.weight(), 200);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn oversized_permit_runs_alone() {
        let cfg = WeightedConfig {
            base: Config {
                poll_interval: Duration::from_millis(10),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: 128,
            stats_max_age: Duration::from_millis(1),
            ..WeightedConfig::default()
        };
        let gate = AsyncWeightedAdmissionGate::new(cfg, stats_provider(1024));
        let small = gate.acquire(64).await.unwrap();

        let waiter_gate = gate.clone();
        let waiter_started = Arc::new(AtomicUsize::new(0));
        let waiter_started_task = Arc::clone(&waiter_started);
        let waiter = tokio::spawn(async move {
            waiter_started_task.store(1, Ordering::SeqCst);
            waiter_gate.acquire(256).await.unwrap()
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(waiter_started.load(Ordering::SeqCst), 1);
        assert!(!waiter.is_finished());

        drop(small);
        let oversized = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("oversized waiter should resume")
            .expect("oversized waiter task should not panic");
        assert_eq!(oversized.weight(), 256);
        assert_eq!(gate.active_permits(), 1);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn provider_failure_falls_back_and_admits() {
        let cfg = WeightedConfig {
            safety_reserve_bytes: 0,
            base: Config {
                fail_open_on_provider_error: true,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            ..WeightedConfig::default()
        };
        let gate = AsyncWeightedAdmissionGate::new(cfg, failing_stats());
        let permit = gate.acquire(2048).await.unwrap();
        assert_eq!(permit.weight(), 2048);
        assert!(!gate.memory_scheduler_active());
        assert_eq!(gate.committed_bytes(), 2048);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn provider_failure_is_closed_by_default() {
        let cfg = WeightedConfig {
            safety_reserve_bytes: 0,
            ..WeightedConfig::default()
        };
        let gate = AsyncWeightedAdmissionGate::new(cfg, failing_stats());
        assert!(matches!(
            gate.acquire(2048).await,
            Err(AdmitError::Provider(_))
        ));
        assert!(gate.memory_scheduler_active());
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn weight_beyond_total_is_impossible() {
        let cfg = WeightedConfig {
            safety_reserve_bytes: 0,
            max_single_weight_bytes: 64,
            ..WeightedConfig::default()
        };
        let gate = AsyncWeightedAdmissionGate::new(cfg, stats_provider(1024));
        assert!(matches!(
            gate.acquire(2048).await,
            Err(AdmitError::Impossible { weight: 2048, .. })
        ));
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn projected_usage_blocks_before_current_fraction() {
        let cfg = fast_cfg(Config {
            max_ram_fraction: 0.80,
            resume_hysteresis: 0.05,
            poll_interval: Duration::from_millis(10),
            ..Config::default()
        });
        // used=750/1000=0.75 < 0.80; weight 100 → projected 0.85 ≥ 0.80
        let gate = AsyncWeightedAdmissionGate::new(cfg, stats_provider(250));
        assert!(
            tokio::time::timeout(Duration::from_millis(40), gate.acquire(100))
                .await
                .is_err()
        );
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn committed_reservations_count_in_projection() {
        let cfg = fast_cfg(Config {
            max_ram_fraction: 0.80,
            poll_interval: Duration::from_millis(10),
            ..Config::default()
        });
        let gate = AsyncWeightedAdmissionGate::new(cfg, stats_provider(900));
        let _first = gate.acquire(400).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(40), gate.acquire(400))
                .await
                .is_err()
        );
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn inexact_fraction_ceiling_matches_projection() {
        let gate = AsyncWeightedAdmissionGate::new(
            open_cfg_with_max(0.29),
            stats_of(MemoryStats::new(100, 100, 0).unwrap()),
        );
        assert_eq!(gate.acquire(29).await.unwrap().weight(), 29);
        assert!(matches!(
            gate.acquire(30).await,
            Err(AdmitError::Impossible { weight: 30, .. })
        ));
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn fraction_ceiling_is_impossible() {
        let cfg = open_cfg_with_max(0.80);
        let gate = AsyncWeightedAdmissionGate::new(
            cfg,
            stats_of(MemoryStats::new(1000, 1000, 0).unwrap()),
        );
        let _permit = gate.acquire(800).await.unwrap();
        assert!(matches!(
            gate.acquire(801).await,
            Err(AdmitError::Impossible { weight: 801, .. })
        ));
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn projection_is_per_domain() {
        let cfg = open_cfg_with_max(0.80);
        let stats = MemoryStats::from_domains(
            vec![
                crate::MemoryDomain::new(10_000, 3_000).unwrap(),
                crate::MemoryDomain::new(100, 100).unwrap(),
            ],
            0,
        )
        .unwrap();
        let gate = AsyncWeightedAdmissionGate::new(cfg, stats_of(stats));
        assert_eq!(gate.acquire(20).await.unwrap().weight(), 20);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn hysteresis_does_not_trap_fitting_weight() {
        let provider = crate::test_support::AtomicAvailable::shared(1024, 100);
        let cfg = fast_cfg(Config {
            max_ram_fraction: 0.80,
            resume_hysteresis: 0.05,
            poll_interval: Duration::from_millis(10),
            ..Config::default()
        });
        let gate = AsyncWeightedAdmissionGate::new(cfg, provider.clone());
        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move { gate.acquire(780).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished());
        provider
            .available
            .store(1024, std::sync::atomic::Ordering::SeqCst);
        let permit = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("fitting weight should resume after pressure clears")
            .expect("task")
            .unwrap();
        assert_eq!(permit.weight(), 780);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn zero_timeout_probes_once_without_retry() {
        let provider = crate::test_support::FirstLowThenFull::shared();
        let cfg = WeightedConfig {
            base: Config {
                max_ram_fraction: 0.50,
                poll_interval: Duration::from_millis(50),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::ZERO,
            ..WeightedConfig::default()
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, provider.clone());
        assert!(matches!(
            gate.acquire_timeout(100, Duration::ZERO),
            Err(AdmitError::TimedOut)
        ));
        assert_eq!(provider.calls(), 1);
        // An expired deadline still permits the documented first probe.
        let permit = gate.acquire_timeout(100, Duration::ZERO).unwrap();
        assert_eq!(permit.weight(), 100);
        assert_eq!(provider.calls(), 2);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn sync_new_default_constructs() {
        let _gate = SyncWeightedAdmissionGate::new_default();
    }

    #[cfg(feature = "async")]
    #[test]
    fn async_new_default_constructs() {
        let _gate = AsyncWeightedAdmissionGate::new_default();
    }

    #[cfg(feature = "sync")]
    #[test]
    fn cleared_pressure_without_budget_is_wait_not_resume() {
        // Regression: try_admit logged "resuming admissions" when global
        // pressure cleared, even when the request still lacked byte budget
        // and the caller returned Wait. Resume must coincide with admission.
        use crate::diagnostics::WaitReason;
        let cfg = WeightedConfig {
            base: Config {
                max_ram_fraction: 0.80,
                resume_hysteresis: 0.05,
                poll_interval: Duration::from_millis(10),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::from_millis(1),
            max_page_cache_fraction: 1.0,
            require_page_cache_bytes: false,
        };
        // total=1000, available=900 → used=0.10; weight 750 → projected 0.85.
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_provider(900));
        // First admit 0-weight probe to enter throttled? Instead force the
        // path: commit 0, request weight that exceeds projection but fits
        // the byte budget, so outcome must be Budget-wait with no resume.
        let (outcome, _) = gate.inner.try_admit(750);
        match outcome {
            AttemptOutcome::Wait { reason } => assert_eq!(reason, WaitReason::Budget),
            other => panic!("expected Budget wait, got {other:?}"),
        }
        let diag = gate.diagnostics();
        // A bare probe parks nobody: the reason must not linger in
        // diagnostics once no request is actually waiting.
        assert_eq!(diag.wait_reason, None);
        assert_eq!(diag.waiters, 0);
        assert_eq!(diag.resume_count, 0);
        // Later checks contradicting an early resume must not have logged it:
        // resume_count stays 0 until an actual admission after pressure.
        assert!(!diag.throttled);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn unknown_cache_skips_cache_check_without_invalidating() {
        let cfg = WeightedConfig {
            base: Config::default().validate().unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            max_page_cache_fraction: 0.6,
            ..WeightedConfig::default()
        };
        let stats =
            MemoryStats::from_domains(vec![crate::MemoryDomain::new(1000, 900).unwrap()], None)
                .unwrap();
        assert!(!stats.page_cache_known());
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_of(stats));
        // Cache unknown + policy enabled: admission still succeeds, cache
        // check is skipped rather than failing closed or throttling on zero.
        let permit = gate.acquire_timeout(100, Duration::from_secs(1)).unwrap();
        assert_eq!(permit.weight(), 100);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn disabled_gate_reports_disabled_not_provider() {
        use crate::diagnostics::SchedulerMode;
        let cfg = WeightedConfig {
            base: Config {
                memory_scheduler_enabled: false,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            ..WeightedConfig::default()
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_provider(1024));
        // No permits have flowed: mode must still be Disabled, never
        // inferred Provider from empty counters.
        assert_eq!(gate.diagnostics().scheduler_mode, SchedulerMode::Disabled);
        let _permit = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        assert_eq!(gate.diagnostics().scheduler_mode, SchedulerMode::Disabled);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn degraded_gate_reports_thread_cap_only() {
        use crate::diagnostics::SchedulerMode;
        let cfg = WeightedConfig {
            base: Config {
                fail_open_on_provider_error: true,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            ..WeightedConfig::default()
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, failing_stats());
        let _permit = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        let diag = gate.diagnostics();
        assert_eq!(diag.scheduler_mode, SchedulerMode::ThreadCapOnly);
        assert!(diag.provider_failures >= 1);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn timeout_counts_and_waiters_converge() {
        // Provider always reports full pressure: every acquire waits, then
        // times out. Timeouts and wait time must be recorded and the waiter
        // count must return to zero on every exit path.
        let cfg = WeightedConfig {
            base: Config {
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::from_millis(1),
            max_page_cache_fraction: 1.0,
            require_page_cache_bytes: false,
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, FixedProvider::shared(0.99));
        assert!(matches!(
            gate.acquire_timeout(64, Duration::from_millis(30)),
            Err(crate::AdmitError::TimedOut)
        ));
        let diag = gate.diagnostics();
        assert_eq!(diag.timeouts, 1);
        assert_eq!(diag.waiters, 0);
        assert!(diag.total_wait_ms >= 1);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn waiter_count_visible_while_blocked() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cfg = WeightedConfig {
            base: Config {
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::from_millis(1),
            max_page_cache_fraction: 1.0,
            require_page_cache_bytes: false,
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, FixedProvider::shared(0.99));
        let gate_clone = gate.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            let _ = gate_clone.acquire_timeout(64, Duration::from_millis(500));
            done_clone.store(true, Ordering::SeqCst);
        });
        // Poll until the waiter registers (bounded: the test fails closed
        // on timeout rather than hanging).
        let started = std::time::Instant::now();
        while gate.diagnostics().waiters == 0 && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(gate.diagnostics().waiters, 1);
        handle.join().unwrap();
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(gate.diagnostics().waiters, 0);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn unknown_cache_sets_explicit_flag() {
        let cfg = WeightedConfig {
            base: Config::default().validate().unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            max_page_cache_fraction: 0.6,
            ..WeightedConfig::default()
        };
        let stats =
            MemoryStats::from_domains(vec![crate::MemoryDomain::new(1000, 900).unwrap()], None)
                .unwrap();
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_of(stats));
        let _permit = gate.acquire_timeout(100, Duration::from_secs(1)).unwrap();
        // Admitted (observation usable) but flagged: the cache policy did
        // not run on this probe.
        assert!(gate.diagnostics().cache_unknown);
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn cancelled_waiter_does_not_leak_waiter_count() {
        let cfg = WeightedConfig {
            base: Config {
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::from_millis(1),
            max_page_cache_fraction: 1.0,
            require_page_cache_bytes: false,
        };
        let gate = AsyncWeightedAdmissionGate::new(cfg, FixedProvider::shared(0.99));
        let gate_clone = gate.clone();
        let handle = tokio::spawn(async move { gate_clone.acquire(64).await });
        // Let the waiter park, then cancel it: the waiter count must
        // converge back to zero.
        let started = std::time::Instant::now();
        while gate.diagnostics().waiters == 0 && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(gate.diagnostics().waiters, 1);
        handle.abort();
        let _ = handle.await;
        assert_eq!(gate.diagnostics().waiters, 0);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn disabled_gate_never_probes_the_provider() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct PanicOnProbe {
            calls: AtomicUsize,
        }
        impl crate::provider::MemoryProvider for PanicOnProbe {
            fn used_fraction(&self) -> Result<f64, ProviderError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("disabled gate must not probe");
            }
            fn stats(&self) -> Result<MemoryStats, ProviderError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("disabled gate must not probe");
            }
        }
        let cfg = WeightedConfig {
            base: Config {
                memory_scheduler_enabled: false,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            ..WeightedConfig::default()
        };
        let provider: SharedMemoryProvider = Arc::new(PanicOnProbe {
            calls: AtomicUsize::new(0),
        });
        let gate = SyncWeightedAdmissionGate::new(cfg, provider);
        let _permit = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        assert_eq!(
            gate.diagnostics().scheduler_mode,
            crate::diagnostics::SchedulerMode::Disabled
        );
    }

    #[cfg(feature = "sync")]
    #[test]
    fn degraded_gate_does_not_reprobe_or_reemit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct SharedFailures {
            calls: Arc<AtomicUsize>,
        }
        impl crate::provider::MemoryProvider for SharedFailures {
            fn used_fraction(&self) -> Result<f64, ProviderError> {
                Err(ProviderError::new("down"))
            }
            fn stats(&self) -> Result<MemoryStats, ProviderError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::new("down"))
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: SharedMemoryProvider = Arc::new(SharedFailures {
            calls: Arc::clone(&calls),
        });
        let cfg = WeightedConfig {
            base: Config {
                fail_open_on_provider_error: true,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            ..WeightedConfig::default()
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, provider);
        let _first = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        let _second = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        // One probe total: after fail-open degradation the fast path
        // bypasses the provider instead of failing (and re-emitting) again.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(gate.diagnostics().provider_failures, 1);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn required_cache_is_an_explicit_policy_failure() {
        // Unknown cache + required + fail-closed: provider error, no permit.
        let cfg = WeightedConfig {
            base: Config::default().validate().unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            max_page_cache_fraction: 0.6,
            require_page_cache_bytes: true,
            ..WeightedConfig::default()
        };
        let unknown =
            MemoryStats::from_domains(vec![crate::MemoryDomain::new(1000, 900).unwrap()], None)
                .unwrap();
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_of(unknown));
        assert!(matches!(
            gate.acquire_timeout(100, Duration::from_secs(1)),
            Err(crate::AdmitError::Provider(_))
        ));
        // Unknown cache + required + fail-open: degraded admission.
        let cfg = WeightedConfig {
            base: Config {
                fail_open_on_provider_error: true,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            max_page_cache_fraction: 0.6,
            require_page_cache_bytes: true,
            ..WeightedConfig::default()
        };
        let unknown =
            MemoryStats::from_domains(vec![crate::MemoryDomain::new(1000, 900).unwrap()], None)
                .unwrap();
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_of(unknown));
        let _permit = gate.acquire_timeout(100, Duration::from_secs(1)).unwrap();
        assert!(!gate.memory_scheduler_active());
    }

    #[cfg(feature = "sync")]
    #[test]
    fn panicking_subscriber_cannot_leak_weighted_reservations() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        struct PanickingSubscriber;
        impl tracing::Subscriber for PanickingSubscriber {
            fn register_callsite(
                &self,
                _: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::always()
            }
            fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
                Some(tracing::metadata::LevelFilter::TRACE)
            }
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                panic!("subscriber boom at {}", event.metadata().name());
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        // Start at the pressure-resume boundary; do not depend on observing
        // another thread's tracing callsite registration during a sleep.
        let low = MemoryStats::new(1000, 900, 0).unwrap();
        let provider = stats_of(low);
        let cfg = WeightedConfig {
            base: Config {
                max_ram_fraction: 0.8,
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            stats_max_age: Duration::from_millis(1),
            max_page_cache_fraction: 1.0,
            require_page_cache_bytes: false,
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, provider);
        gate.inner.state.lock().unwrap().throttled = true;
        let dispatch = tracing::dispatcher::Dispatch::new(PanickingSubscriber);
        let result = catch_unwind(AssertUnwindSafe(|| {
            tracing::dispatcher::with_default(&dispatch, || {
                let _permit = gate.acquire_timeout(100, Duration::from_secs(5)).unwrap();
            })
        }));
        assert!(result.is_err(), "subscriber should have panicked");
        // The unwinding permit destructor releases the reservation.
        assert_eq!(gate.committed_bytes(), 0);
        assert_eq!(gate.active_permits(), 0);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn parked_counts_split_by_reason_across_waiters() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cfg = WeightedConfig {
            base: Config {
                max_ram_fraction: 1.0,
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: 128,
            stats_max_age: Duration::from_millis(1),
            max_page_cache_fraction: 1.0,
            require_page_cache_bytes: false,
        };
        // total 1024, available 512.
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_provider(512));
        let _holder = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        // Budget waiter: projected (512 + 64 + 500) / 1024 > 1.0.
        let budget_gate = gate.clone();
        let budget_done = Arc::new(AtomicBool::new(false));
        let budget_flag = Arc::clone(&budget_done);
        let budget_handle = std::thread::spawn(move || {
            let result = budget_gate.acquire_timeout(500, Duration::from_millis(600));
            budget_flag.store(true, Ordering::SeqCst);
            result
        });
        // Exclusivity waiter: 256 > max_single 128 while a permit is out.
        let exclusive_gate = gate.clone();
        let exclusive_done = Arc::new(AtomicBool::new(false));
        let exclusive_flag = Arc::clone(&exclusive_done);
        let exclusive_handle = std::thread::spawn(move || {
            let result = exclusive_gate.acquire_timeout(256, Duration::from_millis(600));
            exclusive_flag.store(true, Ordering::SeqCst);
            result
        });
        let started = Instant::now();
        while gate.diagnostics().waiters < 2 && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let diag = gate.diagnostics();
        assert_eq!(diag.waiters, 2);
        assert_eq!(diag.waiters_by_reason.budget, 1);
        assert_eq!(diag.waiters_by_reason.exclusivity, 1);
        assert_eq!(diag.waiters_by_reason.total(), 2);
        // The reported reason is whichever waiter probed last; it must be
        // one of the actually parked reasons.
        assert!(
            matches!(
                diag.wait_reason,
                Some(crate::diagnostics::WaitReason::Budget)
                    | Some(crate::diagnostics::WaitReason::Exclusivity)
            ),
            "{diag:?}"
        );
        // The holder stays out while both waiters exhaust their 600ms
        // budgets: neither can ever be admitted (the budget waiter lacks
        // bytes and fraction headroom; the exclusivity waiter lacks a
        // quiet gate), so both time out deterministically — no wake-up
        // race to flake on.
        let budget_result = budget_handle.join().unwrap();
        let exclusive_result = exclusive_handle.join().unwrap();
        assert!(matches!(budget_result, Err(crate::AdmitError::TimedOut)));
        assert!(matches!(exclusive_result, Err(crate::AdmitError::TimedOut)));
        assert!(budget_done.load(Ordering::SeqCst));
        assert!(exclusive_done.load(Ordering::SeqCst));
        drop(_holder);
        let diag = gate.diagnostics();
        assert_eq!(diag.timeouts, 2);
        assert_eq!(diag.waiters, 0);
        assert_eq!(diag.waiters_by_reason.total(), 0);
        assert_eq!(diag.wait_reason, None);
        assert!(diag.total_wait_ms >= 1);
    }

    #[cfg(feature = "sync")]
    #[test]
    fn wait_reasons_are_request_specific_and_exposed() {
        use crate::diagnostics::WaitReason;
        let cfg = WeightedConfig {
            base: Config {
                max_ram_fraction: 1.0,
                poll_interval: Duration::from_millis(10),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            safety_reserve_bytes: 0,
            max_single_weight_bytes: 128,
            stats_max_age: Duration::from_millis(1),
            ..WeightedConfig::default()
        };
        let gate = SyncWeightedAdmissionGate::new(cfg, stats_provider(1024));
        let _small = gate.acquire_timeout(64, Duration::from_secs(1)).unwrap();
        // Oversized while busy → Exclusivity, not Pressure.
        match gate.inner.try_admit(256).0 {
            AttemptOutcome::Wait { reason } => assert_eq!(reason, WaitReason::Exclusivity),
            other => panic!("expected Exclusivity wait, got {other:?}"),
        }
        // A bare probe parks nobody, so diagnostics stay quiet; the live
        // parked reason is covered by `waiter_count_visible_while_blocked`.
        assert_eq!(gate.wait_reason(), None);
    }
}
