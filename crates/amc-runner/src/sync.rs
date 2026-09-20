//! Synchronous, `Condvar`-backed admission gate.
//!
//! Use this with thread-pool workers (Rayon, std::thread) where blocking the
//! current OS thread is acceptable while waiting for memory pressure to clear.

use std::sync::{Arc, Condvar, Mutex};

use crate::AdmitError;
use crate::config::Config;
use crate::gate_common::{Decision, GateState, decide};
use crate::provider::{SharedMemoryProvider, finite_fraction};

/// Synchronous admission gate.
///
/// Each call to [`AdmissionGate::acquire`] blocks the calling thread until
/// host RAM usage is below the configured ceiling, then returns a
/// [`AdmissionPermit`] whose `Drop` releases the slot.
#[derive(Clone)]
pub struct AdmissionGate {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<GateState>,
    condvar: Condvar,
    provider: SharedMemoryProvider,
    config: Config,
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
        let (state, init_event) = GateState::new(&config, &provider);
        let inner = Arc::new(Inner {
            state: Mutex::new(state),
            condvar: Condvar::new(),
            provider,
            config,
        });
        // No lock is held here; safe to invoke the subscriber.
        if let Some(event) = init_event {
            crate::gate_common::emit_gate_event(event);
        }
        Self { inner }
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
        use crate::diagnostics::WaitReason;
        use crate::gate_common::{WaiterGuard, emit_gate_event};
        let deadline = std::time::Instant::now().checked_add(timeout);
        let mut attempted = false;
        // Probe outside the lock; only the decision mutates state.
        loop {
            if attempted && deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                if let Ok(mut state) = self.inner.state.lock() {
                    state.record_timeout();
                }
                return Err(AdmitError::TimedOut);
            }
            attempted = true;
            let usage = self
                .inner
                .provider
                .used_fraction()
                .and_then(finite_fraction);
            let (decision, event) = {
                let mut state = self.lock();
                decide(&mut state, usage, &self.inner.config)
            };
            match decision {
                Decision::Admitted => {
                    // RAII ownership first: the permit exists before any
                    // subscriber code runs, so a panicking subscriber
                    // unwinds through a live permit whose destructor
                    // releases the slot.
                    let permit = AdmissionPermit {
                        gate: Some(self.clone()),
                    };
                    // Subscriber work runs without the admission lock held.
                    if let Some(event) = event {
                        emit_gate_event(event);
                    }
                    return Ok(permit);
                }
                Decision::ProviderFailed(e) => {
                    if let Some(event) = event {
                        emit_gate_event(event);
                    }
                    return Err(AdmitError::Provider(e));
                }
                Decision::Wait => {
                    if let Some(event) = event {
                        emit_gate_event(event);
                    }
                    // Park for this wait cycle only. The guard counts the
                    // parked request and accounts its duration on drop, so
                    // probing iterations are not waiters and cancellation
                    // converges both.
                    let _parked = WaiterGuard::enter(&self.inner.state, WaitReason::Pressure);
                    let wait = deadline.map_or(self.inner.config.poll_interval, |d| {
                        d.saturating_duration_since(std::time::Instant::now())
                            .min(self.inner.config.poll_interval)
                    });
                    if wait.is_zero() {
                        if let Ok(mut state) = self.inner.state.lock() {
                            state.record_timeout();
                        }
                        return Err(AdmitError::TimedOut);
                    }
                    let (guard, _) = self
                        .inner
                        .condvar
                        .wait_timeout(self.lock(), wait)
                        .expect("admission gate mutex poisoned while waiting");
                    drop(guard);
                }
            }
        }
    }

    /// Consistent diagnostics snapshot: permits, mode, throttled state,
    /// last wait reason, waiter count, and transition/error/timeout counts.
    /// State is read under the lock; no formatting happens under it.
    pub fn diagnostics(&self) -> crate::diagnostics::DiagnosticsSnapshot {
        self.lock().snapshot()
    }

    /// Whether the memory-aware scheduler is currently active. Returns `false`
    /// once the provider has been disabled due to a runtime failure.
    pub fn memory_scheduler_active(&self) -> bool {
        self.lock().memory_scheduler_active
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

    /// Human-readable identifier of the active memory source.
    pub fn memory_source(&self) -> &'static str {
        self.lock().memory_source.as_str()
    }

    /// Number of permits currently in flight.
    pub fn active_tasks(&self) -> usize {
        self.lock().active_tasks
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
    use crate::provider::ProviderError;
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

    #[test]
    fn sensitive_provider_text_never_reaches_event_fields_or_errors() {
        use std::sync::Mutex;
        struct FieldSink {
            fields: Arc<Mutex<Vec<String>>>,
        }
        struct Grab<'a>(&'a Mutex<Vec<String>>);
        impl tracing::field::Visit for Grab<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .lock()
                    .expect("field sink poisoned")
                    .push(format!("{field}={value:?}"));
            }
        }
        impl tracing::Subscriber for FieldSink {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                event.record(&mut Grab(&self.fields));
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let fields = Arc::new(Mutex::new(Vec::new()));
        let sink = FieldSink {
            fields: Arc::clone(&fields),
        };
        let dispatch = tracing::dispatcher::Dispatch::new(sink);
        tracing::dispatcher::with_default(&dispatch, || {
            let provider: SharedMemoryProvider =
                Arc::new(|| Err(ProviderError::new("SENSITIVE token=abc pid=1234")));
            let gate = AdmissionGate::new(Config::default().validate().unwrap(), provider);
            let error = match gate.acquire_timeout(Duration::from_millis(20)) {
                Ok(_) => panic!("expected provider error"),
                Err(error) => error,
            };
            // Formatted error chains must carry only the reason code.
            assert!(!format!("{error:#}").contains("SENSITIVE"), "{error:#}");
        });
        let captured = fields.lock().expect("field sink poisoned").join("\n");
        assert!(!captured.contains("SENSITIVE"), "{captured}");
        assert!(
            captured.contains("provider-source-unavailable"),
            "{captured}"
        );
    }

    #[test]
    fn panicking_subscriber_cannot_leak_permits() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct PanicOnSecond {
            events: AtomicUsize,
        }
        impl tracing::Subscriber for PanicOnSecond {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                if self.events.fetch_add(1, Ordering::SeqCst) >= 1 {
                    panic!("subscriber boom at {}", event.metadata().name());
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        // First two probes throttle (event 0, no panic); the third sees
        // pressure cleared and admits with a resume event (event 1,
        // panics) after the slot is already committed.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_provider = Arc::clone(&calls);
        let provider: SharedMemoryProvider = Arc::new(move || {
            let n = calls_for_provider.fetch_add(1, Ordering::SeqCst);
            Ok(if n < 2 { 0.95 } else { 0.50 })
        });
        let gate = AdmissionGate::new(
            Config {
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            provider,
        );
        let dispatch = tracing::dispatcher::Dispatch::new(PanicOnSecond {
            events: AtomicUsize::new(0),
        });
        let result = catch_unwind(AssertUnwindSafe(|| {
            tracing::dispatcher::with_default(&dispatch, || {
                let _permit = gate.acquire().unwrap();
            })
        }));
        assert!(result.is_err(), "subscriber should have panicked");
        // The unwinding permit destructor releases the slot.
        assert_eq!(gate.active_tasks(), 0);
    }

    #[test]
    fn timeout_is_counted_and_waiters_converge() {
        let gate = AdmissionGate::new(
            Config {
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            FixedProvider::shared(0.99),
        );
        assert!(matches!(
            gate.acquire_timeout(Duration::from_millis(30)),
            Err(AdmitError::TimedOut)
        ));
        let diag = gate.diagnostics();
        assert_eq!(diag.timeouts, 1);
        assert_eq!(diag.waiters, 0);
        assert!(diag.total_wait_ms >= 1);
        // Nobody is parked after the timeout returns, so no stale reason
        // may linger in diagnostics.
        assert_eq!(diag.wait_reason, None);
        assert_eq!(diag.waiters_by_reason.total(), 0);
    }

    #[test]
    fn disabled_gate_reports_disabled_mode() {
        let gate = AdmissionGate::new(
            Config {
                memory_scheduler_enabled: false,
                ..Config::default()
            }
            .validate()
            .unwrap(),
            FixedProvider::shared(0.99),
        );
        let diag = gate.diagnostics();
        assert_eq!(
            diag.scheduler_mode,
            crate::diagnostics::SchedulerMode::Disabled
        );
        assert_eq!(diag.throttle_entries, 0);
    }

    #[test]
    fn throttle_resume_transition_counts_are_real() {
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
        let _permit = gate.acquire().unwrap();
        let diag = gate.diagnostics();
        assert_eq!(diag.throttle_entries, 1);
        assert_eq!(diag.resume_count, 1);
        assert!(gate.memory_scheduler_active());
    }
}
