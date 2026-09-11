//! Asynchronous, `tokio::sync::Notify`-backed admission gate.
//!
//! Use this with Tokio fanout patterns like `futures::stream::buffer_unordered`
//! where blocking the executor thread would starve other tasks.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

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
    notify: Notify,
    provider: SharedMemoryProvider,
    config: Config,
}

/// Asynchronous admission gate.
///
/// Each call to [`AdmissionGate::acquire`] yields the current task until host
/// RAM usage is below the configured ceiling, then returns a permit whose
/// `Drop` releases the slot.
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

    /// Build a gate from a validated config and memory provider.
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
                notify: Notify::new(),
                provider,
                config,
            }),
        }
    }

    /// Yield until a slot is available, then return a permit.
    pub async fn acquire(&self) -> Result<AdmissionPermit, AdmitError> {
        loop {
            {
                let mut state = self.lock();
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
                                    "RAM usage back below resume threshold; resuming admissions"
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
            }

            // Wait for either a release or the poll interval, whichever comes first.
            let notified = self.inner.notify.notified();
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(self.inner.config.poll_interval) => {}
            }
        }
    }

    /// Whether the memory-aware scheduler is still active.
    pub fn memory_scheduler_active(&self) -> bool {
        self.lock().memory_scheduler_active
    }

    /// Human-readable identifier for the memory source.
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
        // Wake every waiter so they can re-check the provider; cheap because
        // each waiter's await branch goes through select! and re-acquires the
        // lock briefly before deciding whether to resume.
        self.inner.notify.notify_waiters();
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

    #[tokio::test]
    async fn admits_when_below_threshold() {
        let gate = AdmissionGate::new(
            Config::default().validate().unwrap(),
            FixedProvider::shared(0.10),
        );
        let _p = gate.acquire().await.unwrap();
    }

    #[tokio::test]
    async fn throttles_then_resumes_via_hysteresis() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_provider = Arc::clone(&calls);
        let provider: SharedMemoryProvider = Arc::new(move || {
            let n = calls_for_provider.fetch_add(1, Ordering::SeqCst);
            Ok(if n < 2 { 0.95 } else { 0.50 })
        });
        let gate = AdmissionGate::new(
            Config {
                poll_interval: Duration::from_millis(10),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            provider,
        );
        let _p = gate.acquire().await.unwrap();
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn falls_back_when_disabled() {
        let cfg = Config {
            memory_scheduler_enabled: false,
            ..Config::default()
        }
        .validate()
        .unwrap();
        let gate = AdmissionGate::new(cfg, FixedProvider::shared(0.99));
        let _p = gate.acquire().await.unwrap();
        assert!(!gate.memory_scheduler_active());
    }

    #[tokio::test]
    async fn active_tasks_tracks_permits() {
        let gate = AdmissionGate::new(Config::validated_default(), FixedProvider::shared(0.10));
        let permit = gate.acquire().await.unwrap();
        assert_eq!(gate.active_tasks(), 1);
        drop(permit);
        assert_eq!(gate.active_tasks(), 0);
    }

    #[test]
    fn new_default_constructs() {
        let _gate = AdmissionGate::new_default();
    }
}
