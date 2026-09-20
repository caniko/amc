//! Downstream integration: the library never installs a global subscriber.
//!
//! Run with: `cargo run -p amc-runner --example admission-diagnostics`
//!
//! This installs a minimal `tracing` subscriber in the binary (stderr,
//! reason codes only), drives a weighted gate with a sensitive custom
//! provider, and prints the diagnostics snapshot. Provider source text is
//! never printed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use amc_runner::diagnostics::WaitReason;
use amc_runner::provider::{MemoryProvider, MemoryStats, ProviderError};
use amc_runner::weighted::{SyncWeightedAdmissionGate, WeightedConfig};

struct CountingSubscriber {
    events: AtomicUsize,
}

impl tracing::Subscriber for CountingSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        // Print only the event metadata, never field values that could
        // carry provider text. Reason codes are stable and safe.
        eprintln!("event: {}", event.metadata().name());
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

struct SensitiveProvider;

impl MemoryProvider for SensitiveProvider {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        Err(ProviderError::new("SENSITIVE token=abc pid=1234"))
    }
    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        Err(ProviderError::new("SENSITIVE token=abc pid=1234"))
    }
}

fn main() {
    let subscriber = Arc::new(CountingSubscriber {
        events: AtomicUsize::new(0),
    });
    // Library never calls this; the downstream binary owns the global.
    let _ = tracing::subscriber::set_global_default(subscriber.clone());

    let cfg = WeightedConfig {
        base: amc_runner::Config {
            fail_open_on_provider_error: true,
            ..Default::default()
        }
        .validate()
        .unwrap(),
        safety_reserve_bytes: 0,
        max_single_weight_bytes: u64::MAX,
        ..WeightedConfig::default()
    };
    let gate = SyncWeightedAdmissionGate::new(cfg, Arc::new(SensitiveProvider));
    let permit = gate
        .acquire_timeout(1024, Duration::from_secs(1))
        .expect("fail-open admits");
    assert_eq!(permit.weight(), 1024);
    let diag = gate.diagnostics();
    println!(
        "committed={} permits={} mode={} throttled={} wait={:?}",
        diag.committed_bytes,
        diag.active_permits,
        diag.scheduler_mode.as_str(),
        diag.throttled,
        diag.wait_reason.map(WaitReason::as_str),
    );
    drop(permit);
    eprintln!("subscriber saw events (count only, no bodies)");
}
