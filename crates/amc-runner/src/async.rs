//! Asynchronous, `tokio::sync::Notify`-backed admission gate.
//!
//! Use this with Tokio fanout patterns like `futures::stream::buffer_unordered`
//! where blocking the executor thread would starve other tasks.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::AdmitError;
use crate::config::Config;
use crate::gate_common::{Decision, GateState, decide};
use crate::provider::{SharedMemoryProvider, finite_fraction};

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
        let (state, init_event) = GateState::new(&config, &provider);
        let gate = Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                notify: Notify::new(),
                provider,
                config,
            }),
        };
        if let Some(event) = init_event {
            crate::gate_common::emit_gate_event(event);
        }
        gate
    }

    /// Yield until a slot is available, then return a permit.
    pub async fn acquire(&self) -> Result<AdmissionPermit, AdmitError> {
        loop {
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
                    // RAII ownership before subscriber work (see sync gate).
                    let permit = AdmissionPermit {
                        gate: Some(self.clone()),
                    };
                    if let Some(event) = event {
                        crate::gate_common::emit_gate_event(event);
                    }
                    return Ok(permit);
                }
                Decision::ProviderFailed(e) => {
                    if let Some(event) = event {
                        crate::gate_common::emit_gate_event(event);
                    }
                    return Err(AdmitError::Provider(e));
                }
                Decision::Wait => {
                    if let Some(event) = event {
                        crate::gate_common::emit_gate_event(event);
                    }
                    // Parked guard spans the sleep only: dropping this
                    // future (cancellation) converges both the count and
                    // the parked duration.
                    let _parked = crate::gate_common::WaiterGuard::enter(
                        &self.inner.state,
                        crate::diagnostics::WaitReason::Pressure,
                    );
                    // Wait for either a release or the poll interval, whichever comes first.
                    let notified = self.inner.notify.notified();
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep(self.inner.config.poll_interval) => {}
                    }
                }
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

    /// Consistent diagnostics snapshot (see sync gate).
    pub fn diagnostics(&self) -> crate::diagnostics::DiagnosticsSnapshot {
        self.lock().snapshot()
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
            ..Config::validated_default()
        };
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

    #[tokio::test]
    async fn cancelled_waiter_does_not_leak_waiter_count() {
        let gate = AdmissionGate::new(
            Config {
                poll_interval: Duration::from_millis(5),
                ..Config::default()
            }
            .validate()
            .unwrap(),
            FixedProvider::shared(0.99),
        );
        let gate_clone = gate.clone();
        let handle = tokio::spawn(async move { gate_clone.acquire().await });
        let started = std::time::Instant::now();
        while gate.diagnostics().waiters == 0 && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(gate.diagnostics().waiters, 1);
        handle.abort();
        let _ = handle.await;
        assert_eq!(gate.diagnostics().waiters, 0);
    }
}
