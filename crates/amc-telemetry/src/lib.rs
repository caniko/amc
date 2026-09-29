//! Shared bounded cgroup observation core.
//!
//! Used by CLI inspection, the finite Rust observer, and applicable
//! `amc-runner` providers. Independent of systemd transport, Tokio,
//! subscribers, and exporters. No `unsafe`. No subprocesses.
//!
//! JSON compatibility: [`Snapshot`] keeps the legacy `path`,
//! `observedUnixMs`, and `files.{value,unknown}` shape and adds only
//! additive fields (`schemaVersion`, `observedMonotonicMs`, `sequence`).

#![forbid(unsafe_code)]

pub mod compare;
pub mod host;
pub mod identity;
pub mod measure;

pub use compare::{
    FieldComparison, MetricKind, compare_cgroup_events, compare_event_maps, counter_delta,
    metric_kind, psi_stall_fraction, psi_stall_fraction_strict,
};
pub use identity::{Comparability, ObservationSession, SourceIdentity, comparability};
pub use measure::{
    FILES, MAX_FILE_BYTES, Measurement, PinnedReader, Snapshot, parse, read_measurement, reason,
};

/// Schema version for all emitted JSON. Bump only with an explicit
/// versioned migration and golden compatibility tests.
pub const SCHEMA_VERSION: u32 = 1;

/// Clock domain label for monotonic timestamps.
pub const CLOCK_DOMAIN: &str = "monotonic-clock";

use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_unix_ms() -> Option<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|v| v.as_millis())
}

/// Snapshot one cgroup directory with optional sequence/monotonic stamps.
/// `sequence` numbers samples within one observation session; `monotonic_ms`
/// must come from a single steady clock held by the caller.
pub fn snapshot(
    directory: &Path,
    path: &str,
    sequence: Option<u64>,
    monotonic_ms: Option<u64>,
) -> Snapshot {
    Snapshot {
        schema_version: SCHEMA_VERSION,
        path: path.to_string(),
        observed_unix_ms: now_unix_ms(),
        observed_monotonic_ms: monotonic_ms,
        sequence,
        // Continuity evidence is attached by the caller (watch/inspect),
        // which owns the verified identity; raw directory reads carry none.
        invocation_id: None,
        boot_id: None,
        inode: None,
        files: FILES
            .iter()
            .map(|name| (name.to_string(), read_measurement(directory, name)))
            .collect(),
    }
}

/// Legacy-shape snapshot without sequence/monotonic stamps. Preserves the
/// exact pre-upgrade JSON shape apart from the additive `schemaVersion`.
pub fn snapshot_legacy(directory: &Path, path: &str) -> Snapshot {
    snapshot(directory, path, None, None)
}

/// Read boot ID best-effort (bounded, optional).
pub fn read_boot_id() -> Option<String> {
    SourceIdentity::read_boot_id()
}

/// Collect a minimal source identity for a cgroup path. Caller supplies
/// verified unit/manager/invocation context; core only reads the inode.
pub fn source_identity(
    directory: &Path,
    cgroup_path: &str,
    unit: Option<&str>,
    manager_context: Option<&str>,
    uid: Option<u32>,
    invocation_id: Option<&str>,
) -> SourceIdentity {
    SourceIdentity::new(
        cgroup_path,
        unit,
        manager_context,
        uid,
        read_boot_id().as_deref(),
        invocation_id,
        SourceIdentity::inode_of(directory),
    )
}

/// Coverage record for an observation interval. A last-readable delta must
/// never masquerade as complete lifetime accounting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub attached_late: bool,
    pub valid_baseline: bool,
    pub termination_observed: bool,
    pub final_counters_unavailable: bool,
    pub incomplete_persistence: bool,
}

impl Coverage {
    pub fn complete(&self) -> bool {
        !self.attached_late
            && self.valid_baseline
            && !self.final_counters_unavailable
            && !self.incomplete_persistence
    }
}

/// Compare two snapshots field-by-field using the comparison engine.
/// Returns per-file, per-key outcomes with explicit unsupported reasons.
/// Gauges report sampled before/after (never budget deltas); peaks report
/// both marks with `peak-not-delta`.
pub fn compare_snapshots(
    before: &Snapshot,
    after: &Snapshot,
    before_identity: Option<&SourceIdentity>,
    after_identity: Option<&SourceIdentity>,
    before_alive: bool,
    after_alive: bool,
) -> BTreeMap<String, BTreeMap<String, FieldComparison>> {
    let comp = comparability(before_identity, after_identity, before_alive, after_alive);
    let mut out: BTreeMap<String, BTreeMap<String, FieldComparison>> = BTreeMap::new();
    for name in FILES {
        let kind = metric_kind(name);
        let b = before.files.get(*name);
        let a = after.files.get(*name);
        // A file absent from either side is an explicit outcome, never a
        // silent omission: consumers must not read absence as "no change".
        let (Some(b), Some(a)) = (b, a) else {
            let mut m = BTreeMap::new();
            m.insert(
                "missing".to_string(),
                FieldComparison::unsupported("unknown", compare::reason::MISSING_KEY),
            );
            out.insert(name.to_string(), m);
            continue;
        };
        let (Some(bv), Some(av)) = (b.value.as_ref(), a.value.as_ref()) else {
            let mut m = BTreeMap::new();
            m.insert(
                "unavailable".to_string(),
                FieldComparison::unsupported(
                    "unknown",
                    if comp == Comparability::Compatible {
                        compare::reason::PARTIAL
                    } else {
                        comp.as_reason()
                    },
                ),
            );
            out.insert(name.to_string(), m);
            continue;
        };
        match kind {
            MetricKind::Counter => {
                if let (Some(bm), Some(am)) = (bv.as_object(), av.as_object()) {
                    out.insert(name.to_string(), compare_event_maps(bm, am, comp));
                } else {
                    // A counter value with the wrong shape is partial
                    // evidence, never a skipped field.
                    let mut m = BTreeMap::new();
                    m.insert(
                        "malformed".to_string(),
                        FieldComparison::unsupported("counter", compare::reason::PARTIAL),
                    );
                    out.insert(name.to_string(), m);
                }
            }
            MetricKind::State => {
                if let (Some(bm), Some(am)) = (bv.as_object(), av.as_object()) {
                    out.insert(name.to_string(), compare_cgroup_events(bm, am));
                } else {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "malformed".to_string(),
                        FieldComparison::unsupported("state", compare::reason::PARTIAL),
                    );
                    out.insert(name.to_string(), m);
                }
            }
            MetricKind::Pressure => {
                // Retain reported averages verbatim; interval stall fractions
                // need elapsed monotonic time supplied separately via
                // `psi_stall_fraction`. Here we mark comparability only.
                // A missing `some`/`full` row on either side is partial
                // evidence, never a supported outcome with an empty delta.
                let mut m = BTreeMap::new();
                for row in ["some", "full"] {
                    let shaped = bv.get(row).is_some() && av.get(row).is_some();
                    m.insert(
                        row.to_string(),
                        FieldComparison {
                            kind: "pressure".to_string(),
                            supported: comp == Comparability::Compatible && shaped,
                            delta: if comp == Comparability::Compatible && shaped {
                                av.get(row).cloned()
                            } else {
                                None
                            },
                            reason: if !shaped {
                                compare::reason::PARTIAL.to_string()
                            } else if comp == Comparability::Compatible {
                                compare::reason::COMPATIBLE.to_string()
                            } else {
                                comp.as_reason().to_string()
                            },
                        },
                    );
                }
                out.insert(name.to_string(), m);
            }
            MetricKind::Peak => {
                let mut m = BTreeMap::new();
                m.insert(
                    "kernelMark".to_string(),
                    FieldComparison {
                        kind: "peak".to_string(),
                        supported: true,
                        delta: Some(serde_json::json!({"before": bv, "after": av})),
                        reason: compare::reason::PEAK_NOT_DELTA.to_string(),
                    },
                );
                out.insert(name.to_string(), m);
            }
            MetricKind::Gauge => {
                let mut m = BTreeMap::new();
                m.insert(
                    "sampled".to_string(),
                    FieldComparison {
                        kind: "gauge".to_string(),
                        supported: comp == Comparability::Compatible,
                        delta: if comp == Comparability::Compatible {
                            Some(serde_json::json!({"before": bv, "after": av}))
                        } else {
                            None
                        },
                        reason: if comp == Comparability::Compatible {
                            compare::reason::GAUGE_SAMPLED.to_string()
                        } else {
                            comp.as_reason().to_string()
                        },
                    },
                );
                out.insert(name.to_string(), m);
            }
        }
    }
    out
}
