//! Synchronous, `Condvar`-backed admission gate.
//!
//! Use this with thread-pool workers (Rayon, std::thread) where blocking the
//! current OS thread is acceptable while waiting for memory pressure to clear.

use std::sync::{Arc, Condvar, Mutex};

use crate::AdmitError;
use crate::config::Config;
use crate::provider::{ProviderError, SharedMemoryProvider, finite_fraction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemorySource {
    Disabled,
    Provider,
    ThreadCapOnly,
}

impl MemorySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Provider => "provider",
            Self::ThreadCapOnly => "thread_cap_only",
        }
    }
}

#[derive(Debug)]
struct GateState {
    active_tasks: usize,
    throttled: bool,
    throttle_logged: bool,
    memory_scheduler_active: bool,
    memory_source: MemorySource,
    provider_failure_logged: bool,
}

struct Inner {
    state: Mutex<GateState>,
    condvar: Condvar,
    provider: SharedMemoryProvider,
    config: Config,
}

/// Synchronous admission gate.
///
/// Each call to [`AdmissionGate::acquire`] blocks the calling thread until
/// host RAM usage is below the configured ceiling, then returns a
/// [`AdmissionPermit`] whose `Drop` releases the slot.
#[derive(Clone)]
pub struct AdmissionGate {
    inner: Arc<Inner>,
}

impl AdmissionGate {
    /// Build a gate using [`Config::validated_default`] and
    /// [`providers::default_provider`].
    ///
    /// [`providers::default_provider`]: crate::providers::default_provider
    #[must_use]
    pub fn new_default() -> Self {
        Self::new(
            Config::validated_default(),
            crate::providers::default_provider(),
        )
    }

    /// Build a gate from a validated config and a memory provider.
    #[must_use]
    pub fn new(config: Config, provider: SharedMemoryProvider) -> Self {
        let mut memory_source = if config.memory_scheduler_enabled {
            MemorySource::Provider
        } else {
            MemorySource::Disabled
        };
        let mut scheduler_active = config.memory_scheduler_enabled;
        let mut provider_failure_logged = false;

        if scheduler_active && let Err(e) = provider.used_fraction() {
            tracing::warn!(
                error = %e,
                fail_open = config.fail_open_on_provider_error,
                "memory provider failed at init"
            );
            if config.fail_open_on_provider_error {
                scheduler_active = false;
                memory_source = MemorySource::ThreadCapOnly;
            }
            provider_failure_logged = true;
        }

        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(GateState {
                    active_tasks: 0,
                    throttled: false,
                    throttle_logged: false,
                    memory_scheduler_active: scheduler_active,
                    memory_source,
                    provider_failure_logged,
                }),
                condvar: Condvar::new(),
                provider,
                config,
            }),
        }
    }

    /// Block until a slot is available, then return a permit.
    ///
    /// The returned permit holds the slot open; drop it to release.
    pub fn acquire(&self) -> Result<AdmissionPermit, AdmitError> {
        self.acquire_timeout(std::time::Duration::MAX)
    }

    /// Like [`Self::acquire`], but fail with [`AdmitError::TimedOut`] after `timeout`.
    ///
    /// The first probe always runs. The timeout bounds subsequent waits, not
    /// provider I/O.
    pub fn acquire_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<AdmissionPermit, AdmitError> {
        let deadline = std::time::Instant::now().checked_add(timeout);
        let mut attempted = false;
        let mut state = self.lock();
        loop {
            if attempted && deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                return Err(AdmitError::TimedOut);
            }
            attempted = true;
            if !state.memory_scheduler_active {
                state.active_tasks += 1;
                return Ok(AdmissionPermit {
                    gate: Some(self.clone()),
                });
            }

            match self
                .inner
                .provider
                .used_fraction()
                .and_then(finite_fraction)
            {
                Ok(usage) => {
                    if state.throttled {
                        if usage < self.inner.config.resume_ram_fraction() {
                            state.throttled = false;
                            state.throttle_logged = false;
                            tracing::info!(
                                current_ram_fraction = usage,
                                resume_ram_fraction = self.inner.config.resume_ram_fraction(),
                                active_tasks = state.active_tasks,
                                "RAM usage back below the resume threshold; resuming admissions"
                            );
                            state.active_tasks += 1;
                            return Ok(AdmissionPermit {
                                gate: Some(self.clone()),
                            });
                        }
                        self.log_throttle(&mut state, usage);
                    } else if usage > self.inner.config.max_ram_fraction {
                        state.throttled = true;
                        state.throttle_logged = false;
                        self.log_throttle(&mut state, usage);
                    } else {
                        state.active_tasks += 1;
                        return Ok(AdmissionPermit {
                            gate: Some(self.clone()),
                        });
                    }
                }
                Err(e) => {
                    if self.inner.config.fail_open_on_provider_error {
                        self.disable_memory_scheduler(&mut state, &e);
                        state.active_tasks += 1;
                        return Ok(AdmissionPermit {
                            gate: Some(self.clone()),
                        });
                    }
                    return Err(AdmitError::Provider(e));
                }
            }

            let wait = deadline.map_or(self.inner.config.poll_interval, |d| {
                d.saturating_duration_since(std::time::Instant::now())
                    .min(self.inner.config.poll_interval)
            });
            if wait.is_zero() {
                return Err(AdmitError::TimedOut);
            }
            let (next_state, _) = self
                .inner
                .condvar
                .wait_timeout(state, wait)
                .expect("admission gate mutex poisoned while waiting");
            state = next_state;
        }
    }

    /// Whether the memory-aware scheduler is currently active. Returns `false`
    /// once the provider has been disabled due to a runtime failure.
    pub fn memory_scheduler_active(&self) -> bool {
        self.lock().memory_scheduler_active
    }

    /// Human-readable identifier of the active memory source.
    pub fn memory_source(&self) -> &'static str {
        self.lock().memory_source.as_str()
    }

    /// Number of permits currently in flight.
    pub fn active_tasks(&self) -> usize {
        self.lock().active_tasks
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.inner
            .state
            .lock()
            .expect("admission gate mutex poisoned")
    }

    fn release(&self) {
        let mut state = self.lock();
        if state.active_tasks > 0 {
            state.active_tasks -= 1;
        }
        self.inner.condvar.notify_all();
    }

    fn log_throttle(&self, state: &mut GateState, usage: f64) {
        if state.throttle_logged {
            return;
        }
        if state.active_tasks > 0 {
            tracing::warn!(
                current_ram_fraction = usage,
                max_ram_fraction = self.inner.config.max_ram_fraction,
                active_tasks = state.active_tasks,
                "RAM usage above threshold while tasks are in flight; throttling new admissions"
            );
        } else {
            tracing::info!(
                current_ram_fraction = usage,
                max_ram_fraction = self.inner.config.max_ram_fraction,
                "RAM usage above threshold; waiting before admitting more work"
            );
        }
        state.throttle_logged = true;
    }

    fn disable_memory_scheduler(&self, state: &mut GateState, error: &ProviderError) {
        if !state.provider_failure_logged {
            tracing::warn!(
                error = %error,
                "memory provider failed at runtime; falling back to thread-cap-only scheduling"
            );
            state.provider_failure_logged = true;
        }
        state.memory_scheduler_active = false;
        state.throttled = false;
        state.throttle_logged = false;
        state.memory_source = MemorySource::ThreadCapOnly;
    }
}

/// RAII permit handed out by [`AdmissionGate::acquire`].
pub struct AdmissionPermit {
    gate: Option<AdmissionGate>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            gate.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::providers::FixedProvider;

    #[test]
    fn admits_when_below_threshold() {
        let cfg = Config::default().validate().unwrap();
        let gate = AdmissionGate::new(cfg, FixedProvider::shared(0.10));
        let _permit = gate.acquire().unwrap();
        assert!(gate.memory_scheduler_active());
    }

    #[test]
    fn falls_back_when_provider_disabled() {
        let cfg = Config {
            memory_scheduler_enabled: false,
            ..Config::default()
        }
        .validate()
        .unwrap();
        let gate = AdmissionGate::new(cfg, FixedProvider::shared(0.99));
        let _permit = gate.acquire().unwrap(); // would otherwise block forever
        assert!(!gate.memory_scheduler_active());
        assert_eq!(gate.memory_source(), "disabled");
    }

    #[test]
    fn permit_release_decrements() {
        let gate = AdmissionGate::new(
            Config::default().validate().unwrap(),
            FixedProvider::shared(0.10),
        );
        {
            let _p = gate.acquire().unwrap();
            assert_eq!(gate.lock().active_tasks, 1);
            assert_eq!(gate.active_tasks(), 1);
        }
        assert_eq!(gate.lock().active_tasks, 0);
        assert_eq!(gate.active_tasks(), 0);
    }

    #[test]
    fn new_default_constructs() {
        let _gate = AdmissionGate::new_default();
    }

    #[test]
    fn throttles_then_resumes_via_hysteresis() {
        // Provider switches from over-threshold to below-resume after the first probe.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_provider = Arc::clone(&calls);
        let provider = Arc::new(move || -> Result<f64, ProviderError> {
            let n = calls_for_provider.fetch_add(1, Ordering::SeqCst);
            // First two probes: 0.95 (throttle). Then 0.50 (resume).
            Ok(if n < 2 { 0.95 } else { 0.50 })
        });
        let gate = AdmissionGate::new(
            Config {
                poll_interval: Duration::from_millis(20),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            provider,
        );

        let _permit = gate.acquire().unwrap();
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[test]
    fn provider_runtime_failure_falls_back() {
        let provider: SharedMemoryProvider =
            Arc::new(|| -> Result<f64, ProviderError> { Err(ProviderError::new("boom")) });
        let cfg = Config {
            fail_open_on_provider_error: true,
            ..Config::default()
        }
        .validate()
        .unwrap();
        let gate = AdmissionGate::new(cfg, provider);
        let _permit = gate.acquire().unwrap();
        assert!(!gate.memory_scheduler_active());
        assert_eq!(gate.memory_source(), "thread_cap_only");
    }

    #[test]
    fn provider_failure_is_closed_by_default() {
        let provider: SharedMemoryProvider =
            Arc::new(|| -> Result<f64, ProviderError> { Err(ProviderError::new("boom")) });
        let gate = AdmissionGate::new(Config::default().validate().unwrap(), provider);
        assert!(matches!(gate.acquire(), Err(AdmitError::Provider(_))));
        assert!(gate.memory_scheduler_active());
    }
}
