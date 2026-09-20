//! Comparison engine: per-metric deltas with explicit unsupported reasons.
//!
//! Classification (stable):
//! - `memory.events`, `memory.events.local`: counters with recorded scope.
//!   Only these support counter deltas.
//! - `cgroup.events`: state transitions (`populated`, `frozen`). Never
//!   blanket-differenced; report transitions explicitly.
//! - usage/limit files: sampled gauges. Differences are sampling artifacts,
//!   reported as gauge change with timing, never as budget accounting.
//! - `memory.peak`, `memory.swap.peak`: kernel high-water marks. Interval
//!   peak is NOT `after - before`; report both marks plus sampled maxima.
//! - `memory.pressure`: retain reported avg10/60/300; derive interval stall
//!   fractions from `total` microseconds over valid elapsed microseconds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::Comparability;

/// Metric classes with distinct comparison rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    State,
    Gauge,
    Peak,
    Pressure,
}

/// Map a file name to its metric class.
pub fn metric_kind(name: &str) -> MetricKind {
    match name {
        "memory.events" | "memory.events.local" => MetricKind::Counter,
        "cgroup.events" => MetricKind::State,
        "memory.pressure" => MetricKind::Pressure,
        "memory.peak" | "memory.swap.peak" => MetricKind::Peak,
        _ => MetricKind::Gauge,
    }
}

/// Stable per-field unsupported-reason codes.
pub mod reason {
    pub const COMPATIBLE: &str = "compatible";
    pub const LIFETIME_MISMATCH: &str = "lifetime-mismatch";
    pub const RESTART: &str = "restart-detected";
    pub const COUNTER_DECREASE: &str = "counter-decrease";
    pub const MISSING_KEY: &str = "missing-key";
    pub const MISSING_IDENTITY: &str = "missing-identity";
    pub const INCOMPATIBLE_CLOCKS: &str = "incompatible-clocks";
    pub const PARTIAL: &str = "partial-observation";
    pub const UNAVAILABLE_OPTIONAL: &str = "unavailable-optional";
    pub const STATE_TRANSITION: &str = "state-transition";
    pub const PEAK_NOT_DELTA: &str = "peak-not-delta";
    pub const GAUGE_SAMPLED: &str = "gauge-sampled";
}

/// One field comparison: either a supported difference or an explicit
/// reason. Never silent omission, never zero-fill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FieldComparison {
    pub kind: String,
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub delta: Option<Value>,
    pub reason: String,
}

impl FieldComparison {
    pub fn supported(kind: &str, delta: Value) -> Self {
        Self {
            kind: kind.to_string(),
            supported: true,
            delta: Some(delta),
            reason: reason::COMPATIBLE.to_string(),
        }
    }

    pub fn unsupported(kind: &str, reason: &str) -> Self {
        Self {
            kind: kind.to_string(),
            supported: false,
            delta: None,
            reason: reason.to_string(),
        }
    }
}

/// Counter delta with lifetime and monotonicity enforcement.
pub fn counter_delta(
    before: u64,
    after: u64,
    comparability: Comparability,
) -> Result<u64, &'static str> {
    match comparability {
        Comparability::Compatible => {}
        Comparability::PathMismatch => return Err(reason::LIFETIME_MISMATCH),
        Comparability::RestartDetected => return Err(reason::RESTART),
        Comparability::ReplacementSuspected => return Err(reason::LIFETIME_MISMATCH),
        Comparability::ContextMismatch => return Err(reason::LIFETIME_MISMATCH),
        Comparability::MissingIdentity => return Err(reason::MISSING_IDENTITY),
        Comparability::Disappeared => return Err(reason::PARTIAL),
    }
    after.checked_sub(before).ok_or(reason::COUNTER_DECREASE)
}

/// Interval stall fraction from PSI `total` microseconds.
/// `elapsed_us` must come from a valid monotonic interval; `None` means
/// incompatible clocks and yields an explicit unsupported outcome.
pub fn psi_stall_fraction(
    before_total: u64,
    after_total: u64,
    elapsed_us: Option<u64>,
) -> Result<f64, &'static str> {
    let elapsed = elapsed_us.ok_or(reason::INCOMPATIBLE_CLOCKS)?;
    if elapsed == 0 {
        return Err(reason::INCOMPATIBLE_CLOCKS);
    }
    let delta = after_total
        .checked_sub(before_total)
        .ok_or(reason::COUNTER_DECREASE)?;
    Ok((delta as f64 / elapsed as f64).clamp(0.0, 1.0))
}

/// Strict interval stall fraction: unlike [`psi_stall_fraction`], a `total`
/// delta larger than the elapsed interval is reported as
/// [`reason::INCOMPATIBLE_CLOCKS`] rather than clamped to `1.0`. A stalled
/// counter cannot accumulate more stall microseconds than elapsed time, so
/// an overshoot means the clocks or the samples are not comparable.
pub fn psi_stall_fraction_strict(
    before_total: u64,
    after_total: u64,
    elapsed_us: Option<u64>,
) -> Result<f64, &'static str> {
    let elapsed = elapsed_us.ok_or(reason::INCOMPATIBLE_CLOCKS)?;
    if elapsed == 0 {
        return Err(reason::INCOMPATIBLE_CLOCKS);
    }
    let delta = after_total
        .checked_sub(before_total)
        .ok_or(reason::COUNTER_DECREASE)?;
    if delta > elapsed {
        return Err(reason::INCOMPATIBLE_CLOCKS);
    }
    Ok(delta as f64 / elapsed as f64)
}

/// Compare two `memory.events`-style counter maps key by key.
/// Missing keys, decreases, and lifetime problems produce per-key
/// unsupported outcomes; supported keys produce deltas.
pub fn compare_event_maps(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
    comparability: Comparability,
) -> BTreeMap<String, FieldComparison> {
    let mut out = BTreeMap::new();
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let (Some(b), Some(a)) = (before.get(key), after.get(key)) else {
            out.insert(
                key.clone(),
                FieldComparison::unsupported("counter", reason::MISSING_KEY),
            );
            continue;
        };
        let (Some(b), Some(a)) = (b.as_u64(), a.as_u64()) else {
            out.insert(
                key.clone(),
                FieldComparison::unsupported("counter", reason::PARTIAL),
            );
            continue;
        };
        match counter_delta(b, a, comparability) {
            Ok(delta) => {
                out.insert(
                    key.clone(),
                    FieldComparison::supported("counter", Value::from(delta)),
                );
            }
            Err(reason) => {
                out.insert(key.clone(), FieldComparison::unsupported("counter", reason));
            }
        }
    }
    out
}

/// Compare `cgroup.events` as state transitions, not counter deltas.
/// Reports `populated`/`frozen` before→after explicitly.
pub fn compare_cgroup_events(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
) -> BTreeMap<String, FieldComparison> {
    let mut out = BTreeMap::new();
    for key in ["populated", "frozen"] {
        match (before.get(key), after.get(key)) {
            (Some(b), Some(a)) if b == a => {
                out.insert(
                    key.to_string(),
                    FieldComparison {
                        kind: "state".to_string(),
                        supported: true,
                        delta: Some(a.clone()),
                        reason: "unchanged".to_string(),
                    },
                );
            }
            (Some(_), Some(a)) => {
                out.insert(
                    key.to_string(),
                    FieldComparison {
                        kind: "state".to_string(),
                        supported: true,
                        delta: Some(a.clone()),
                        reason: reason::STATE_TRANSITION.to_string(),
                    },
                );
            }
            _ => {
                out.insert(
                    key.to_string(),
                    FieldComparison::unsupported("state", reason::MISSING_KEY),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: &[(&str, u64)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(*v)))
            .collect()
    }

    #[test]
    fn counters_need_compatible_lifetime_and_monotonicity() {
        assert_eq!(counter_delta(10, 15, Comparability::Compatible), Ok(5));
        assert_eq!(
            counter_delta(15, 10, Comparability::Compatible),
            Err(reason::COUNTER_DECREASE)
        );
        assert_eq!(
            counter_delta(10, 15, Comparability::RestartDetected),
            Err(reason::RESTART)
        );
        assert_eq!(
            counter_delta(10, 15, Comparability::MissingIdentity),
            Err(reason::MISSING_IDENTITY)
        );
    }

    #[test]
    fn event_maps_report_per_key_reasons() {
        let before = map(&[("oom_kill", 1), ("max", 5)]);
        let after = map(&[("oom_kill", 3), ("max", 4)]);
        let out = compare_event_maps(&before, &after, Comparability::Compatible);
        assert_eq!(out["oom_kill"].delta, Some(json!(2)));
        assert!(!out["max"].supported);
        assert_eq!(out["max"].reason, reason::COUNTER_DECREASE);
        let after_missing = map(&[("oom_kill", 3)]);
        let out = compare_event_maps(&before, &after_missing, Comparability::Compatible);
        assert_eq!(out["max"].reason, reason::MISSING_KEY);
    }

    #[test]
    fn cgroup_events_are_transitions_not_deltas() {
        let before = map(&[("populated", 1), ("frozen", 0)]);
        let after = map(&[("populated", 0), ("frozen", 0)]);
        let out = compare_cgroup_events(&before, &after);
        assert_eq!(out["populated"].reason, reason::STATE_TRANSITION);
        assert_eq!(out["frozen"].reason, "unchanged");
    }

    #[test]
    fn psi_strict_rejects_overshoot_instead_of_clamping() {
        assert!((psi_stall_fraction_strict(100, 200, Some(1000)).unwrap() - 0.1).abs() < 1e-9);
        assert_eq!(
            psi_stall_fraction_strict(100, 1200, Some(1000)),
            Err(reason::INCOMPATIBLE_CLOCKS)
        );
        // The lenient helper clamps; the strict one reports.
        assert_eq!(psi_stall_fraction(100, 1200, Some(1000)), Ok(1.0));
    }

    #[test]
    fn psi_needs_valid_elapsed_time() {
        assert!((psi_stall_fraction(100, 200, Some(1000)).unwrap() - 0.1).abs() < 1e-9);
        assert_eq!(
            psi_stall_fraction(200, 100, Some(1000)),
            Err(reason::COUNTER_DECREASE)
        );
        assert_eq!(
            psi_stall_fraction(100, 200, None),
            Err(reason::INCOMPATIBLE_CLOCKS)
        );
        assert_eq!(
            psi_stall_fraction(100, 200, Some(0)),
            Err(reason::INCOMPATIBLE_CLOCKS)
        );
    }

    #[test]
    fn peaks_are_never_differenced_here() {
        assert_eq!(metric_kind("memory.peak"), MetricKind::Peak);
        assert_eq!(metric_kind("memory.events"), MetricKind::Counter);
        assert_eq!(metric_kind("cgroup.events"), MetricKind::State);
        assert_eq!(metric_kind("memory.pressure"), MetricKind::Pressure);
        assert_eq!(metric_kind("memory.current"), MetricKind::Gauge);
    }
}
