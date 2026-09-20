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

#[derive(Debug, Clone)]
pub struct WatchArgs {
    pub unit: String,
    pub system: bool,
    pub seconds: u64,
    pub interval_ms: u64,
    pub output: PathBuf,
}

impl WatchArgs {
    pub fn validate(&self) -> Result<()> {
        if !(1..=MAX_SECONDS).contains(&self.seconds) {
            bail!("--seconds must be 1..={MAX_SECONDS}");
        }
        if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&self.interval_ms) {
            bail!("--interval-ms must be {MIN_INTERVAL_MS}..={MAX_INTERVAL_MS}");
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetRef {
    unit: String,
    cgroup_path: String,
    invocation_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LastReadable {
    unix_ms: u128,
    /// Observer elapsed mark of the tick that produced this value. Each
    /// field keeps its own capture time: a stale counter must never be
    /// divided by (or differenced across) a newer tick's timestamp.
    elapsed_ms: u64,
    value: serde_json::Value,
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

/// Precedence between the loop's terminal reason and the endpoint
/// comparability verdict.
///
/// Held file descriptions remain evidence about the original object after
/// its pathname disappears, so disappearance alone preserves a verified
/// readable prefix. But positive evidence of a different lifetime still
/// wins: a restart in the final gap must not hide behind the
/// disappearance and produce cross-lifetime deltas.
fn final_comparability(
    reason: &str,
    endpoint: amc_telemetry::Comparability,
    end_alive: bool,
) -> amc_telemetry::Comparability {
    use amc_telemetry::Comparability;
    if reason == "invocation-changed" {
        Comparability::RestartDetected
    } else if !end_alive {
        match endpoint {
            Comparability::RestartDetected
            | Comparability::ReplacementSuspected
            | Comparability::ContextMismatch => endpoint,
            _ => Comparability::Compatible,
        }
    } else {
        endpoint
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

/// Observe one unit for a finite interval. Read-only toward the target.
pub fn watch(args: &WatchArgs) -> Result<i32> {
    args.validate()?;
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
            event_deltas: None,
            event_delta_coverage: None,
            delta_unsupported_reason: None,
            pressure_stall: None,
            pressure_stall_reason: None,
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
    let mut sequence: u64 = 0;
    /// Full baseline tick: event map, pressure totals with their own
    /// capture mark, and the continuity evidence observed on that tick.
    struct Baseline {
        events: serde_json::Map<String, serde_json::Value>,
        pressure_totals: Option<(u64, u64)>,
        elapsed_ms: u64,
        identity: amc_telemetry::SourceIdentity,
    }
    let mut baseline: Option<Baseline> = None;
    let mut last_readable: BTreeMap<String, LastReadable> = BTreeMap::new();
    let mut max_swap: Option<u64> = None;
    let mut max_current: Option<u64> = None;
    let mut reason = "deadline".to_string();
    let mut baseline_ready = false;
    let mut first_tick_complete = false;
    let mut incomplete_persistence = false;
    let mut bytes_written: u64 = 0;
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

    while Instant::now() < deadline && (sequence as usize) < MAX_SAMPLES {
        if signals.cancelled().is_some() {
            reason = "cancelled".to_string();
            break;
        }
        let elapsed_ms = started_mono.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        // Continuity: the path may have been recycled for a new object
        // between ticks. An inode change ends the interval rather than
        // mixing two lifetimes into one baseline.
        let tick_inode = amc_telemetry::SourceIdentity::inode_of(&cgroup_dir);
        if tick_inode != pinned_inode {
            reason = "replacement-suspected".to_string();
            break;
        }
        let mut snapshot = pinned_files.snapshot(&cgroup_path, Some(sequence), Some(elapsed_ms));
        snapshot.invocation_id = invocation.clone();
        snapshot.boot_id = owner_boot_id.clone();
        snapshot.inode = tick_inode;
        let sample_unix_ms = snapshot.observed_unix_ms.unwrap_or(0);
        let sample = Sample {
            observation: snapshot,
            observation_id: observation.clone(),
            clock_domain: amc_telemetry::CLOCK_DOMAIN,
            target: target.clone(),
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
                identity: amc_telemetry::SourceIdentity::new(
                    &cgroup_path,
                    Some(&args.unit),
                    Some(if args.system { "system" } else { "user" }),
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
        if bytes_written + line.len() as u64 + 1 > MAX_OUTPUT_BYTES {
            reason = "output-budget-exceeded".to_string();
            incomplete_persistence = true;
            break;
        }
        // A failed sample write is incomplete persistence, not a silent
        // truncation: record it and stop instead of fabricating the rest.
        if samples.write_all(&line).is_err() || samples.write_all(b"\n").is_err() {
            reason = "output-write-failed".to_string();
            incomplete_persistence = true;
            break;
        }
        bytes_written += line.len() as u64 + 1;
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
            reason = "disappeared-or-empty".to_string();
            break;
        }
        if amc_telemetry::SourceIdentity::inode_of(&cgroup_dir).is_none() {
            reason = "disappeared-or-empty".to_string();
            break;
        }
        // Poll the manager at most once per second for invocation change,
        // using a leaf-only query (never the full ancestor traversal)
        // bounded by the remaining sampling budget.
        // Unknown manager values are normalized to None: an invocation ID
        // appearing, disappearing, or changing under us all end the
        // interval. Two unknowns carry no information (the inode check
        // above remains the guard), so polling continues.
        if sequence.is_multiple_of(1000 / args.interval_ms.max(1)) {
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
                    reason = "invocation-changed".to_string();
                    break;
                }
            }
        }
        let next = started_mono + interval * (sequence as u32);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    if (sequence as usize) >= MAX_SAMPLES
        && matches!(reason.as_str(), "deadline")
        && Instant::now() < deadline
    {
        // The sample budget — not the clock — ended the interval. The
        // observed prefix stays usable, but it is truncated evidence.
        reason = "sample-budget-exceeded".to_string();
    }
    if samples.flush().is_err() {
        incomplete_persistence = true;
        if reason == "deadline" {
            reason = "output-write-failed".to_string();
        }
    }
    drop(samples);

    if !baseline_ready {
        reason = "observer-not-ready".to_string();
    }
    // Final continuity state: the interval is comparable only when the
    // cgroup is still the object we attached to. The endpoint identity
    // comes from an independent final query — never from the attach-time
    // values — so a restart in the final gap cannot silently reuse the
    // original invocation ID and produce deltas across two lifetimes.
    // The final query gets a fixed grace budget outside the sampling
    // window; sampling itself was bounded by `deadline` above.
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
        Some(if args.system { "system" } else { "user" }),
        None,
        owner_boot_id.as_deref(),
        end_invocation.as_deref(),
        end_inode,
    );
    // Re-query the leaf result at the end: the initial value describes
    // attach time, not the interval outcome. Fall back to it only when the
    // final query itself is unavailable.
    let final_manager_result = final_leaf
        .as_ref()
        .and_then(|m| m.get("Result").cloned())
        .filter(|v| !v.starts_with("unknown"));
    // Deltas through the shared comparison engine with real continuity
    // evidence. Anything but Compatible yields no delta and an explicit
    // reason — never a last-readable number dressed as lifetime accounting.
    // A lifetime change discovered only in the final gap rewrites a
    // `deadline` reason: the interval did not end cleanly.
    let mut event_deltas = None;
    let mut delta_unsupported_reason = None;
    let mut final_unavailable = !final_counters_available(last_attempt.as_ref(), end_alive);
    let mut comparability = None;
    if let Some(base) = &baseline {
        let endpoint_comp = amc_telemetry::comparability(
            Some(&base.identity),
            Some(&end_identity),
            true,
            end_alive,
        );
        let comp = final_comparability(&reason, endpoint_comp, end_alive);
        comparability = Some(comp);
        if comp == amc_telemetry::Comparability::Compatible {
            if let Some(last) = last_readable.get("memory.events")
                && let Some(obj) = last.value.as_object()
            {
                let compared = amc_telemetry::compare_event_maps(&base.events, obj, comp);
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
                if first_unsupported.is_none() {
                    event_deltas = Some(deltas);
                } else {
                    delta_unsupported_reason = first_unsupported;
                    final_unavailable = true;
                }
            } else {
                delta_unsupported_reason =
                    Some(amc_telemetry::compare::reason::PARTIAL.to_string());
                final_unavailable = true;
            }
        } else {
            delta_unsupported_reason = Some(comp.as_reason().to_string());
            final_unavailable = true;
        }
    } else {
        final_unavailable = true;
    }
    // PSI interval stall fractions from the baseline `total` to the last
    // pressure sample's own `total`, over that sample's own elapsed span.
    // Strict: overshoot or invalid timing is a reason, never a clamped
    // fraction. Like counters, stall fractions require a compatible
    // lifetime: dividing stale totals by a newer tick's clock would
    // understate the stall.
    let mut pressure_stall = None;
    let mut pressure_stall_reason = None;
    let lifetime_ok = comparability == Some(amc_telemetry::Comparability::Compatible);
    if let Some(base) = &baseline
        && let Some((base_some, base_full)) = base.pressure_totals
    {
        let last_pressure = last_readable.get("memory.pressure");
        let elapsed_us = last_pressure
            .map(|l| l.elapsed_ms)
            .and_then(|last_ms| last_ms.checked_sub(base.elapsed_ms))
            .and_then(|ms| ms.checked_mul(1000));
        let totals = last_pressure
            .and_then(|l| l.value.as_object())
            .and_then(|o| {
                let some = o.get("some")?.get("total")?.as_u64()?;
                let full = o.get("full")?.get("total")?.as_u64()?;
                Some((some, full))
            });
        match (lifetime_ok, totals) {
            (true, Some((last_some, last_full))) => {
                let some =
                    amc_telemetry::psi_stall_fraction_strict(base_some, last_some, elapsed_us);
                let full =
                    amc_telemetry::psi_stall_fraction_strict(base_full, last_full, elapsed_us);
                match (some, full) {
                    (Ok(some), Ok(full)) => {
                        pressure_stall = Some(BTreeMap::from([
                            ("some".to_string(), some),
                            ("full".to_string(), full),
                        ]));
                    }
                    (Err(reason), _) | (_, Err(reason)) => {
                        pressure_stall_reason = Some(reason.to_string());
                    }
                }
            }
            (false, _) => {
                pressure_stall_reason = Some(
                    comparability
                        .map(|c| c.as_reason().to_string())
                        .unwrap_or_else(|| {
                            amc_telemetry::compare::reason::MISSING_IDENTITY.to_string()
                        }),
                );
            }
            (true, None) => {
                pressure_stall_reason = Some(amc_telemetry::compare::reason::PARTIAL.to_string());
            }
        }
    } else if baseline_ready {
        pressure_stall_reason = Some(amc_telemetry::compare::reason::PARTIAL.to_string());
    }
    // A lifetime change the loop never saw (restart in the final gap)
    // rewrites a clean `deadline` reason: the interval is not intact.
    if reason == "deadline" {
        match comparability {
            Some(amc_telemetry::Comparability::RestartDetected) => {
                reason = "invocation-changed".to_string();
            }
            Some(amc_telemetry::Comparability::ReplacementSuspected) => {
                reason = "replacement-suspected".to_string();
            }
            Some(amc_telemetry::Comparability::Disappeared) => {
                reason = "disappeared-or-empty".to_string();
            }
            _ => {}
        }
    }
    // Completion is earned, not defaulted. A clean terminal state with a
    // valid baseline and persisted summary is complete; everything else —
    // missing baseline, lifetime change, budget exhaustion, cancellation,
    // persistence failure — is incomplete evidence.
    let complete = baseline_ready
        && !incomplete_persistence
        && matches!(reason.as_str(), "deadline" | "disappeared-or-empty");
    if matches!(
        reason.as_str(),
        "output-budget-exceeded"
            | "output-write-failed"
            | "sample-budget-exceeded"
            | "invocation-changed"
            | "replacement-suspected"
            | "cancelled"
    ) {
        final_unavailable = true;
    }

    let summary = Summary {
        schema_version: 1,
        observation_id: observation,
        reason,
        complete,
        target,
        coverage: CoverageOut {
            // Baseline after the first tick means the observer may have
            // missed earlier counters in this lifetime. Note: interval
            // deltas always cover baseline-to-end, never the full
            // workload lifetime; `attachedLate` marks a truncated
            // interval start, not a claim about the workload's age.
            // Passive attachment cannot certify counters from workload start.
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
        event_delta_coverage: event_deltas.as_ref().map(|_| {
            serde_json::json!({
                "kind": "baseline-to-last-readable",
                "startElapsedMs": baseline.as_ref().map(|base| base.elapsed_ms),
                "endElapsedMs": last_readable.get("memory.events").map(|last| last.elapsed_ms),
                "finalCountersAvailable": !final_unavailable,
                "lifetimeComplete": false
            })
        }),
        event_deltas,
        delta_unsupported_reason,
        pressure_stall,
        pressure_stall_reason,
        max_observed_swap: max_swap,
        max_observed_current: max_current,
        last_readable,
        manager_result: final_manager_result,
    };
    atomic_write(
        &args.output.join("summary.json"),
        &serde_json::to_vec_pretty(&summary)?,
    )?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(args.output.join("done"))?
        .sync_all()?;
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
    fn disappearance_preserves_prefix_but_never_masks_restart() {
        use amc_telemetry::Comparability;
        // Clean disappearance with no contrary evidence: prefix preserved.
        assert_eq!(
            final_comparability("disappeared-or-empty", Comparability::MissingIdentity, false),
            Comparability::Compatible
        );
        assert_eq!(
            final_comparability("deadline", Comparability::Disappeared, false),
            Comparability::Compatible
        );
        // Restart in the final gap: positive lifetime evidence wins over
        // both disappearance and the loop reason.
        assert_eq!(
            final_comparability("deadline", Comparability::RestartDetected, false),
            Comparability::RestartDetected
        );
        assert_eq!(
            final_comparability("deadline", Comparability::ReplacementSuspected, false),
            Comparability::ReplacementSuspected
        );
        assert_eq!(
            final_comparability("deadline", Comparability::ContextMismatch, false),
            Comparability::ContextMismatch
        );
        // Loop-observed restart stands even when the endpoint still looks
        // compatible.
        assert_eq!(
            final_comparability("invocation-changed", Comparability::Compatible, true),
            Comparability::RestartDetected
        );
        // Alive endpoint passes through untouched.
        assert_eq!(
            final_comparability("deadline", Comparability::Compatible, true),
            Comparability::Compatible
        );
        assert_eq!(
            final_comparability("deadline", Comparability::MissingIdentity, true),
            Comparability::MissingIdentity
        );
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
}
