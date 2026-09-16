//! Shared memory-admission coordinator (in-process).
//!
//! One `Coordinator` owns a single byte budget shared by every registered
//! application. Two clients that both observe free memory cannot both commit
//! to it: the second waits or times out instead of overcommitting.
//!
//! Behavioral contract: `docs/admission-contract.md`. Grants are
//! non-revocable, released only on drop after confirmed termination, and
//! never silently extended. Cross-process transport is future work; this
//! type proves the accounting model in-process.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::AdmitError;
use crate::provider::SharedMemoryProvider;
use crate::weighted::{SyncWeightedAdmissionGate, WeightedConfig, WeightedPermit};

/// Failure to register or acquire a grant.
#[derive(Debug)]
pub enum CoordinatorError {
    /// Application name is empty.
    InvalidApp,
    /// Application already registered.
    DuplicateApp(String),
    /// Application is not registered. Unregistered work is refused, never
    /// admitted unconstrained.
    UnregisteredApp(String),
    /// Requested weight is zero. Grants authorize a real ceiling.
    InvalidWeight,
    /// The shared budget refused the request.
    Admission(AdmitError),
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidApp => f.write_str("application name must not be empty"),
            Self::DuplicateApp(app) => write!(f, "application already registered: {app}"),
            Self::UnregisteredApp(app) => write!(f, "application not registered: {app}"),
            Self::InvalidWeight => f.write_str("grant weight must be greater than zero"),
            Self::Admission(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Admission(error) => Some(error),
            _ => None,
        }
    }
}

struct Inner {
    gate: SyncWeightedAdmissionGate,
    /// Per-application committed bytes. Registration inserts a zero entry;
    /// entries are never removed, so a crashed client stays accounted for.
    committed: Mutex<BTreeMap<String, u64>>,
}

/// Shared budget owner. Clone freely; all clones draw from the same budget.
#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<Inner>,
}

/// A held grant. Releases its reservation on drop.
pub struct Grant {
    inner: Arc<Inner>,
    app: String,
    permit: Option<WeightedPermit>,
}

impl Grant {
    /// Owning application.
    #[must_use]
    pub fn app(&self) -> &str {
        &self.app
    }

    /// Reserved ceiling in bytes.
    #[must_use]
    pub fn weight(&self) -> u64 {
        self.permit.as_ref().map_or(0, WeightedPermit::weight)
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            let weight = permit.weight();
            drop(permit);
            if let Ok(mut committed) = self.inner.committed.lock() {
                let entry = committed.entry(self.app.clone()).or_default();
                *entry = entry.saturating_sub(weight);
            }
        }
    }
}

impl Coordinator {
    /// Build a coordinator over one shared weighted budget.
    #[must_use]
    pub fn new(config: WeightedConfig, provider: SharedMemoryProvider) -> Self {
        Self {
            inner: Arc::new(Inner {
                gate: SyncWeightedAdmissionGate::new(config, provider),
                committed: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Register an application. Must happen before any acquisition.
    ///
    /// # Errors
    /// Returns [`CoordinatorError`] for empty or duplicate names.
    pub fn register(&self, app: &str) -> Result<(), CoordinatorError> {
        if app.is_empty() {
            return Err(CoordinatorError::InvalidApp);
        }
        let mut committed = self.inner.committed.lock().expect("coordinator poisoned");
        if committed.contains_key(app) {
            return Err(CoordinatorError::DuplicateApp(app.to_owned()));
        }
        committed.insert(app.to_owned(), 0);
        Ok(())
    }

    /// Reserve `weight` bytes for `app`, waiting up to `timeout`.
    ///
    /// # Errors
    /// Returns [`CoordinatorError`] for unregistered apps, zero weights, or
    /// when the shared budget refuses.
    pub fn acquire(
        &self,
        app: &str,
        weight: u64,
        timeout: Duration,
    ) -> Result<Grant, CoordinatorError> {
        if weight == 0 {
            return Err(CoordinatorError::InvalidWeight);
        }
        {
            let committed = self.inner.committed.lock().expect("coordinator poisoned");
            if !committed.contains_key(app) {
                return Err(CoordinatorError::UnregisteredApp(app.to_owned()));
            }
        }
        let permit = self
            .inner
            .gate
            .acquire_timeout(weight, timeout)
            .map_err(CoordinatorError::Admission)?;
        {
            let mut committed = self.inner.committed.lock().expect("coordinator poisoned");
            // Registration cannot be removed, but a poisoned-then-recovered
            // lock must not mint grants for unknown apps.
            let entry = committed
                .get_mut(app)
                .ok_or_else(|| CoordinatorError::UnregisteredApp(app.to_owned()))?;
            *entry = entry.saturating_add(weight);
        }
        Ok(Grant {
            inner: Arc::clone(&self.inner),
            app: app.to_owned(),
            permit: Some(permit),
        })
    }

    /// Total committed bytes across all applications.
    #[must_use]
    pub fn committed_bytes(&self) -> u64 {
        self.inner.gate.committed_bytes()
    }

    /// Committed bytes for one application (zero when unregistered).
    #[must_use]
    pub fn app_committed(&self, app: &str) -> u64 {
        self.inner
            .committed
            .lock()
            .expect("coordinator poisoned")
            .get(app)
            .copied()
            .unwrap_or(0)
    }

    /// Registered application names.
    #[must_use]
    pub fn registered_apps(&self) -> Vec<String> {
        self.inner
            .committed
            .lock()
            .expect("coordinator poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{MemoryProvider, MemoryStats, ProviderError};
    use std::sync::Arc as StdArc;

    struct FixedBytes {
        total: u64,
        available: u64,
    }

    impl MemoryProvider for FixedBytes {
        fn used_fraction(&self) -> Result<f64, ProviderError> {
            Ok(self.stats()?.used_fraction())
        }

        fn stats(&self) -> Result<MemoryStats, ProviderError> {
            MemoryStats::new(self.total, self.available, 0)
        }
    }

    fn coordinator() -> Coordinator {
        let config = WeightedConfig {
            safety_reserve_bytes: 0,
            max_single_weight_bytes: u64::MAX,
            ..WeightedConfig::default()
        };
        Coordinator::new(
            config,
            StdArc::new(FixedBytes {
                total: 1000,
                available: 1000,
            }),
        )
    }

    #[test]
    fn registration_rules_are_fail_closed() {
        let coord = coordinator();
        assert!(matches!(
            coord.acquire("ghost", 100, Duration::ZERO),
            Err(CoordinatorError::UnregisteredApp(_))
        ));
        assert!(matches!(
            coord.register(""),
            Err(CoordinatorError::InvalidApp)
        ));
        coord.register("bekiper").unwrap();
        assert!(matches!(
            coord.register("bekiper"),
            Err(CoordinatorError::DuplicateApp(_))
        ));
        assert!(matches!(
            coord.acquire("bekiper", 0, Duration::ZERO),
            Err(CoordinatorError::InvalidWeight)
        ));
    }

    #[test]
    fn two_clients_cannot_overcommit_the_shared_budget() {
        let coord = coordinator();
        coord.register("bekiper").unwrap();
        coord.register("embedder").unwrap();

        let first = coord.acquire("bekiper", 600, Duration::ZERO).unwrap();
        assert_eq!(coord.committed_bytes(), 600);
        // 600 + 600 > 1000: the second client waits, then times out.
        assert!(matches!(
            coord.acquire("embedder", 600, Duration::from_millis(30)),
            Err(CoordinatorError::Admission(AdmitError::TimedOut))
        ));
        assert_eq!(coord.committed_bytes(), 600);
        drop(first);
        assert_eq!(coord.committed_bytes(), 0);
        let second = coord.acquire("embedder", 600, Duration::ZERO).unwrap();
        assert_eq!(second.weight(), 600);
        assert_eq!(coord.app_committed("embedder"), 600);
        assert_eq!(coord.app_committed("bekiper"), 0);
    }

    #[test]
    fn concurrent_clients_share_one_budget() {
        let coord = coordinator();
        for app in ["a", "b", "c", "d"] {
            coord.register(app).unwrap();
        }
        // Four threads × 400 against a 1000 budget: at most two hold grants
        // at once; every thread eventually completes without overcommit.
        let peak = StdArc::new(std::sync::atomic::AtomicU64::new(0));
        std::thread::scope(|scope| {
            for app in ["a", "b", "c", "d"] {
                let coord = coord.clone();
                let peak = peak.clone();
                scope.spawn(move || {
                    for _ in 0..10 {
                        let grant = coord.acquire(app, 400, Duration::from_secs(5)).unwrap();
                        peak.fetch_max(
                            coord.committed_bytes(),
                            std::sync::atomic::Ordering::SeqCst,
                        );
                        drop(grant);
                    }
                });
            }
        });
        let peak = peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(peak <= 1000, "shared budget overcommitted: {peak}");
        assert_eq!(coord.committed_bytes(), 0);
    }
}
