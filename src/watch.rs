//! Finite read-only observer for one explicitly selected unit.
//!
//! `amc watch UNIT --seconds N --interval-ms M --output DIR` resolves the
//! unit once via the manager, then polls cgroup files directly without
//! invoking systemctl per tick. Never starts, stops, or reconfigures the
//! target. Observer cleanup never touches the target.
//!
//! Output (private, bounded):
//! - `samples.jsonl`: streaming per-tick snapshots with sequence and both
//!   clocks.
//! - `summary.json`: versioned terminal summary written atomically; only
//!   present with `complete:true` on a clean finish. A missing summary or
//!   `complete:false` is incomplete evidence, never success.
//! - `manifest.json`: binary/kernel/systemd/placement/coverage metadata.
//! - `ready`: touched only after a valid baseline (memory.events +
//!   cgroup.events readable) for coordinated fixtures.
//! - `done`: touched only after the atomic summary is persisted.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufWriter, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const DEFAULT_SECONDS: u64 = 40;
pub const MAX_SECONDS: u64 = 60;
pub const DEFAULT_INTERVAL_MS: u64 = 20;
pub const MIN_INTERVAL_MS: u64 = 10;
pub const MAX_INTERVAL_MS: u64 = 1000;
pub const MAX_SAMPLES: usize = 5000;
pub const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const PRODUCTION_MAX_SAMPLES: usize = 86_400;
const PRODUCTION_MAX_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;

/// Refuse a sample when its serialized bytes and newline exceed the stream cap.
fn budget_exceeded(bytes_written: u64, line_len: usize, limit: u64) -> bool {
    line_len as u64 >= limit.saturating_sub(bytes_written)
}

#[derive(Debug, Clone)]
pub struct WatchArgs {
    pub unit: String,
    pub system: bool,
    pub production: bool,
    pub seconds: u64,
    pub interval_ms: u64,
    pub output: PathBuf,
}

impl WatchArgs {
    pub fn validate(&self) -> Result<()> {
        let max_seconds = if self.production { 86_400 } else { MAX_SECONDS };
        let (min_interval, max_interval) = if self.production {
            (1000, 60_000)
        } else {
            (MIN_INTERVAL_MS, MAX_INTERVAL_MS)
        };
        if !(1..=max_seconds).contains(&self.seconds) {
            bail!("--seconds must be 1..={max_seconds}");
        }
        if !(min_interval..=max_interval).contains(&self.interval_ms) {
            bail!("--interval-ms must be {min_interval}..={max_interval}");
        }
        if self.output.as_os_str().is_empty() {
            bail!("--output must not be empty");
        }
        Ok(())
    }
}

/// One streamed sample: the shared [`amc_telemetry::Snapshot`] envelope
/// plus session routing. Because the observation is a plain Snapshot,
/// `amc diff` accepts both bare snapshots and watch sample lines (it
/// extracts `.observation` when the top level is a sample).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    observation: amc_telemetry::Snapshot,
    observation_id: String,
    clock_domain: &'static str,
    target: TargetRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<amc_telemetry::host::HostSnapshot>,
    capture_duration_us: u64,
    schedule_lag_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetRef {
    unit: String,
    cgroup_path: String,
    invocation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LastReadable {
    unix_ms: u128,
    /// Observer elapsed mark of the tick that produced this value. Each
    /// field keeps its own capture time: a stale counter must never be
    /// divided by (or differenced across) a newer tick's timestamp.
    elapsed_ms: u64,
    /// Sample sequence number of the producing tick.
    sequence: u64,
    /// Directory inode observed on the producing tick. Prefix endpoints
    /// are built from this, not from a later lookup, so a replacement
    /// cannot retroactively validate earlier values.
    inode: Option<u64>,
    value: serde_json::Value,
}

/// Verified readable prefix: the last-readable map as of the most recent
/// successful invocation poll.
///
/// Polls verify the manager-reported invocation, so every value in here
/// predates any restart the loop later detects: a change after the poll
/// is caught by the next poll or tick check, never retroactively. When
/// the terminal state is a restart or replacement, deltas derive from
/// this snapshot — never from post-change ticks that still carry the old
/// labels. Per-value timestamps and inodes come from the entries
/// themselves (see [`LastReadable`]).
#[derive(Debug, Clone, Default)]
struct VerifiedPrefix {
    values: BTreeMap<String, LastReadable>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    observation_id: String,
    amc_version: String,
    kernel_release: String,
    systemd_version: String,
    target: TargetRef,
    observer_placement: ObserverPlacement,
    boot_id: Option<String>,
    seconds: u64,
    interval_ms: u64,
    started_unix_ms: Option<u128>,
    production: bool,
    max_samples: usize,
    max_output_bytes: u64,
    host_scope: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ObserverPlacement {
    observer_cgroup: String,
    target_cgroup: String,
    shared_ancestor: String,
    note: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    schema_version: u32,
    observation_id: String,
    reason: String,
    complete: bool,
    target: TargetRef,
    coverage: CoverageOut,
    collection: CollectionOut,
    event_deltas: Option<BTreeMap<String, serde_json::Value>>,
    /// Endpoint coverage of the derived counter interval, never lifetime accounting.
    event_delta_coverage: Option<serde_json::Value>,
    /// Why no counter delta is reported when `event_deltas` is `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_unsupported_reason: Option<String>,
    /// Interval stall fractions for `some`/`full` from PSI `total`
    /// microseconds over valid elapsed microseconds (strict: overshoot is
    /// reported via `pressure_stall_reason`, never clamped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pressure_stall: Option<BTreeMap<String, f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pressure_stall_reason: Option<String>,
    /// Endpoint coverage of the derived PSI interval, same shape as the
    /// counter coverage. Present only with `pressure_stall`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pressure_stall_coverage: Option<serde_json::Value>,
    max_observed_swap: Option<u64>,
    max_observed_current: Option<u64>,
    last_readable: BTreeMap<String, LastReadable>,
    manager_result: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverageOut {
    attached_late: bool,
    baseline_delayed: bool,
    valid_baseline: bool,
    termination_observed: bool,
    final_counters_unavailable: bool,
    incomplete_persistence: bool,
}

fn unix_ms_now() -> Option<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|v| v.as_millis())
}

fn observation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_nanos())
        .unwrap_or(0);
    format!("obs-{}-{}", std::process::id(), nanos % 1_000_000)
}

fn read_one(path: &str) -> String {
    fs::read_to_string(path)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

fn observer_cgroup() -> String {
    read_one("/proc/self/cgroup")
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or("")
        .to_string()
}

fn shared_ancestor(a: &str, b: &str) -> String {
    let mut shared: Vec<&str> = Vec::new();
    for (ca, cb) in a.split('/').zip(b.split('/')) {
        if ca == cb {
            shared.push(ca);
        } else {
            break;
        }
    }
    // Drop the trailing empty segment of absolute paths, then rejoin.
    // Two absolute paths always share at least the root.
    while shared.last() == Some(&"") && shared.len() > 1 {
        shared.pop();
    }
    let joined = shared.join("/");
    if joined.is_empty() {
        "/".to_string()
    } else {
        joined
    }
}

fn create_private_dir(output: &Path) -> Result<()> {
    // Reject symlink races and accidental overwrite: output must not exist.
    match fs::symlink_metadata(output) {
        Ok(_) => bail!(
            "--output {} already exists; refusing to overwrite",
            output.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot stat {}", output.display())),
    }
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty())
        && fs::symlink_metadata(parent).is_err()
    {
        bail!("--output parent {} does not exist", parent.display());
    }
    for parent in output
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
    {
        if fs::symlink_metadata(parent)?.file_type().is_symlink() {
            bail!("output parent must not be a symbolic link");
        }
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(output)
        .with_context(|| format!("cannot create {}", output.display()))?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionOut {
    /// Ticks sampled, including ticks where files were unreadable.
    attempted_samples: u64,
    /// Sample lines accepted by the OS buffer without I/O error.
    persisted_samples: u64,
    /// The samples file was flushed and synced before the summary.
    storage_durable: bool,
    bytes_written: u64,
    missed_intervals: u64,
    max_capture_duration_us: u64,
}

/// Skip missed ticks rather than issuing a catch-up burst after stalled I/O.
fn next_sample_deadline(previous: Instant, now: Instant, interval: Duration) -> (Instant, u64) {
    let next = previous + interval;
    if next > now {
        return (next, 0);
    }
    // Bounded sessions and a minimum 10ms interval keep this below u32::MAX.
    let skipped = (now.duration_since(next).as_nanos() / interval.as_nanos() + 1) as u32;
    (next + interval * skipped, u64::from(skipped))
}

/// What the collection loop observed, recorded at each exit site.
/// This is loop evidence only; the terminal verdict combines it with the
/// independent final endpoint query in [`terminal_state`]. Reason strings
/// are output, never decision input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopOutcome {
    Deadline,
    DisappearedOrEmpty,
    ReplacementSuspected,
    InvocationChanged,
    Cancelled,
    OutputBudgetExceeded,
    OutputWriteFailed,
    SampleBudgetExceeded,
    ObserverNotReady,
}

impl LoopOutcome {
    fn reason(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::DisappearedOrEmpty => "disappeared-or-empty",
            Self::ReplacementSuspected => "replacement-suspected",
            Self::InvocationChanged => "invocation-changed",
            Self::Cancelled => "cancelled",
            Self::OutputBudgetExceeded => "output-budget-exceeded",
            Self::OutputWriteFailed => "output-write-failed",
            Self::SampleBudgetExceeded => "sample-budget-exceeded",
            Self::ObserverNotReady => "observer-not-ready",
        }
    }
}

/// Final endpoint verdict: the loop outcome combined with the
/// independent post-loop endpoint evidence. Retained independently of
/// any readable prefix: a valid earlier prefix never erases disappearance,
/// restart, replacement, or an unconfirmable endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalState {
    Intact,
    Restarted,
    Replaced,
    Disappeared,
    EndpointUnknown,
}

impl TerminalState {
    fn as_reason(self) -> &'static str {
        match self {
            // Intact has no failure to name; callers use the loop reason.
            Self::Intact => "compatible",
            Self::Restarted => "restart-detected",
            Self::Replaced => "replacement-suspected",
            Self::Disappeared => "disappeared",
            Self::EndpointUnknown => "missing-identity",
        }
    }
}

fn terminal_state(
    loop_outcome: LoopOutcome,
    endpoint: amc_telemetry::Comparability,
) -> TerminalState {
    use amc_telemetry::Comparability;
    match loop_outcome {
        // Positive loop evidence of a different lifetime wins over any
        // endpoint verdict, including a compatible-looking one.
        LoopOutcome::InvocationChanged => TerminalState::Restarted,
        LoopOutcome::ReplacementSuspected => TerminalState::Replaced,
        LoopOutcome::DisappearedOrEmpty => TerminalState::Disappeared,
        _ => match endpoint {
            Comparability::Compatible => TerminalState::Intact,
            Comparability::RestartDetected | Comparability::ContextMismatch => {
                TerminalState::Restarted
            }
            Comparability::ReplacementSuspected | Comparability::PathMismatch => {
                TerminalState::Replaced
            }
            Comparability::Disappeared => TerminalState::Disappeared,
            Comparability::MissingIdentity => TerminalState::EndpointUnknown,
        },
    }
}

/// Prefix selection for a terminal verdict, pure for testing.
///
/// Restarts and replacements use the verified pre-change snapshot (empty
/// means no pre-change evidence); intact, disappeared, and unconfirmed
/// endpoints use the latest readable map. In particular a replacement
/// followed by disappearance keeps the verified prefix — the later
/// disappearance does not erase earlier evidence, and the terminal
/// verdict stays replacement via [`terminal_state`].
fn select_prefix<'a>(
    terminal: TerminalState,
    verified: Option<&'a BTreeMap<String, LastReadable>>,
    latest: &'a BTreeMap<String, LastReadable>,
) -> Option<&'a BTreeMap<String, LastReadable>> {
    match terminal {
        TerminalState::Restarted | TerminalState::Replaced => verified.filter(|v| !v.is_empty()),
        TerminalState::Intact | TerminalState::Disappeared | TerminalState::EndpointUnknown => {
            Some(latest)
        }
    }
}

/// One endpoint of a derived interval, with timestamp provenance.
#[derive(Debug, Clone, Copy)]
struct Mark {
    elapsed_ms: u64,
    sequence: u64,
}

/// Session identity constants for prefix endpoint construction.
/// Invocation and boot come from attach-time verification; per-field
/// inodes come from the producing ticks.
struct SessionRef<'a> {
    cgroup_path: &'a str,
    unit: &'a str,
    context: &'static str,
    invocation: Option<&'a str>,
    boot_id: Option<&'a str>,
}

impl SessionRef<'_> {
    fn endpoint(&self, inode: Option<u64>) -> amc_telemetry::SourceIdentity {
        amc_telemetry::SourceIdentity::new(
            self.cgroup_path,
            Some(self.unit),
            Some(self.context),
            None,
            self.boot_id,
            self.invocation,
            inode,
        )
    }
}

/// Derived prefix evidence with per-interval endpoints. Every reported
/// delta carries the baseline and prefix-end marks it was computed from;
/// unsupported outcomes carry the explicit reason instead.
struct IntervalSummary {
    event_deltas: Option<BTreeMap<String, serde_json::Value>>,
    delta_reason: Option<String>,
    event_start: Option<Mark>,
    event_end: Option<Mark>,
    psi: Option<BTreeMap<String, f64>>,
    psi_reason: Option<String>,
    psi_start: Option<Mark>,
    psi_end: Option<Mark>,
}

/// Pure interval summary over verified endpoints.
///
/// `prefix` already selects the right value set for the terminal state:
/// the latest map for intact/disappeared/unconfirmed endpoints, the
/// verified pre-change snapshot for restarts and replacements. Each
/// derived quantity additionally requires its own endpoint
/// comparability, so a missing or contradictory endpoint suppresses
/// that quantity without touching the others or the terminal verdict.
fn summarize_interval(
    baseline: &Baseline,
    prefix: Option<&BTreeMap<String, LastReadable>>,
    terminal: TerminalState,
    session: &SessionRef,
) -> IntervalSummary {
    use amc_telemetry::Comparability;
    let terminal_reason = terminal.as_reason().to_string();
    let mut out = IntervalSummary {
        event_deltas: None,
        delta_reason: Some(terminal_reason.clone()),
        event_start: None,
        event_end: None,
        psi: None,
        psi_reason: Some(terminal_reason),
        psi_start: None,
        psi_end: None,
    };
    // No verified values exist at all (change before the first poll):
    // the terminal verdict is the whole story.
    let Some(prefix) = prefix else {
        let reason = terminal.as_reason().to_string();
        out.delta_reason = Some(reason.clone());
        out.psi_reason = Some(reason);
        return out;
    };
    let prefix_comp = |entry: Option<&LastReadable>| -> (Comparability, Option<Mark>) {
        match entry {
            Some(last) => (
                amc_telemetry::comparability(
                    Some(&baseline.identity),
                    Some(&session.endpoint(last.inode)),
                    true,
                    true,
                ),
                Some(Mark {
                    elapsed_ms: last.elapsed_ms,
                    sequence: last.sequence,
                }),
            ),
            None => (Comparability::Disappeared, None),
        }
    };
    // Event counters: missing or unreadable final values are partial
    // evidence, never a zero delta — and a compatible lifetime with an
    // unparsable value is partial, never a bare "compatible" with no
    // delta to show for it.
    let (events_comp, events_end) = prefix_comp(prefix.get("memory.events"));
    let events = prefix
        .get("memory.events")
        .and_then(|m| m.value.as_object());
    match (events_comp, events) {
        (Comparability::Compatible, Some(obj)) => {
            let end = events_end.expect("present entry always carries a mark");
            let compared =
                amc_telemetry::compare_event_maps(&baseline.events, obj, Comparability::Compatible);
            let mut deltas = BTreeMap::new();
            let mut first_unsupported = None;
            for (key, field) in &compared {
                if field.supported
                    && let Some(value) = &field.delta
                {
                    deltas.insert(key.clone(), value.clone());
                } else if first_unsupported.is_none() {
                    first_unsupported = Some(field.reason.clone());
                }
            }
            if let Some(reason) = first_unsupported {
                out.delta_reason = Some(reason);
            } else {
                out.event_deltas = Some(deltas);
                out.delta_reason = None;
                out.event_start = Some(Mark {
                    elapsed_ms: baseline.elapsed_ms,
                    sequence: baseline.sequence,
                });
                out.event_end = Some(end);
            }
        }
        (Comparability::Disappeared, _) | (Comparability::Compatible, None) => {
            out.delta_reason = Some(amc_telemetry::compare::reason::PARTIAL.to_string());
        }
        (comp, _) => {
            out.delta_reason = Some(comp.as_reason().to_string());
        }
    }
    // PSI stall fractions over the pressure sample's own elapsed span,
    // with the same lifetime requirement as counters. A missing or
    // malformed pressure value is partial evidence however the lifetime
    // compares.
    let (psi_comp, psi_end) = prefix_comp(prefix.get("memory.pressure"));
    let totals = prefix
        .get("memory.pressure")
        .and_then(|m| m.value.as_object())
        .and_then(|o| {
            let some = o.get("some")?.get("total")?.as_u64()?;
            let full = o.get("full")?.get("total")?.as_u64()?;
            Some((some, full))
        });
    let has_pressure_baseline = baseline.pressure_totals.is_some();
    match (psi_comp, totals, has_pressure_baseline) {
        (Comparability::Compatible, Some((last_some, last_full)), true) => {
            let end = psi_end.expect("present entry always carries a mark");
            let (base_some, base_full) = baseline.pressure_totals.expect("checked above");
            let elapsed_us = end
                .elapsed_ms
                .checked_sub(baseline.elapsed_ms)
                .and_then(|ms| ms.checked_mul(1000));
            let some = amc_telemetry::psi_stall_fraction_strict(base_some, last_some, elapsed_us);
            let full = amc_telemetry::psi_stall_fraction_strict(base_full, last_full, elapsed_us);
            match (some, full) {
                (Ok(some), Ok(full)) => {
                    out.psi = Some(BTreeMap::from([
                        ("some".to_string(), some),
                        ("full".to_string(), full),
                    ]));
                    out.psi_reason = None;
                    out.psi_start = Some(Mark {
                        elapsed_ms: baseline.elapsed_ms,
                        sequence: baseline.sequence,
                    });
                    out.psi_end = Some(end);
                }
                (Err(reason), _) | (_, Err(reason)) => {
                    out.psi_reason = Some(reason.to_string());
                }
            }
        }
        (Comparability::Disappeared, _, _)
        | (Comparability::Compatible, None, _)
        | (_, _, false) => {
            out.psi_reason = Some(amc_telemetry::compare::reason::PARTIAL.to_string());
        }
        (comp, _, _) => {
            out.psi_reason = Some(comp.as_reason().to_string());
        }
    }
    out
}

/// Full baseline tick: event map, pressure totals with their own
/// capture mark, sample sequence, and the continuity evidence observed
/// on that tick.
struct Baseline {
    events: serde_json::Map<String, serde_json::Value>,
    pressure_totals: Option<(u64, u64)>,
    elapsed_ms: u64,
    sequence: u64,
    identity: amc_telemetry::SourceIdentity,
}

/// Classify one per-tick continuity check against the pinned attach
/// inode. A vanished path is disappearance — nothing here describes a
/// new object — while a different present inode is a suspected
/// replacement. Conflating the two would route a collected cgroup into
/// the verified-prefix path and erase the disappearance verdict.
fn tick_continuity(tick: Option<u64>, pinned: Option<u64>) -> Option<LoopOutcome> {
    if tick.is_none() {
        Some(LoopOutcome::DisappearedOrEmpty)
    } else if tick != pinned {
        Some(LoopOutcome::ReplacementSuspected)
    } else {
        None
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

fn final_counters_available(
    last_attempt: Option<&amc_telemetry::Snapshot>,
    end_alive: bool,
) -> bool {
    end_alive
        && last_attempt
            .and_then(|sample| sample.files.get("memory.events"))
            .is_some_and(|measurement| measurement.value.is_some())
}

/// Strict final availability: intact terminal, live endpoint, readable
/// last persisted counters, and durable persistence. A stale
/// previously-persisted value after a failed final write must not
/// acquire availability, so any persistence failure forces unavailable.
fn final_available(
    terminal: TerminalState,
    end_alive: bool,
    incomplete_persistence: bool,
    storage_durable: bool,
    last_persisted: Option<&amc_telemetry::Snapshot>,
) -> bool {
    terminal == TerminalState::Intact
        && end_alive
        && !incomplete_persistence
        && storage_durable
        && final_counters_available(last_persisted, end_alive)
}

/// Observe one unit for a finite interval. Read-only toward the target.
pub fn watch(args: &WatchArgs) -> Result<i32> {
    args.validate()?;
    let (max_samples, max_output_bytes) = if args.production {
        (PRODUCTION_MAX_SAMPLES, PRODUCTION_MAX_OUTPUT_BYTES)
    } else {
        (MAX_SAMPLES, MAX_OUTPUT_BYTES)
    };
    super::systemd::validate_unit_public(&args.unit)?;
    create_private_dir(&args.output)?;

    let observation = observation_id();
    let started_unix = unix_ms_now();
    let started_mono = Instant::now();
    let deadline = started_mono + Duration::from_secs(args.seconds);
    let systemctl = super::systemd::resolve_executable("systemctl")?;

    // Single manager resolution up front; no systemctl per tick.
    let properties = super::systemd::leaf_status_with_timeout(
        &systemctl,
        &args.unit,
        args.system,
        &["ControlGroup", "InvocationID", "Result"],
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let invocation = properties
        .get("InvocationID")
        .filter(|value| !value.is_empty() && !value.starts_with("unknown"))
        .cloned();
    let cgroup_path = properties
        .get("ControlGroup")
        .filter(|p| p.starts_with('/'))
        .cloned()
        .unwrap_or_default();
    if cgroup_path.is_empty() {
        // No cgroup identity: record an explicit incomplete summary rather
        // than fabricating samples.
        let summary = Summary {
            schema_version: 1,
            observation_id: observation.clone(),
            reason: "observer-not-ready".into(),
            complete: false,
            target: TargetRef {
                unit: args.unit.clone(),
                cgroup_path: String::new(),
                invocation_id: invocation.clone(),
            },
            coverage: CoverageOut {
                attached_late: false,
                baseline_delayed: true,
                valid_baseline: false,
                termination_observed: false,
                final_counters_unavailable: true,
                incomplete_persistence: false,
            },
            collection: CollectionOut {
                attempted_samples: 0,
                persisted_samples: 0,
                storage_durable: false,
                bytes_written: 0,
                missed_intervals: 0,
                max_capture_duration_us: 0,
            },
            event_deltas: None,
            event_delta_coverage: None,
            delta_unsupported_reason: None,
            pressure_stall: None,
            pressure_stall_reason: None,
            pressure_stall_coverage: None,
            max_observed_swap: None,
            max_observed_current: None,
            last_readable: BTreeMap::new(),
            manager_result: properties.get("Result").cloned(),
        };
        let bytes = serde_json::to_vec_pretty(&summary)?;
        atomic_write(&args.output.join("summary.json"), &bytes)?;
        return Ok(2);
    }
    let cgroup_dir = super::systemd::cgroup_directory(&cgroup_path)?;
    let boot_id = amc_telemetry::read_boot_id();
    let target = TargetRef {
        unit: args.unit.clone(),
        cgroup_path: cgroup_path.clone(),
        invocation_id: invocation.clone(),
    };
    // Pin the target directory: the held handle proves the directory we
    // resolved at attach time stays open, and per-tick inode comparison
    // detects same-path replacement underneath us.
    let mut pinned_files = amc_telemetry::PinnedReader::open(&cgroup_dir)?;
    let pinned_inode = Some(pinned_files.inode()?);
    // Cancellation must leave recognizable incomplete evidence, never a
    // fabricated success: the summary below is only `complete` on a clean
    // terminal state, and `done` is only written after it persists. Without
    // handlers a SIGKILL-style death still leaves no summary file, which is
    // itself recognizable as incomplete — but a handled cancel records an
    // explicit reason, so refuse to run unhandled.
    let signals = crate::control::Signals::install()?;

    let manifest = Manifest {
        schema_version: 1,
        observation_id: observation.clone(),
        amc_version: env!("CARGO_PKG_VERSION").to_string(),
        kernel_release: read_one("/proc/sys/kernel/osrelease"),
        systemd_version: super::systemd::manager_version(&systemctl, args.system),
        target: target.clone(),
        observer_placement: {
            let obs = observer_cgroup();
            ObserverPlacement {
                shared_ancestor: shared_ancestor(&obs, &cgroup_path),
                observer_cgroup: obs,
                target_cgroup: cgroup_path.clone(),
                note: "observer runs outside the target leaf; shared ancestors are reported, independence is not claimed from leaf difference alone",
            }
        },
        boot_id: boot_id.clone(),
        seconds: args.seconds,
        interval_ms: args.interval_ms,
        started_unix_ms: started_unix,
        production: args.production,
        max_samples,
        max_output_bytes,
        host_scope: if args.production {
            "observer-procfs-view; aggregate context, not target attribution"
        } else {
            "not-collected"
        },
    };
    atomic_write(
        &args.output.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;

    let samples_path = args.output.join("samples.jsonl");
    let samples_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&samples_path)
        .with_context(|| format!("cannot create {}", samples_path.display()))?;
    let mut samples = BufWriter::new(samples_file);

    let interval = Duration::from_millis(args.interval_ms);
    let owner_boot_id = boot_id.clone();
    let context: &'static str = if args.system { "system" } else { "user" };
    let mut sequence: u64 = 0;
    // Ticks attempted, including ticks whose samples never reached the
    // stream. `sequence` only advances on successful writes, so it
    // cannot serve as the attempt count.
    let mut attempts: u64 = 0;
    let mut baseline: Option<Baseline> = None;
    let mut verified: Option<VerifiedPrefix> = None;
    let mut last_readable: BTreeMap<String, LastReadable> = BTreeMap::new();
    let mut max_swap: Option<u64> = None;
    let mut max_current: Option<u64> = None;
    let mut reason = LoopOutcome::Deadline.reason().to_string();
    let mut outcome = LoopOutcome::Deadline;
    let mut baseline_ready = false;
    let mut first_tick_complete = false;
    let mut incomplete_persistence = false;
    let mut bytes_written: u64 = 0;
    let mut persisted_samples: u64 = 0;
    let mut wrote_ready = false;
    // Pinned file descriptions for every sample: reads stay anchored to
    // the validated object even across a check-to-read race.
    let attached = super::systemd::leaf_status_with_timeout(
        &systemctl,
        &args.unit,
        args.system,
        &["InvocationID", "ControlGroup"],
        deadline.saturating_duration_since(Instant::now()),
    )?;
    if attached.get("InvocationID") != invocation.as_ref()
        || attached.get("ControlGroup") != Some(&cgroup_path)
        || amc_telemetry::SourceIdentity::inode_of(&cgroup_dir) != pinned_inode
    {
        bail!("target identity changed or unavailable during attachment");
    }
    let mut last_attempt: Option<amc_telemetry::Snapshot> = None;
    let mut next_sample = Instant::now();
    let mut last_manager_poll = next_sample;
    let mut missed_intervals = 0;
    let mut max_capture_duration_us = 0;

    while Instant::now() < deadline && (sequence as usize) < max_samples {
        if signals.cancelled().is_some() {
            outcome = LoopOutcome::Cancelled;
            reason = outcome.reason().to_string();
            break;
        }
        attempts += 1;
        let capture_started = Instant::now();
        let schedule_lag_ms = capture_started
            .saturating_duration_since(next_sample)
            .as_millis() as u64;
        let elapsed_ms = started_mono.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        // Continuity: the path may have been recycled for a new object
        // between ticks (see `tick_continuity`).
        let tick_inode = amc_telemetry::SourceIdentity::inode_of(&cgroup_dir);
        if let Some(end) = tick_continuity(tick_inode, pinned_inode) {
            outcome = end;
            reason = outcome.reason().to_string();
            break;
        }
        let mut snapshot = pinned_files.snapshot(&cgroup_path, Some(sequence), Some(elapsed_ms));
        snapshot.invocation_id = invocation.clone();
        snapshot.boot_id = owner_boot_id.clone();
        snapshot.inode = tick_inode;
        let sample_unix_ms = snapshot.observed_unix_ms.unwrap_or(0);
        let host = args.production.then(|| {
            amc_telemetry::host::snapshot(
                Path::new("/proc"),
                Some(started_mono.elapsed().as_millis() as u64),
            )
        });
        let capture_duration_us = capture_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        max_capture_duration_us = max_capture_duration_us.max(capture_duration_us);
        let sample = Sample {
            observation: snapshot,
            observation_id: observation.clone(),
            clock_domain: amc_telemetry::CLOCK_DOMAIN,
            target: target.clone(),
            host,
            capture_duration_us,
            schedule_lag_ms,
        };
        // Track last readable per file and running maxima from known values.
        // These are sampled maxima, distinct from kernel-reported peaks.
        // Each field keeps its own capture mark: derived measurements must
        // use the timestamps of the values they derive from, never a
        // newer global tick.
        for (name, m) in &sample.observation.files {
            if let Some(v) = &m.value {
                last_readable.insert(
                    name.clone(),
                    LastReadable {
                        unix_ms: sample_unix_ms,
                        elapsed_ms,
                        sequence,
                        inode: tick_inode,
                        value: v.clone(),
                    },
                );
                if name == "memory.swap.current"
                    && let Some(n) = v.as_u64()
                {
                    max_swap = Some(max_swap.map_or(n, |m| m.max(n)));
                }
                if name == "memory.current"
                    && let Some(n) = v.as_u64()
                {
                    max_current = Some(max_current.map_or(n, |m| m.max(n)));
                }
            }
        }
        // Baseline requires both event scopes readable on the same tick.
        let mut baseline_just_ready = false;
        if !baseline_ready
            && let (Some(mem), Some(cg)) = (
                sample
                    .observation
                    .files
                    .get("memory.events")
                    .and_then(|m| m.value.as_ref()),
                sample
                    .observation
                    .files
                    .get("cgroup.events")
                    .and_then(|m| m.value.as_ref()),
            )
            && let (Some(mem), Some(_)) = (mem.as_object(), cg.as_object())
            && !mem.is_empty()
        {
            let pressure_totals = sample
                .observation
                .files
                .get("memory.pressure")
                .and_then(|m| m.value.as_ref())
                .and_then(|v| v.as_object())
                .and_then(|o| {
                    let some = o.get("some")?.get("total")?.as_u64()?;
                    let full = o.get("full")?.get("total")?.as_u64()?;
                    Some((some, full))
                });
            baseline = Some(Baseline {
                events: mem.clone(),
                pressure_totals,
                elapsed_ms,
                sequence,
                identity: amc_telemetry::SourceIdentity::new(
                    &cgroup_path,
                    Some(&args.unit),
                    Some(context),
                    None,
                    owner_boot_id.as_deref(),
                    invocation.as_deref(),
                    tick_inode,
                ),
            });
            baseline_ready = true;
            baseline_just_ready = true;
            if sequence == 0 {
                first_tick_complete = true;
            }
        }
        let line = serde_json::to_vec(&sample)?;
        if budget_exceeded(bytes_written, line.len(), max_output_bytes) {
            outcome = LoopOutcome::OutputBudgetExceeded;
            reason = outcome.reason().to_string();
            incomplete_persistence = true;
            break;
        }
        // A failed sample write is incomplete persistence, not a silent
        // truncation: record it and stop instead of fabricating the rest.
        if samples.write_all(&line).is_err()
            || samples.write_all(b"\n").is_err()
            || (args.production && samples.flush().is_err())
        {
            outcome = LoopOutcome::OutputWriteFailed;
            reason = outcome.reason().to_string();
            incomplete_persistence = true;
            break;
        }
        bytes_written += line.len() as u64 + 1;
        persisted_samples += 1;
        last_attempt = Some(sample.observation.clone());
        sequence += 1;
        if baseline_just_ready && !wrote_ready {
            // Readiness gates coordinated fixtures, so it is published
            // only after the baseline sample itself is persisted: a
            // consumer released by `ready` must find the baseline on
            // disk. A failed write is recorded, never treated as ready.
            match samples
                .flush()
                .and_then(|()| samples.get_ref().sync_all())
                .and_then(|()| {
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(args.output.join("ready"))
                })
                .and_then(|file| file.sync_all())
            {
                Ok(()) => wrote_ready = true,
                Err(_) => incomplete_persistence = true,
            }
        }

        // Termination: populated==0 or cgroup gone. Never bridge restarts:
        // invocation change ends with an explicit reason.
        let populated_gone = sample
            .observation
            .files
            .get("cgroup.events")
            .and_then(|m| m.value.as_ref())
            .and_then(|v| v.get("populated"))
            .and_then(|v| v.as_u64())
            == Some(0);
        if populated_gone {
            outcome = LoopOutcome::DisappearedOrEmpty;
            reason = outcome.reason().to_string();
            break;
        }
        if amc_telemetry::SourceIdentity::inode_of(&cgroup_dir).is_none() {
            outcome = LoopOutcome::DisappearedOrEmpty;
            reason = outcome.reason().to_string();
            break;
        }
        // Poll the manager at most once per second for invocation change,
        // using a leaf-only query (never the full ancestor traversal)
        // bounded by the remaining sampling budget.
        // Unknown manager values are normalized to None: an invocation ID
        // appearing, disappearing, or changing under us all end the
        // interval. Two unknowns carry no information (the inode check
        // above remains the guard), so polling continues.
        // A successful poll additionally snapshots the verified readable
        // prefix: every value in it predates any restart detected later.
        if last_manager_poll.elapsed() >= Duration::from_secs(1) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Ok(current) = super::systemd::leaf_status_with_timeout(
                &systemctl,
                &args.unit,
                args.system,
                &["InvocationID"],
                remaining,
            ) {
                let current_inv = current
                    .get("InvocationID")
                    .filter(|v| !v.is_empty() && !v.starts_with("unknown"))
                    .cloned();
                if current_inv != invocation && (current_inv.is_some() || invocation.is_some()) {
                    outcome = LoopOutcome::InvocationChanged;
                    reason = outcome.reason().to_string();
                    break;
                }
                verified = Some(VerifiedPrefix {
                    values: last_readable.clone(),
                });
            }
            last_manager_poll = Instant::now();
        }
        // Count only scheduled slots inside the requested window, even if
        // the last read/query stalled past its end.
        let scheduling_end = Instant::now().min(deadline - Duration::from_nanos(1));
        let (next, skipped) = next_sample_deadline(next_sample, scheduling_end, interval);
        next_sample = next;
        missed_intervals += skipped;
        let wake = next_sample.min(deadline);
        while signals.cancelled().is_none() {
            let remaining = wake.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
    if (sequence as usize) >= max_samples
        && outcome == LoopOutcome::Deadline
        && Instant::now() < deadline
    {
        // The sample budget — not the clock — ended the interval. The
        // observed prefix stays usable, but it is truncated evidence.
        outcome = LoopOutcome::SampleBudgetExceeded;
        reason = outcome.reason().to_string();
    }
    // Durability: the samples file is flushed and synced before the
    // summary may claim persistence. Unsynced acceptance is buffered,
    // not durable.
    let storage_durable = samples
        .flush()
        .and_then(|()| samples.get_ref().sync_all())
        .is_ok();
    if !storage_durable {
        incomplete_persistence = true;
        if outcome == LoopOutcome::Deadline {
            outcome = LoopOutcome::OutputWriteFailed;
            reason = outcome.reason().to_string();
        }
    }
    drop(samples);

    if !baseline_ready {
        outcome = LoopOutcome::ObserverNotReady;
        reason = outcome.reason().to_string();
    }
    // Final continuity state from an independent post-loop query — never
    // from attach-time values — so a restart in the final gap cannot
    // silently reuse the original invocation ID and produce deltas across
    // two lifetimes. The final query gets a fixed grace budget outside the
    // sampling window; sampling itself was bounded by `deadline` above.
    let end_inode = amc_telemetry::SourceIdentity::inode_of(&cgroup_dir);
    let end_alive = end_inode.is_some();
    let final_leaf = super::systemd::leaf_status_with_timeout(
        &systemctl,
        &args.unit,
        args.system,
        &["Result", "InvocationID"],
        crate::control::QUERY_TIMEOUT,
    )
    .ok();
    let end_invocation = final_leaf
        .as_ref()
        .and_then(|m| m.get("InvocationID"))
        .filter(|v| !v.is_empty() && !v.starts_with("unknown"))
        .cloned();
    let end_identity = amc_telemetry::SourceIdentity::new(
        &cgroup_path,
        Some(&args.unit),
        Some(context),
        None,
        owner_boot_id.as_deref(),
        end_invocation.as_deref(),
        end_inode,
    );
    let endpoint_identity = final_leaf.is_some().then_some(end_identity);
    // The leaf result describes the interval outcome; the attach-time
    // value is gone (no fallback): a failed final query stays unknown
    // rather than borrowing an older answer.
    let final_manager_result = final_leaf
        .as_ref()
        .and_then(|m| m.get("Result").cloned())
        .filter(|v| !v.starts_with("unknown"));
    // Terminal verdict from loop evidence plus the endpoint verdict —
    // never from reason strings. A valid earlier prefix never erases the
    // terminal state recorded here.
    let endpoint_comp = match (&baseline, &endpoint_identity) {
        (Some(base), Some(end)) => {
            amc_telemetry::comparability(Some(&base.identity), Some(end), true, end_alive)
        }
        _ => amc_telemetry::Comparability::MissingIdentity,
    };
    let terminal = terminal_state(outcome, endpoint_comp);
    let session = SessionRef {
        cgroup_path: &cgroup_path,
        unit: &args.unit,
        context,
        invocation: invocation.as_deref(),
        boot_id: owner_boot_id.as_deref(),
    };
    // No baseline means no interval at all; an empty verified snapshot
    // means no pre-change evidence. Both report the terminal verdict.
    // A non-empty verified snapshot is pre-change by poll ordering (see
    // the poll site); post-change ticks still carry old labels.
    let prefix = select_prefix(
        terminal,
        verified.as_ref().map(|v| &v.values),
        &last_readable,
    );
    let interval = match &baseline {
        Some(base) => summarize_interval(base, prefix, terminal, &session),
        None => IntervalSummary {
            event_deltas: None,
            delta_reason: Some(terminal.as_reason().to_string()),
            event_start: None,
            event_end: None,
            psi: None,
            psi_reason: Some(terminal.as_reason().to_string()),
            psi_start: None,
            psi_end: None,
        },
    };
    // A lifetime change the loop never saw (restart in the final gap)
    // rewrites a clean `deadline` reason: the interval is not intact. An
    // unconfirmable endpoint gets its own reason instead of borrowing
    // `deadline`: collection finished, confirmation did not.
    if outcome == LoopOutcome::Deadline {
        reason = match terminal {
            TerminalState::Intact => LoopOutcome::Deadline.reason().to_string(),
            TerminalState::Restarted => LoopOutcome::InvocationChanged.reason().to_string(),
            TerminalState::Replaced => LoopOutcome::ReplacementSuspected.reason().to_string(),
            TerminalState::Disappeared => LoopOutcome::DisappearedOrEmpty.reason().to_string(),
            TerminalState::EndpointUnknown => "endpoint-query-failed".to_string(),
        };
    }
    // Final counters are available only on the strict path: an intact
    // terminal, a live endpoint, counters readable on the last persisted
    // attempt, and durable persistence. Anything else — disappearance,
    // restart, unconfirmed endpoint, unreadable final, or any persistence
    // failure (a stale previously-persisted value must not acquire final
    // availability) — is unavailable, even next to a valid readable prefix.
    let final_unavailable = !final_available(
        terminal,
        end_alive,
        incomplete_persistence,
        storage_durable,
        last_attempt.as_ref(),
    );
    // Completion is earned, not defaulted. A clean terminal state with a
    // valid baseline and persisted summary is complete; everything else —
    // missing baseline, lifetime change, budget exhaustion, cancellation,
    // persistence failure — is incomplete evidence.
    let complete = baseline_ready
        && !incomplete_persistence
        && matches!(reason.as_str(), "deadline" | "disappeared-or-empty");
    let mark = |mark: Option<Mark>| {
        mark.map(|m| {
            serde_json::json!({
                "elapsedMs": m.elapsed_ms,
                "sequence": m.sequence,
            })
        })
    };

    let mut summary = Summary {
        schema_version: 1,
        observation_id: observation,
        reason,
        complete,
        target,
        coverage: CoverageOut {
            // Passive attachment cannot certify counters from workload
            // start. Note: interval deltas always cover baseline-to-end,
            // never the full workload lifetime; `attachedLate` marks that,
            // while `baselineDelayed` marks a truncated interval start.
            attached_late: true,
            baseline_delayed: !first_tick_complete,
            valid_baseline: baseline_ready,
            termination_observed: !end_alive
                || last_readable
                    .get("cgroup.events")
                    .and_then(|l| l.value.get("populated"))
                    .and_then(|v| v.as_u64())
                    == Some(0),
            final_counters_unavailable: final_unavailable,
            incomplete_persistence,
        },
        collection: CollectionOut {
            attempted_samples: attempts,
            persisted_samples,
            storage_durable,
            bytes_written,
            missed_intervals,
            max_capture_duration_us,
        },
        event_delta_coverage: interval.event_deltas.as_ref().map(|_| {
            serde_json::json!({
                "kind": "baseline-to-last-readable",
                "start": mark(interval.event_start),
                "end": mark(interval.event_end),
                "startElapsedMs": interval.event_start.map(|m| m.elapsed_ms),
                "endElapsedMs": interval.event_end.map(|m| m.elapsed_ms),
                "startSequence": interval.event_start.map(|m| m.sequence),
                "endSequence": interval.event_end.map(|m| m.sequence),
                "finalCountersAvailable": !final_unavailable,
                "lifetimeComplete": false
            })
        }),
        event_deltas: interval.event_deltas,
        delta_unsupported_reason: interval.delta_reason,
        pressure_stall: interval.psi.clone(),
        pressure_stall_reason: interval.psi_reason,
        pressure_stall_coverage: interval.psi.as_ref().map(|_| {
            serde_json::json!({
                "kind": "baseline-to-last-readable",
                "start": mark(interval.psi_start),
                "end": mark(interval.psi_end),
                "startElapsedMs": interval.psi_start.map(|m| m.elapsed_ms),
                "endElapsedMs": interval.psi_end.map(|m| m.elapsed_ms),
                "startSequence": interval.psi_start.map(|m| m.sequence),
                "endSequence": interval.psi_end.map(|m| m.sequence),
                "finalCountersAvailable": !final_unavailable,
                "lifetimeComplete": false
            })
        }),
        max_observed_swap: max_swap,
        max_observed_current: max_current,
        last_readable,
        manager_result: final_manager_result,
    };
    // Primary error preserved: if the summary itself cannot persist,
    // nothing is claimed.
    atomic_write(
        &args.output.join("summary.json"),
        &serde_json::to_vec_pretty(&summary)?,
    )?;
    // The completion marker must not imply persistence that failed: on a
    // `done` write failure the summary is repaired to incomplete and the
    // original error is returned.
    if let Err(error) = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(args.output.join("done"))
        .and_then(|file| file.sync_all())
    {
        summary.coverage.incomplete_persistence = true;
        summary.complete = false;
        let _ = atomic_write(
            &args.output.join("summary.json"),
            &serde_json::to_vec_pretty(&summary)?,
        );
        return Err(error).with_context(|| format!("cannot write {}", args.output.display()));
    }
    Ok(0)
}

/// Maximum bytes read per diff input. Enforced while reading from the
/// opened handle (not via metadata size, which special files and races
/// defeat): one probe byte past the budget rejects without allocating
/// for unbounded inputs.
const DIFF_INPUT_LIMIT: u64 = 4 * 1024 * 1024;

fn read_bounded_input(path: &Path) -> Result<String> {
    let file = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = Vec::new();
    file.take(DIFF_INPUT_LIMIT + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if buf.len() as u64 > DIFF_INPUT_LIMIT {
        bail!(
            "diff input {} exceeds {}-byte budget",
            path.display(),
            DIFF_INPUT_LIMIT
        );
    }
    String::from_utf8(buf).map_err(|_| anyhow::anyhow!("diff input is not valid UTF-8"))
}

/// Load one observation: a bare [`amc_telemetry::Snapshot`], or a watch
/// sample line carrying the snapshot under `observation`. One envelope,
/// so watch output feeds `diff` directly.
fn load_observation(text: &str) -> Result<amc_telemetry::Snapshot> {
    let mut value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| anyhow::anyhow!("invalid-observation-json"))?;
    let observation = value
        .get_mut("observation")
        .map(serde_json::Value::take)
        .unwrap_or(value);
    let snapshot: amc_telemetry::Snapshot = serde_json::from_value(observation)
        .map_err(|_| anyhow::anyhow!("invalid-observation-shape"))?;
    snapshot.validate().map_err(anyhow::Error::msg)?;
    Ok(snapshot)
}

/// Offline comparison of two observation JSON files using the shared
/// engine. Identity comes only from fields carried by the files
/// themselves (`invocationId`/`bootId`/`inode`); files without them
/// compare as `missing-identity`, never guessed from matching paths.
pub fn diff(before_path: &Path, after_path: &Path) -> Result<i32> {
    let before_text = read_bounded_input(before_path)?;
    let after_text = read_bounded_input(after_path)?;
    let before = load_observation(&before_text)?;
    let after = load_observation(&after_text)?;
    let before_id = before.source_identity();
    let after_id = after.source_identity();
    let out = amc_telemetry::compare_snapshots(
        &before,
        &after,
        before_id.as_ref(),
        after_id.as_ref(),
        true,
        true,
    );
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_coverage_does_not_reuse_a_readable_prefix() {
        let mut last: amc_telemetry::Snapshot = serde_json::from_value(serde_json::json!({
            "path": "/test", "observedUnixMs": 1,
            "files": {"memory.events": {"value": {"oom_kill": 3}, "unknown": null}}
        }))
        .unwrap();
        assert!(final_counters_available(Some(&last), true));
        assert!(!final_counters_available(Some(&last), false));
        last.files.insert(
            "memory.events".into(),
            amc_telemetry::Measurement::unknown("unreadable"),
        );
        assert!(!final_counters_available(Some(&last), true));
        assert!(!final_counters_available(None, true));
    }

    #[test]
    fn watch_args_are_finitely_bounded() {
        let ok = WatchArgs {
            unit: "app-amc-test@1.service".into(),
            system: false,
            production: false,
            seconds: 5,
            interval_ms: 20,
            output: PathBuf::from("/tmp/amc-watch-test"),
        };
        assert!(ok.validate().is_ok());
        for bad_seconds in [0, 61] {
            let args = WatchArgs {
                seconds: bad_seconds,
                ..ok.clone()
            };
            assert!(args.validate().is_err());
        }
        for bad_interval in [0, 9, 1001] {
            let args = WatchArgs {
                interval_ms: bad_interval,
                ..ok.clone()
            };
            assert!(args.validate().is_err());
        }
        let production = WatchArgs {
            production: true,
            seconds: 86_400,
            interval_ms: 1000,
            ..ok
        };
        assert!(production.validate().is_ok());
        for (seconds, interval_ms) in [(0, 1000), (86_401, 1000), (3600, 999), (3600, 60_001)] {
            assert!(
                WatchArgs {
                    seconds,
                    interval_ms,
                    ..production.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            WatchArgs {
                interval_ms: 60_000,
                ..production
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn stalled_collection_skips_slots_instead_of_catching_up() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        assert_eq!(
            next_sample_deadline(start, start, interval),
            (start + interval, 0)
        );
        assert_eq!(
            next_sample_deadline(start, start + interval, interval),
            (start + interval * 2, 1)
        );
        assert_eq!(
            next_sample_deadline(start, start + Duration::from_millis(3500), interval),
            (start + interval * 4, 3)
        );
    }

    #[test]
    fn shared_ancestor_compares_segments() {
        assert_eq!(shared_ancestor("/x/y", "/x/z"), "/x");
        assert_eq!(shared_ancestor("/x", "/x"), "/x");
        assert_eq!(shared_ancestor("/a", "/b"), "/");
        assert_eq!(shared_ancestor("/", "/x"), "/");
        // Prefix overlap without a segment boundary is not an ancestor.
        assert_eq!(shared_ancestor("/xy", "/x"), "/");
    }

    #[test]
    fn output_refuses_overwrite_and_missing_parent() {
        let dir = std::env::temp_dir().join(format!(
            "amc-watch-out-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Missing parent is rejected, not created.
        let nested = dir.join("no-such-parent").join("out");
        assert!(create_private_dir(&nested).is_err());
        // Existing output is never overwritten.
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("exists");
        std::fs::create_dir(&existing).unwrap();
        assert!(create_private_dir(&existing).is_err());
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(&existing, &alias).unwrap();
        assert!(create_private_dir(&alias.join("out")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_rejects_oversized_inputs() {
        let dir = std::env::temp_dir().join(format!(
            "amc-diff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let before = dir.join("before.json");
        let after = dir.join("after.json");
        std::fs::write(&before, "{}").unwrap();
        std::fs::write(&after, "{}").unwrap();
        // `{}` is not a Snapshot: serde error, not a fabricated diff.
        assert!(diff(&before, &after).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_reports_counter_deltas_and_state_transitions() {
        let dir = std::env::temp_dir().join(format!(
            "amc-diff-ok-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let mk = |events: &str, populated: u64| {
            serde_json::json!({
                "schemaVersion": 1,
                "path": "/sys/fs/cgroup/a.service",
                "observedUnixMs": 1,
                "invocationId": "inv-1",
                "bootId": "boot-1",
                "inode": 42,
                "files": {
                    "memory.events": {"value": serde_json::from_str::<serde_json::Value>(events).unwrap(), "unknown": null},
                    "memory.events.local": {"value": {}, "unknown": "not-collected"},
                    "cgroup.events": {"value": {"populated": populated, "frozen": 0}, "unknown": null},
                    "memory.current": {"value": 100, "unknown": null},
                    "memory.peak": {"value": 200, "unknown": null},
                    "memory.pressure": {"value": null, "unknown": "not-collected"},
                    "memory.swap.current": {"value": null, "unknown": "not-collected"},
                    "memory.swap.peak": {"value": null, "unknown": "not-collected"},
                    "memory.min": {"value": null, "unknown": "not-collected"},
                    "memory.low": {"value": null, "unknown": "not-collected"},
                    "memory.high": {"value": null, "unknown": "not-collected"},
                    "memory.max": {"value": null, "unknown": "not-collected"},
                    "memory.swap.max": {"value": null, "unknown": "not-collected"},
                    "memory.oom.group": {"value": null, "unknown": "not-collected"}
                }
            })
        };
        let before = dir.join("before.json");
        let after = dir.join("after.json");
        std::fs::write(
            &before,
            serde_json::to_string(&mk(r#"{"oom_kill": 1}"#, 1)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &after,
            serde_json::to_string(&mk(r#"{"oom_kill": 3}"#, 0)).unwrap(),
        )
        .unwrap();
        let b: amc_telemetry::Snapshot =
            serde_json::from_str(&std::fs::read_to_string(&before).unwrap()).unwrap();
        let a: amc_telemetry::Snapshot =
            serde_json::from_str(&std::fs::read_to_string(&after).unwrap()).unwrap();
        // Identity comes from the files themselves, exactly as `diff` does.
        let b_id = b.source_identity();
        let a_id = a.source_identity();
        assert!(b_id.is_some() && a_id.is_some());
        let out =
            amc_telemetry::compare_snapshots(&b, &a, b_id.as_ref(), a_id.as_ref(), true, true);
        assert_eq!(
            out["memory.events"]["oom_kill"].delta,
            Some(serde_json::json!(2))
        );
        assert_eq!(out["cgroup.events"]["populated"].reason, "state-transition");
        // Peaks are never differenced.
        assert_eq!(out["memory.peak"]["kernelMark"].reason, "peak-not-delta");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vanished_path_is_disappearance_not_replacement() {
        // The VM OOM case: systemd collects the failed unit's cgroup, so
        // the tick lookup returns nothing. That must end the interval as
        // disappearance (preserving the OOM prefix), never as a suspected
        // replacement (which would demand verified pre-change evidence
        // for counters that postdate the last poll).
        assert_eq!(
            tick_continuity(None, Some(5533)),
            Some(LoopOutcome::DisappearedOrEmpty)
        );
        assert_eq!(
            tick_continuity(Some(9999), Some(5533)),
            Some(LoopOutcome::ReplacementSuspected)
        );
        assert_eq!(tick_continuity(Some(5533), Some(5533)), None);
    }

    #[test]
    fn terminal_state_combines_loop_evidence_with_endpoint_verdict() {
        use amc_telemetry::Comparability::*;
        // Loop lifetime evidence wins over any endpoint verdict.
        assert_eq!(
            terminal_state(LoopOutcome::InvocationChanged, Compatible),
            TerminalState::Restarted
        );
        assert_eq!(
            terminal_state(LoopOutcome::ReplacementSuspected, Compatible),
            TerminalState::Replaced
        );
        assert_eq!(
            terminal_state(LoopOutcome::DisappearedOrEmpty, Compatible),
            TerminalState::Disappeared
        );
        // Clean loop defers to the endpoint verdict, including final-gap
        // lifetime changes and unconfirmable endpoints.
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, Compatible),
            TerminalState::Intact
        );
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, RestartDetected),
            TerminalState::Restarted
        );
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, ReplacementSuspected),
            TerminalState::Replaced
        );
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, PathMismatch),
            TerminalState::Replaced
        );
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, ContextMismatch),
            TerminalState::Restarted
        );
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, Disappeared),
            TerminalState::Disappeared
        );
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, MissingIdentity),
            TerminalState::EndpointUnknown
        );
        // Non-lifetime loop exits still combine with the endpoint: a
        // restart in the final gap is not erased by a clean-looking loop.
        assert_eq!(
            terminal_state(LoopOutcome::Cancelled, RestartDetected),
            TerminalState::Restarted
        );
        assert_eq!(
            terminal_state(LoopOutcome::Cancelled, Compatible),
            TerminalState::Intact
        );
    }

    struct SummaryEnv {
        unit: String,
        path: String,
        invocation: String,
        boot: String,
    }

    impl SummaryEnv {
        fn new() -> Self {
            Self {
                unit: "a.service".to_string(),
                path: "/sys/fs/cgroup/a.service".to_string(),
                invocation: "inv-1".to_string(),
                boot: "boot-1".to_string(),
            }
        }

        fn session(&self) -> SessionRef<'_> {
            SessionRef {
                cgroup_path: &self.path,
                unit: &self.unit,
                context: "user",
                invocation: Some(&self.invocation)
                    .filter(|s| !s.is_empty())
                    .map(|s| &**s),
                boot_id: Some(&self.boot),
            }
        }

        fn baseline(
            &self,
            events: serde_json::Value,
            pressure: Option<(u64, u64)>,
            elapsed: u64,
            sequence: u64,
            inode: u64,
        ) -> Baseline {
            Baseline {
                events: events.as_object().unwrap().clone(),
                pressure_totals: pressure,
                elapsed_ms: elapsed,
                sequence,
                identity: amc_telemetry::SourceIdentity::new(
                    &self.path,
                    Some(&self.unit),
                    Some("user"),
                    None,
                    Some(&self.boot),
                    Some(&self.invocation)
                        .filter(|s| !s.is_empty())
                        .map(|s| &**s),
                    Some(inode),
                ),
            }
        }

        fn readable(
            &self,
            value: serde_json::Value,
            elapsed: u64,
            sequence: u64,
            inode: u64,
        ) -> LastReadable {
            LastReadable {
                unix_ms: 1,
                elapsed_ms: elapsed,
                sequence,
                inode: Some(inode),
                value,
            }
        }

        fn values(
            &self,
            events: serde_json::Value,
            pressure: Option<serde_json::Value>,
            elapsed: u64,
            sequence: u64,
            inode: u64,
        ) -> BTreeMap<String, LastReadable> {
            let mut map = BTreeMap::new();
            map.insert(
                "memory.events".to_string(),
                self.readable(events, elapsed, sequence, inode),
            );
            if let Some(pressure) = pressure {
                map.insert(
                    "memory.pressure".to_string(),
                    self.readable(pressure, elapsed, sequence, inode),
                );
            }
            map
        }

        fn pressure_totals(some: u64, full: u64) -> serde_json::Value {
            serde_json::json!({
                "some": {"avg10": 0.0, "avg60": 0.0, "avg300": 0.0, "total": some},
                "full": {"avg10": 0.0, "avg60": 0.0, "avg300": 0.0, "total": full},
            })
        }
    }

    #[test]
    fn intact_prefix_reports_deltas_with_endpoint_marks() {
        let env = SummaryEnv::new();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 1, "max": 4}),
            Some((1000, 2000)),
            100,
            5,
            42,
        );
        let values = env.values(
            serde_json::json!({"oom_kill": 3, "max": 4}),
            Some(SummaryEnv::pressure_totals(1100, 2050)),
            900,
            45,
            42,
        );
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(
            out.event_deltas,
            Some(BTreeMap::from([
                ("oom_kill".to_string(), serde_json::json!(2)),
                ("max".to_string(), serde_json::json!(0))
            ]))
        );
        assert_eq!(out.delta_reason, None);
        assert!(matches!(
            out.event_start,
            Some(Mark {
                elapsed_ms: 100,
                sequence: 5
            })
        ));
        assert!(matches!(
            out.event_end,
            Some(Mark {
                elapsed_ms: 900,
                sequence: 45
            })
        ));
        let psi = out.psi.expect("stall fractions");
        assert!((psi["some"] - 0.000125).abs() < 1e-12);
        assert!((psi["full"] - 0.0000625).abs() < 1e-12);
        assert_eq!(out.psi_reason, None);
    }

    #[test]
    fn disappearance_preserves_prefix_without_claiming_final() {
        // OOM prefix survives a vanished endpoint; the terminal verdict
        // stays disappearance and final availability is decided outside.
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 1}), None, 100, 5, 42);
        let values = env.values(serde_json::json!({"oom_kill": 4}), None, 900, 45, 42);
        let out = summarize_interval(
            &base,
            Some(&values),
            TerminalState::Disappeared,
            &env.session(),
        );
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(3));
        assert_eq!(out.delta_reason, None);
    }

    #[test]
    fn unconfirmed_endpoint_keeps_verified_prefix() {
        // The final manager query failed, but baseline and prefix
        // endpoints carry full identity: the prefix stands on its own.
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 1}), None, 100, 5, 42);
        let values = env.values(serde_json::json!({"oom_kill": 2}), None, 900, 45, 42);
        let out = summarize_interval(
            &base,
            Some(&values),
            TerminalState::EndpointUnknown,
            &env.session(),
        );
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(1));
    }

    #[test]
    fn restart_uses_verified_prefix_never_post_change_ticks() {
        // Post-change ticks still carry old labels until the next poll:
        // latest claims oom_kill 8 (delta 3 would bridge lifetimes) while
        // the verified pre-change snapshot claims 6 (delta 1, honest).
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        let polluted = env.values(serde_json::json!({"oom_kill": 8}), None, 950, 47, 42);
        let verified = env.values(serde_json::json!({"oom_kill": 6}), None, 800, 40, 42);
        let out = summarize_interval(
            &base,
            Some(&verified),
            TerminalState::Restarted,
            &env.session(),
        );
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(1));
        // The polluted map must never be handed to summarize with a
        // restart terminal; assert the guard directly on its own terms:
        // same inputs through the latest path would fabricate delta 3.
        let bad = summarize_interval(
            &base,
            Some(&polluted),
            TerminalState::Intact,
            &env.session(),
        );
        assert_eq!(bad.event_deltas.unwrap()["oom_kill"], serde_json::json!(3));
    }

    #[test]
    fn restart_without_verified_prefix_reports_no_delta() {
        // Change detected before the first successful poll: no verified
        // values exist, so nothing feeds the delta and the terminal
        // reason is retained.
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        let out = summarize_interval(&base, None, TerminalState::Restarted, &env.session());
        assert_eq!(out.event_deltas, None);
        assert_eq!(out.delta_reason.as_deref(), Some("restart-detected"));
        assert_eq!(out.psi, None);
        assert_eq!(out.psi_reason.as_deref(), Some("restart-detected"));
    }

    #[test]
    fn replacement_without_verified_prefix_reports_no_delta() {
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        let out = summarize_interval(&base, None, TerminalState::Replaced, &env.session());
        assert_eq!(out.event_deltas, None);
        assert_eq!(out.delta_reason.as_deref(), Some("replacement-suspected"));
    }

    #[test]
    fn prefix_endpoints_must_carry_identity() {
        // Verified values from a session without invocation evidence must
        // not produce deltas, even with a matching terminal.
        let mut env = SummaryEnv::new();
        env.invocation.clear();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        let values = env.values(serde_json::json!({"oom_kill": 9}), None, 900, 45, 42);
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.event_deltas, None);
        assert_eq!(out.delta_reason.as_deref(), Some("missing-identity"));
    }

    #[test]
    fn readable_prefix_to_unreadable_final_is_partial_not_zero() {
        // Counters readable at baseline but unknown at the prefix end.
        // Unknown measurements are never inserted into the readable map,
        // so the end is absent: partial evidence, never a zero delta.
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        let values: BTreeMap<String, LastReadable> = BTreeMap::new();
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.event_deltas, None);
        assert_eq!(out.delta_reason.as_deref(), Some("partial-observation"));
    }

    #[test]
    fn counter_decrease_and_missing_keys_stay_explicit() {
        let env = SummaryEnv::new();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 5, "max": 9}),
            None,
            100,
            5,
            42,
        );
        // Decrease on one key, missing key on the other.
        let values = env.values(serde_json::json!({"oom_kill": 3}), None, 900, 45, 42);
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.event_deltas, None);
        assert!(matches!(
            out.delta_reason.as_deref(),
            Some("counter-decrease") | Some("missing-key")
        ));
    }

    #[test]
    fn psi_rejects_overshoot_and_inverted_clocks() {
        let env = SummaryEnv::new();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 0}),
            Some((1000, 2000)),
            100,
            5,
            42,
        );
        // Stall delta larger than the elapsed span: incompatible clocks,
        // never a clamped fraction.
        let values = env.values(
            serde_json::json!({"oom_kill": 0}),
            Some(SummaryEnv::pressure_totals(1000 + 900_000, 2000)),
            900,
            45,
            42,
        );
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.psi, None);
        assert_eq!(out.psi_reason.as_deref(), Some("incompatible-clocks"));
        // Inverted clock (end elapsed before baseline): same verdict.
        let mut backwards = values;
        backwards.get_mut("memory.pressure").unwrap().elapsed_ms = 50;
        let out = summarize_interval(
            &base,
            Some(&backwards),
            TerminalState::Intact,
            &env.session(),
        );
        assert_eq!(out.psi, None);
        assert_eq!(out.psi_reason.as_deref(), Some("incompatible-clocks"));
    }

    #[test]
    fn psi_without_lifetime_is_a_reason_not_a_fraction() {
        // Same values as the intact case but a contradictory prefix-end
        // inode suppresses PSI (and the counters sharing that endpoint).
        let env = SummaryEnv::new();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 0}),
            Some((1000, 2000)),
            100,
            5,
            42,
        );
        let values = env.values(
            serde_json::json!({"oom_kill": 0}),
            Some(SummaryEnv::pressure_totals(1100, 2050)),
            900,
            45,
            99,
        );
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.psi, None);
        assert_eq!(out.event_deltas, None);
    }

    #[test]
    fn restart_with_higher_counters_is_no_delta_not_a_difference() {
        // The critical regression: a replacement unit whose counters
        // exceed the old baseline (8 > 5) must not produce a delta of 3.
        // Counter-decrease detection cannot catch this; only lifetime
        // evidence can.
        let mk = |invocation: &str, oom_kill: u64| {
            serde_json::json!({
                "schemaVersion": 1,
                "path": "/sys/fs/cgroup/a.service",
                "observedUnixMs": 1,
                "invocationId": invocation,
                "bootId": "boot-1",
                "inode": 42,
                "files": {
                    "memory.events": {"value": {"oom_kill": oom_kill}, "unknown": null},
                    "memory.events.local": {"value": {}, "unknown": "not-collected"},
                    "cgroup.events": {"value": {"populated": 1, "frozen": 0}, "unknown": null},
                    "memory.current": {"value": 100, "unknown": null},
                    "memory.peak": {"value": 200, "unknown": null},
                    "memory.pressure": {"value": null, "unknown": "not-collected"},
                    "memory.swap.current": {"value": null, "unknown": "not-collected"},
                    "memory.swap.peak": {"value": null, "unknown": "not-collected"},
                    "memory.min": {"value": null, "unknown": "not-collected"},
                    "memory.low": {"value": null, "unknown": "not-collected"},
                    "memory.high": {"value": null, "unknown": "not-collected"},
                    "memory.max": {"value": null, "unknown": "not-collected"},
                    "memory.swap.max": {"value": null, "unknown": "not-collected"},
                    "memory.oom.group": {"value": null, "unknown": "not-collected"}
                }
            })
        };
        let b: amc_telemetry::Snapshot = serde_json::from_value(mk("inv-1", 5)).unwrap();
        let a: amc_telemetry::Snapshot = serde_json::from_value(mk("inv-2", 8)).unwrap();
        let b_id = b.source_identity();
        let a_id = a.source_identity();
        let out =
            amc_telemetry::compare_snapshots(&b, &a, b_id.as_ref(), a_id.as_ref(), true, true);
        let field = &out["memory.events"]["oom_kill"];
        assert!(!field.supported, "restart must not produce a delta");
        assert_eq!(field.reason, "restart-detected");
        assert_eq!(field.delta, None);
    }

    #[test]
    fn samples_and_snapshots_share_one_envelope() {
        // A serialized watch sample reloads through `load_observation`
        // with identity intact, so samples feed `diff` directly.
        let mut files = BTreeMap::new();
        files.insert(
            "memory.events".to_string(),
            amc_telemetry::Measurement::known(serde_json::json!({"oom_kill": 2})),
        );
        let observation = amc_telemetry::Snapshot {
            schema_version: 1,
            path: "/sys/fs/cgroup/a.service".to_string(),
            observed_unix_ms: Some(1),
            observed_monotonic_ms: Some(20),
            sequence: Some(0),
            invocation_id: Some("inv-1".to_string()),
            boot_id: Some("boot-1".to_string()),
            inode: Some(42),
            files,
        };
        let sample = Sample {
            observation,
            observation_id: "obs-1".to_string(),
            clock_domain: amc_telemetry::CLOCK_DOMAIN,
            target: TargetRef {
                unit: "a.service".to_string(),
                cgroup_path: "/a.service".to_string(),
                invocation_id: Some("inv-1".to_string()),
            },
            host: None,
            capture_duration_us: 0,
            schedule_lag_ms: 0,
        };
        let text = serde_json::to_string(&sample).unwrap();
        let loaded = load_observation(&text).unwrap();
        assert_eq!(loaded.path, "/sys/fs/cgroup/a.service");
        let id = loaded.source_identity().expect("identity must survive");
        assert_eq!(id.invocation_id.as_deref(), Some("inv-1"));
        // And a bare snapshot still loads directly.
        let bare = serde_json::to_string(&loaded).unwrap();
        assert_eq!(load_observation(&bare).unwrap().path, loaded.path);
    }

    #[test]
    fn diff_rejects_unbounded_special_files_without_allocating() {
        // A pipe reporting size 0 must not bypass the budget: the bounded
        // handle read enforces it while reading.
        let dir = std::env::temp_dir().join(format!(
            "amc-diff-pipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let big = dir.join("big.json");
        // One probe byte over the budget.
        let mut file = std::fs::File::create(&big).unwrap();
        use std::io::Write;
        file.write_all(&vec![b'x'; DIFF_INPUT_LIMIT as usize + 1])
            .unwrap();
        drop(file);
        assert!(read_bounded_input(&big).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_without_identity_is_missing_identity_not_a_delta() {
        // Legacy files carry no continuity evidence: matching paths must
        // not produce counter deltas.
        let mk = || {
            serde_json::json!({
                "schemaVersion": 1,
                "path": "/sys/fs/cgroup/a.service",
                "observedUnixMs": 1,
                "files": {
                    "memory.events": {"value": {"oom_kill": 1}, "unknown": null},
                    "memory.events.local": {"value": {}, "unknown": "not-collected"},
                    "cgroup.events": {"value": {"populated": 1, "frozen": 0}, "unknown": null},
                    "memory.current": {"value": 100, "unknown": null},
                    "memory.peak": {"value": 200, "unknown": null},
                    "memory.pressure": {"value": null, "unknown": "not-collected"},
                    "memory.swap.current": {"value": null, "unknown": "not-collected"},
                    "memory.swap.peak": {"value": null, "unknown": "not-collected"},
                    "memory.min": {"value": null, "unknown": "not-collected"},
                    "memory.low": {"value": null, "unknown": "not-collected"},
                    "memory.high": {"value": null, "unknown": "not-collected"},
                    "memory.max": {"value": null, "unknown": "not-collected"},
                    "memory.swap.max": {"value": null, "unknown": "not-collected"},
                    "memory.oom.group": {"value": null, "unknown": "not-collected"}
                }
            })
        };
        let b: amc_telemetry::Snapshot = serde_json::from_value(mk()).unwrap();
        let mut after = mk();
        after["files"]["memory.events"]["value"] = serde_json::json!({"oom_kill": 3});
        let a: amc_telemetry::Snapshot = serde_json::from_value(after).unwrap();
        assert!(b.source_identity().is_none());
        let out = amc_telemetry::compare_snapshots(&b, &a, None, None, true, true);
        assert!(!out["memory.events"]["oom_kill"].supported);
        assert_eq!(out["memory.events"]["oom_kill"].reason, "missing-identity");
    }

    #[test]
    fn replacement_followed_by_disappearance_keeps_verified_prefix() {
        // Loop saw a same-path replacement; the endpoint later vanished.
        // Loop evidence wins (replacement), and the verified pre-change
        // prefix still feeds the delta — the later disappearance neither
        // erases earlier evidence nor rewrites the verdict.
        use amc_telemetry::Comparability::*;
        assert_eq!(
            terminal_state(LoopOutcome::ReplacementSuspected, Disappeared),
            TerminalState::Replaced
        );
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        let verified = env.values(serde_json::json!({"oom_kill": 6}), None, 800, 40, 42);
        let latest: BTreeMap<String, LastReadable> = BTreeMap::new();
        let prefix = select_prefix(TerminalState::Replaced, Some(&verified), &latest);
        let out = summarize_interval(&base, prefix, TerminalState::Replaced, &env.session());
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(1));
        assert_eq!(out.delta_reason, None);
    }

    #[test]
    fn final_gap_restart_with_increasing_counters_is_no_bridge() {
        // Clean loop (`deadline`) but the independent final query sees a
        // restart; counters increased (8 > 5) so monotonicity cannot catch
        // it. Only lifetime evidence suppresses the delta.
        use amc_telemetry::Comparability::*;
        assert_eq!(
            terminal_state(LoopOutcome::Deadline, RestartDetected),
            TerminalState::Restarted
        );
        let env = SummaryEnv::new();
        let base = env.baseline(serde_json::json!({"oom_kill": 5}), None, 100, 5, 42);
        // Verified pre-change prefix claims 6 (honest delta 1); the latest
        // post-change tick claims 8 (bridged delta 3 must never surface).
        let verified = env.values(serde_json::json!({"oom_kill": 6}), None, 800, 40, 42);
        let latest = env.values(serde_json::json!({"oom_kill": 8}), None, 950, 47, 42);
        let prefix = select_prefix(TerminalState::Restarted, Some(&verified), &latest);
        let out = summarize_interval(&base, prefix, TerminalState::Restarted, &env.session());
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(1));
        // No verified prefix at all: increasing latest counters still yield
        // no delta, only the terminal reason.
        let out = summarize_interval(
            &base,
            select_prefix(TerminalState::Restarted, None, &BTreeMap::new()),
            TerminalState::Restarted,
            &env.session(),
        );
        assert_eq!(out.event_deltas, None);
        assert_eq!(out.delta_reason.as_deref(), Some("restart-detected"));
    }

    #[test]
    fn event_and_psi_endpoints_are_independent() {
        // Pressure unreadable on the last tick: events keep their newer
        // endpoint while PSI keeps its older one. Neither acquires the
        // other's timestamp.
        let env = SummaryEnv::new();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 0}),
            Some((1000, 2000)),
            100,
            5,
            42,
        );
        let mut values = BTreeMap::new();
        values.insert(
            "memory.events".to_string(),
            env.readable(serde_json::json!({"oom_kill": 2}), 900, 45, 42),
        );
        values.insert(
            "memory.pressure".to_string(),
            env.readable(SummaryEnv::pressure_totals(1100, 2050), 700, 35, 42),
        );
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(2));
        assert!(out.psi.is_some());
        assert_eq!(out.event_end.map(|m| m.elapsed_ms), Some(900));
        assert_eq!(out.event_end.map(|m| m.sequence), Some(45));
        assert_eq!(out.psi_end.map(|m| m.elapsed_ms), Some(700));
        assert_eq!(out.psi_end.map(|m| m.sequence), Some(35));
    }

    #[test]
    fn partial_pressure_keeps_event_delta() {
        // Missing pressure value suppresses only PSI; the counter delta
        // stands on its own endpoint.
        let env = SummaryEnv::new();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 1}),
            Some((1000, 2000)),
            100,
            5,
            42,
        );
        let mut values = BTreeMap::new();
        values.insert(
            "memory.events".to_string(),
            env.readable(serde_json::json!({"oom_kill": 3}), 900, 45, 42),
        );
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.event_deltas.unwrap()["oom_kill"], serde_json::json!(2));
        assert_eq!(out.psi, None);
        assert_eq!(out.psi_reason.as_deref(), Some("partial-observation"));
    }

    #[test]
    fn psi_without_identity_is_a_reason_not_a_fraction() {
        // Same pressure values but no invocation evidence: PSI suppressed
        // alongside the counters, never a fraction without lifetime.
        let mut env = SummaryEnv::new();
        env.invocation.clear();
        let base = env.baseline(
            serde_json::json!({"oom_kill": 0}),
            Some((1000, 2000)),
            100,
            5,
            42,
        );
        let values = env.values(
            serde_json::json!({"oom_kill": 0}),
            Some(SummaryEnv::pressure_totals(1100, 2050)),
            900,
            45,
            42,
        );
        let out = summarize_interval(&base, Some(&values), TerminalState::Intact, &env.session());
        assert_eq!(out.psi, None);
        assert_eq!(out.psi_reason.as_deref(), Some("missing-identity"));
        assert_eq!(out.event_deltas, None);
    }

    #[test]
    fn final_availability_requires_durable_persistence() {
        // Readable last-persisted counters with intact terminal and live
        // endpoint are available only when persistence itself succeeded.
        // A stale previously-persisted value after a failed final write
        // must not acquire availability.
        let snapshot: amc_telemetry::Snapshot = serde_json::from_value(serde_json::json!({
            "schemaVersion": 1, "path": "/sys/fs/cgroup/a.service", "observedUnixMs": 1,
            "files": {
                "memory.events": {"value": {"oom_kill": 3}, "unknown": null},
                "memory.events.local": {"value": {}, "unknown": "not-collected"},
                "cgroup.events": {"value": {"populated": 1, "frozen": 0}, "unknown": null},
                "memory.current": {"value": 100, "unknown": null},
                "memory.peak": {"value": 200, "unknown": null},
                "memory.pressure": {"value": null, "unknown": "not-collected"},
                "memory.swap.current": {"value": null, "unknown": "not-collected"},
                "memory.swap.peak": {"value": null, "unknown": "not-collected"},
                "memory.min": {"value": null, "unknown": "not-collected"},
                "memory.low": {"value": null, "unknown": "not-collected"},
                "memory.high": {"value": null, "unknown": "not-collected"},
                "memory.max": {"value": null, "unknown": "not-collected"},
                "memory.swap.max": {"value": null, "unknown": "not-collected"},
                "memory.oom.group": {"value": null, "unknown": "not-collected"}
            }
        }))
        .unwrap();
        assert!(final_available(
            TerminalState::Intact,
            true,
            false,
            true,
            Some(&snapshot)
        ));
        // Any persistence failure forces unavailable, even with readable
        // counters and an intact terminal.
        assert!(!final_available(
            TerminalState::Intact,
            true,
            true,
            true,
            Some(&snapshot)
        ));
        assert!(!final_available(
            TerminalState::Intact,
            true,
            false,
            false,
            Some(&snapshot)
        ));
        assert!(!final_available(
            TerminalState::Restarted,
            true,
            false,
            true,
            Some(&snapshot)
        ));
        assert!(!final_available(
            TerminalState::Intact,
            true,
            false,
            true,
            None
        ));
    }

    #[test]
    fn output_budget_is_one_probe_byte_past_the_cap() {
        for limit in [MAX_OUTPUT_BYTES, PRODUCTION_MAX_OUTPUT_BYTES] {
            assert!(!budget_exceeded(0, 10, limit));
            assert!(!budget_exceeded(limit - 11, 10, limit));
            assert!(budget_exceeded(limit - 10, 10, limit));
            assert!(budget_exceeded(limit, 0, limit));
            assert!(budget_exceeded(u64::MAX, 1, limit));
        }
    }
}
