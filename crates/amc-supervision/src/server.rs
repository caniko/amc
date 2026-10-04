//! Host-wide arbitration. Sampling has no manager subprocesses; discovery and
//! actuation run in separate bounded workers. Unknown observations inhibit
//! admission in enforce mode and never supply recovery evidence.
use crate::{
    forecast::{Forecast, Report},
    native::{self, Manager, Session, Target},
    policy::{Domain, Lifecycle, Mode, Policy},
    recovery::{Action, FailedIdentity, Phase, Recovery, RecoveryIdentity, State},
    store::{Store, atomic_json},
};
use amc_admission::{
    health::{Health, boot_ms},
    server::now_ms,
};
use amc_telemetry::host::{self, HostSnapshot};
use anyhow::{Result, ensure};
use rustix::process::Signal;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct Known {
    identities: BTreeMap<String, crate::recovery::Identity>,
    priority: Option<Domain>,
}
struct Seen {
    domain: Domain,
    shown: Option<BTreeMap<String, String>>,
    session: Option<Session>,
    failed: Option<FailedIdentity>,
    observed_boot_ms: u64,
}
enum Discovery {
    Seen(Box<Seen>),
    Pools(BTreeSet<String>, bool, u64),
}
enum Work {
    Signal(Target, Signal),
    Start(Domain, RecoveryIdentity),
}
struct WorkResult {
    id: String,
    invocation: String,
    ok: bool,
}

#[derive(Clone, Copy)]
enum Intervention {
    Term,
    Kill,
    Start,
}

fn permitted(
    active: Option<&Recovery>,
    domain: &Domain,
    invocation: &str,
    kind: Intervention,
    ms: u64,
) -> bool {
    let Some(active) = active else {
        return false;
    };
    if &active.domain != domain
        || active.identity.invocation() != invocation
        || domain.lifecycle == Lifecycle::Observe
        || (matches!(kind, Intervention::Start) && domain.lifecycle != Lifecycle::Restart)
        || (!matches!(kind, Intervention::Start)
            && !matches!(active.identity, RecoveryIdentity::Process(_)))
    {
        return false;
    }
    match (&active.phase, kind) {
        (Phase::Terminating { deadline_ms }, Intervention::Term)
        | (Phase::Killing { deadline_ms }, Intervention::Kill)
        | (Phase::Starting { deadline_ms }, Intervention::Start) => ms < *deadline_ms,
        _ => false,
    }
}

#[derive(Serialize)]
pub struct DomainReport {
    pub id: String,
    pub status: &'static str,
    pub lifecycle: Lifecycle,
    pub identity: Option<crate::recovery::Identity>,
    pub resident_bytes: Option<u64>,
    pub swap_bytes: Option<u64>,
    pub forecast: Option<Report>,
    pub emergency: bool,
    pub boundaries: Vec<native::Boundary>,
}

#[derive(Serialize)]
pub struct Status<'a> {
    pub version: u32,
    pub boot_id: &'a str,
    pub observed_boot_ms: u64,
    pub mode: Mode,
    pub inhibit: bool,
    pub degraded: bool,
    pub trace_drops: u64,
    pub host: &'a HostSnapshot,
    pub domains: &'a [DomainReport],
    pub recovery: &'a State,
    pub policy: &'a Policy,
}

fn discover_one(manager: &Manager, domain: &Domain, policy: &Policy, known: &Mutex<Known>) -> Seen {
    let shown = manager.show(domain).ok();
    let session = if let Some(shown) = &shown {
        let changed = known.lock().ok().is_none_or(|k| {
            k.identities.get(&domain.id).is_none_or(|i| {
                i.invocation != native::field(shown, "InvocationID")
                    || i.pid.to_string() != native::field(shown, "MainPID")
            })
        });
        if native::field(shown, "ActiveState") == "active" && changed {
            manager.attach(domain.clone(), policy).ok()
        } else {
            None
        }
    } else {
        None
    };
    let failed = shown
        .as_ref()
        .filter(|s| native::field(s, "ActiveState") == "failed")
        .filter(|_| {
            known
                .lock()
                .ok()
                .is_some_and(|k| !k.identities.contains_key(&domain.id))
        })
        .and_then(|_| manager.failed_identity(domain, policy).ok());
    Seen {
        domain: domain.clone(),
        shown,
        session,
        failed,
        observed_boot_ms: boot_ms().unwrap_or(0),
    }
}

fn discovery_worker(
    policy: Policy,
    manager: Manager,
    boot: String,
    known: Arc<Mutex<Known>>,
    tx: mpsc::SyncSender<Discovery>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let jobs = native::job_domains(&policy, &boot);
        let healthy = jobs.is_ok();
        let jobs = jobs.unwrap_or_default();
        let _ = tx.try_send(Discovery::Pools(
            jobs.iter().map(|d| d.id.clone()).collect(),
            healthy,
            boot_ms().unwrap_or(0),
        ));
        for domain in policy.domains.iter().chain(&jobs) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Recovery metadata gets priority between every ordinary query.
            let priority = known.lock().ok().and_then(|k| k.priority.clone());
            if let Some(p) = priority {
                let _ = tx.try_send(Discovery::Seen(Box::new(discover_one(
                    &manager, &p, &policy, &known,
                ))));
            }
            let _ = tx.try_send(Discovery::Seen(Box::new(discover_one(
                &manager, domain, &policy, &known,
            ))));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn actuation_worker(
    manager: Manager,
    policy: Policy,
    authority: Arc<Mutex<Option<Recovery>>>,
    rx: mpsc::Receiver<Work>,
    tx: mpsc::SyncSender<WorkResult>,
) {
    while let Ok(work) = rx.recv() {
        let (id, invocation) = match &work {
            Work::Start(d, i) => (d.id.clone(), i.invocation().to_owned()),
            Work::Signal(t, _) => (t.domain.id.clone(), t.identity.invocation.clone()),
        };
        let authorized = |domain: &Domain, kind| {
            authority
                .lock()
                .ok()
                .zip(boot_ms().ok())
                .is_some_and(|(a, ms)| permitted(a.as_ref(), domain, &invocation, kind, ms))
        };
        let result = match work {
            Work::Start(domain, origin) => manager.start(&domain, &origin, &policy, || {
                authorized(&domain, Intervention::Start)
                    && host_healthy(&host::snapshot(Path::new("/proc"), None), &policy)
            }),
            Work::Signal(target, signal) => (|| {
                let shown = manager.show(&target.domain)?;
                let kind = if signal == Signal::TERM {
                    Intervention::Term
                } else {
                    Intervention::Kill
                };
                ensure!(
                    authorized(&target.domain, kind),
                    "recovery action expired or revoked"
                );
                // If the original already exited, do not signal any other PID.
                if matches!(
                    native::field(&shown, "ActiveState"),
                    "inactive" | "failed" | "deactivating"
                ) && native::field(&shown, "MainPID") == "0"
                    && (native::field(&shown, "InvocationID").is_empty()
                        || native::field(&shown, "InvocationID") == target.identity.invocation)
                {
                    return Ok(());
                }
                ensure!(
                    native::field(&shown, "InvocationID") == target.identity.invocation
                        && native::field(&shown, "MainPID") == target.identity.pid.to_string()
                        && native::field(&shown, "ControlGroup") == target.identity.cgroup
                        && native::field(&shown, "Restart") == "no"
                        && native::field(&shown, "KillMode") == "control-group"
                        && native::field(&shown, "OOMPolicy") == "kill"
                        && native::cleanup_bounded(&shown, &policy)
                        && native::limits_match(&shown, &target.domain),
                    "native authority/identity changed"
                );
                target.signal(signal)
            })(),
        };
        if tx
            .send(WorkResult {
                id,
                invocation,
                ok: result.is_ok(),
            })
            .is_err()
        {
            break;
        }
    }
}

fn trace_worker(directory: PathBuf, rx: mpsc::Receiver<Vec<u8>>) {
    while let Ok(mut bytes) = rx.recv() {
        let result = (|| -> Result<()> {
            let path = directory.join("trace.jsonl");
            if fs::metadata(&path).is_ok_and(|m| m.len() >= 64 * 1024 * 1024) {
                fs::rename(&path, directory.join("trace.previous.jsonl"))?;
            }
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(path)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            Ok(())
        })();
        if result.is_err() {
            break;
        }
    }
}

fn host_number(snapshot: &HostSnapshot, key: &str) -> Option<u64> {
    snapshot.files.get(key)?.value.as_ref()?.as_u64()
}
fn psi(snapshot: &HostSnapshot) -> Option<f64> {
    snapshot
        .files
        .get("pressure.memory")?
        .value
        .as_ref()?
        .get("full")?
        .get("avg10")?
        .as_f64()
        .map(|p| p / 100.0)
}

fn host_healthy(snapshot: &HostSnapshot, policy: &Policy) -> bool {
    host_number(snapshot, "meminfo.MemTotal")
        .zip(host_number(snapshot, "meminfo.MemAvailable"))
        .is_some_and(|(total, available)| {
            total > policy.reserve_bytes && available <= total && available >= policy.reserve_bytes
        })
        && psi(snapshot).is_some_and(|p| p.is_finite() && p >= 0.0 && p < policy.emergency_full_psi)
}

/// Start exactly one host-wide owner. Runtime is public read-only; durable
/// history and rotating traces are private. Persistence failure ends service;
/// its stale heartbeat then inhibits admission, leaving hard ceilings intact.
pub fn serve(
    policy: Policy,
    state_path: &Path,
    runtime: &Path,
    manager: Manager,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    policy.validate()?;
    ensure!(
        nix::unistd::geteuid().is_root(),
        "host supervision requires root native-manager access"
    );
    ensure!(
        state_path.is_absolute() && runtime.is_absolute(),
        "supervision paths must be absolute"
    );
    let metadata = fs::symlink_metadata(runtime)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "untrusted supervisor runtime directory"
    );
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    let (store, mut state) = Store::open(state_path, &boot)?;
    store.save(&state)?;
    let authority = Arc::new(Mutex::new(state.active.clone()));
    let known = Arc::new(Mutex::new(Known::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (updates_tx, updates) = mpsc::sync_channel(128);
    let (work, work_rx) = mpsc::sync_channel(1);
    let (result_tx, results) = mpsc::sync_channel(1);
    let (trace, trace_rx) = mpsc::sync_channel(32);
    thread::spawn({
        let p = policy.clone();
        let m = manager.clone();
        let b = boot.clone();
        let k = known.clone();
        let s = stop.clone();
        move || discovery_worker(p, m, b, k, updates_tx, s)
    });
    thread::spawn({
        let p = policy.clone();
        let a = authority.clone();
        move || actuation_worker(manager, p, a, work_rx, result_tx)
    });
    thread::spawn({
        let p = state_path.to_owned();
        move || trace_worker(p, trace_rx)
    });
    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut models: BTreeMap<String, Forecast> = BTreeMap::new();
    let mut seen: BTreeMap<String, Option<BTreeMap<String, String>>> = BTreeMap::new();
    let mut seen_at: BTreeMap<String, u64> = BTreeMap::new();
    let mut failures = BTreeMap::new();
    let mut jobs = BTreeSet::new();
    let mut pools_healthy = policy.job_pools.is_empty();
    let mut pools_seen_at = None;
    let mut recovering: Option<Session> = None;
    let mut trace_drops = 0u64;
    let mut healthy_since = None;
    let result = (|| -> Result<()> {
        loop {
            if stopping() {
                break;
            }
            let tick = Instant::now();
            let ms = boot_ms()?;
            for update in updates.try_iter().take(128) {
                match update {
                    Discovery::Seen(update) => {
                        let update = *update;
                        if let Some(session) = update.session
                            && (sessions.len() < 96 || sessions.contains_key(&update.domain.id))
                        {
                            models.remove(&update.domain.id);
                            sessions.insert(update.domain.id.clone(), session);
                        }
                        seen_at.insert(update.domain.id.clone(), update.observed_boot_ms);
                        if let Some(failed) = update.failed {
                            failures.insert(update.domain.id.clone(), failed);
                        } else {
                            failures.remove(&update.domain.id);
                        }
                        seen.insert(update.domain.id, update.shown);
                    }
                    Discovery::Pools(new, healthy, observed) => {
                        jobs = new;
                        pools_healthy = healthy;
                        pools_seen_at = Some(observed);
                    }
                }
            }
            let static_ids: BTreeSet<_> = policy.domains.iter().map(|d| d.id.clone()).collect();
            let wanted = |id: &String| static_ids.contains(id) || jobs.contains(id);
            sessions.retain(|id, _| wanted(id));
            models.retain(|id, _| wanted(id));
            seen.retain(|id, _| {
                wanted(id) || state.active.as_ref().is_some_and(|a| &a.domain.id == id)
            });
            seen_at.retain(|id, _| seen.contains_key(id));
            failures.retain(|id, _| wanted(id));
            let host = host::snapshot(Path::new("/proc"), Some(ms));
            let available = host_number(&host, "meminfo.MemAvailable");
            let total = host_number(&host, "meminfo.MemTotal");
            let full = psi(&host);
            let host_unknown = available.zip(total).is_none_or(|(available, total)| {
                available > total || total <= policy.reserve_bytes
            }) || full
                .is_none_or(|p| !p.is_finite() || !(0.0..=1.0).contains(&p));
            let host_emergency = available.is_some_and(|a| a <= policy.emergency_available_bytes)
                || (available.is_some_and(|a| a < policy.reserve_bytes)
                    && full.is_some_and(|p| p >= policy.emergency_full_psi));
            let healthy = host_healthy(&host, &policy);
            let mut degraded = host_unknown
                || !pools_healthy
                || (!policy.job_pools.is_empty()
                    && pools_seen_at.is_none_or(|at| ms.saturating_sub(at) > 10_000));
            let mut pressure = !healthy;
            let mut reports = Vec::new();
            let mut candidates = Vec::new();
            for id in static_ids.iter().chain(&jobs) {
                let domain = policy
                    .domains
                    .iter()
                    .find(|d| &d.id == id)
                    .cloned()
                    .or_else(|| sessions.get(id).map(|s| s.domain.clone()));
                let Some(domain) = domain else {
                    degraded = true;
                    continue;
                };
                if seen_at
                    .get(id)
                    .is_none_or(|at| ms.saturating_sub(*at) > 10_000)
                {
                    degraded = true;
                }
                if let Some(source) = sessions.get_mut(id) {
                    match source.observations(ms) {
                        Ok((mut snapshot, boundaries)) => {
                            snapshot.boot_id = Some(boot.clone());
                            let mut boundaries = boundaries.ok();
                            if let (Some(total), Some(available)) = (total, available) {
                                if total > policy.reserve_bytes {
                                    if let Some(boundaries) = &mut boundaries {
                                        boundaries.push(native::Boundary {
                                            key: "host.memory.used".into(),
                                            cgroup: "/".into(),
                                            inode: 0,
                                            current_bytes: total.saturating_sub(available),
                                            limit_bytes: total - policy.reserve_bytes,
                                        });
                                    }
                                } else {
                                    degraded = true;
                                }
                            }
                            let resident = native::number(&snapshot, "memory.current").ok();
                            let swap = native::number(&snapshot, "memory.swap.current").ok();
                            let emergency = host_emergency
                                || (domain.memory_max > 0
                                    && resident.is_some_and(|v| {
                                        v as f64 >= domain.memory_max as f64 * 0.98
                                    }))
                                || (domain.memory_swap_max > 0
                                    && swap.is_some_and(|v| {
                                        v as f64 >= domain.memory_swap_max as f64 * 0.98
                                    }));
                            let model = models
                                .entry(id.clone())
                                .or_insert(Forecast::new(policy.forecast)?);
                            let report = if let Some(boundaries) = &boundaries
                                && !host_unknown
                            {
                                let (signature, values) =
                                    native::forecast_input(&source.identity, boundaries)?;
                                model.observe(ms, &signature, &values).ok()
                            } else {
                                None
                            };
                            if report.is_none() || host_unknown {
                                model.censor();
                                degraded = true;
                            }
                            pressure |= emergency || report.as_ref().is_some_and(|r| r.possible);
                            if (emergency
                                || (policy.forecast_recovery
                                    && report.as_ref().is_some_and(|r| r.strong)))
                                && domain.lifecycle != Lifecycle::Observe
                            {
                                candidates.push((
                                    domain.priority,
                                    std::cmp::Reverse(resident.unwrap_or(0)),
                                    id.clone(),
                                ));
                            }
                            reports.push(DomainReport {
                                id: id.clone(),
                                status: if report.as_ref().is_some_and(|r| r.ready) {
                                    "observed"
                                } else if report.is_none() {
                                    "unknown"
                                } else {
                                    "warming"
                                },
                                lifecycle: domain.lifecycle,
                                identity: Some(source.identity.clone()),
                                resident_bytes: resident,
                                swap_bytes: swap,
                                forecast: report,
                                emergency,
                                boundaries: boundaries.unwrap_or_default(),
                            });
                        }
                        Err(_) => {
                            if let Some(model) = models.get_mut(id) {
                                model.censor();
                            }
                            let inactive = seen.get(id).and_then(|s| s.as_ref()).is_some_and(|s| {
                                matches!(native::field(s, "ActiveState"), "inactive" | "failed")
                            });
                            if !inactive
                                && !state.active.as_ref().is_some_and(|a| &a.domain.id == id)
                            {
                                degraded = true;
                            }
                            // A failed dedicated backend can recover through the
                            // same persisted budget; a deliberate stop stays stopped.
                            if healthy
                                && domain.lifecycle == Lifecycle::Restart
                                && seen
                                    .get(id)
                                    .and_then(|s| s.as_ref())
                                    .is_some_and(|s| native::field(s, "ActiveState") == "failed")
                            {
                                candidates.push((
                                    domain.priority,
                                    std::cmp::Reverse(0),
                                    id.clone(),
                                ));
                            }
                            reports.push(DomainReport {
                                id: id.clone(),
                                status: if inactive { "inactive" } else { "unknown" },
                                lifecycle: domain.lifecycle,
                                identity: Some(source.identity.clone()),
                                resident_bytes: None,
                                swap_bytes: None,
                                forecast: None,
                                emergency: false,
                                boundaries: Vec::new(),
                            });
                        }
                    }
                } else {
                    let inactive = seen.get(id).and_then(|s| s.as_ref()).is_some_and(|s| {
                        matches!(native::field(s, "ActiveState"), "inactive" | "failed")
                    });
                    if !inactive && !state.active.as_ref().is_some_and(|a| &a.domain.id == id) {
                        degraded = true;
                    }
                    if domain.lifecycle == Lifecycle::Restart
                        && seen
                            .get(id)
                            .and_then(|s| s.as_ref())
                            .is_some_and(|s| native::field(s, "ActiveState") == "failed")
                    {
                        let verified = failures.get(id).is_some_and(|identity| {
                            seen_at
                                .get(id)
                                .is_some_and(|at| ms.saturating_sub(*at) <= 3000)
                                && native::empty_failed_slot(identity).unwrap_or(false)
                        });
                        if !verified && !state.active.as_ref().is_some_and(|a| &a.domain.id == id) {
                            degraded = true;
                        }
                        if verified && healthy {
                            candidates.push((domain.priority, std::cmp::Reverse(0), id.clone()));
                        }
                    }
                    reports.push(DomainReport {
                        id: id.clone(),
                        status: if inactive { "inactive" } else { "unenrolled" },
                        lifecycle: domain.lifecycle,
                        identity: None,
                        resident_bytes: None,
                        swap_bytes: None,
                        forecast: None,
                        emergency: false,
                        boundaries: Vec::new(),
                    });
                }
            }
            let before = serde_json::to_vec(&state)?;
            if results.try_iter().any(|r| {
                !r.ok
                    && state.active.as_ref().is_some_and(|a| {
                        a.domain.id == r.id && a.identity.invocation() == r.invocation
                    })
            }) && let Some(active) = &mut state.active
            {
                active.phase = Phase::Tripped;
            }
            let mut action = Action::None;
            if let Some(active) = &state.active {
                let shown = seen.get(&active.domain.id).and_then(|s| s.as_ref());
                let replacement = sessions.get(&active.domain.id).is_some_and(|s| {
                    RecoveryIdentity::Process(s.identity.clone()) != active.identity
                }) || shown.is_some_and(|s| {
                    !native::field(s, "InvocationID").is_empty()
                        && native::field(s, "InvocationID") != active.identity.invocation()
                });
                let empty = match &active.identity {
                    RecoveryIdentity::Process(_) => recovering.as_mut().and_then(|s| s.empty()),
                    RecoveryIdentity::Failed(identity) => {
                        native::empty_failed_slot(identity).ok().filter(|_| {
                            shown.is_some_and(|s| {
                                native::failed_matches(s, &active.domain, identity)
                            })
                        })
                    }
                }
                .filter(|_| {
                    seen_at
                        .get(&active.domain.id)
                        .is_some_and(|at| ms.saturating_sub(*at) <= 3000)
                        && shown.is_some_and(|s| {
                            matches!(native::field(s, "ActiveState"), "inactive" | "failed")
                                && native::field(s, "MainPID") == "0"
                        })
                });
                let started = replacement
                    && sessions
                        .get(&active.domain.id)
                        .is_some_and(|s| s.verify_process().is_ok());
                action = state.advance(
                    ms,
                    empty,
                    replacement,
                    healthy && !degraded,
                    started,
                    &policy,
                );
            }
            if policy.mode == Mode::Enforce && state.active.is_none() && action != Action::Finished
            {
                candidates.sort();
                if let Some((_, _, id)) = candidates.first() {
                    let source = sessions.remove(id);
                    let origin = source
                        .as_ref()
                        .map(|s| (s.domain.clone(), s.identity.clone().into()))
                        .or_else(|| {
                            failures
                                .get(id)
                                .cloned()
                                .zip(policy.domains.iter().find(|d| &d.id == id).cloned())
                                .map(|(i, d)| (d, RecoveryIdentity::Failed(i)))
                        });
                    if let Some((domain, identity)) = origin {
                        let begin = match &identity {
                            RecoveryIdentity::Process(identity) => state.begin(
                                domain.clone(),
                                identity.clone(),
                                now_ms()?,
                                ms,
                                &policy,
                            ),
                            RecoveryIdentity::Failed(identity) => state.begin_failed(
                                domain.clone(),
                                identity.clone(),
                                now_ms()?,
                                ms,
                                &policy,
                            ),
                        };
                        if begin.is_err() {
                            state.active = Some(Recovery {
                                domain,
                                identity,
                                phase: Phase::Tripped,
                            });
                        }
                        recovering = source;
                        models.remove(id);
                        // Publish inhibition before durable I/O or any signal.
                        atomic_json(
                            &runtime.join("health.json"),
                            &Health {
                                version: 1,
                                boot_id: boot.clone(),
                                observed_boot_ms: ms,
                                inhibit: true,
                                degraded,
                            },
                            0o644,
                            false,
                        )?;
                        store.save(&state)?;
                        *authority
                            .lock()
                            .map_err(|_| anyhow::anyhow!("recovery authority unavailable"))? =
                            state.active.clone();
                        if !matches!(
                            state.active.as_ref().map(|a| &a.phase),
                            Some(Phase::Tripped)
                        ) && let Some(source) = &recovering
                        {
                            let target = source.target();
                            if work.try_send(Work::Signal(target, Signal::TERM)).is_err() {
                                state
                                    .active
                                    .as_mut()
                                    .ok_or_else(|| anyhow::anyhow!("missing recovery"))?
                                    .phase = Phase::Tripped;
                            }
                            eprintln!("amc supervision: persisted intervention for {id}");
                        }
                    }
                }
            }
            // State transitions are durable before escalation or restart.
            if before != serde_json::to_vec(&state)? {
                store.save(&state)?;
            }
            *authority
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery authority unavailable"))? =
                state.active.clone();
            let submitted = match action {
                Action::Kill => recovering.as_ref().is_some_and(|s| {
                    work.try_send(Work::Signal(s.target(), Signal::KILL))
                        .is_ok()
                }),
                Action::Start => state.active.as_ref().is_some_and(|a| {
                    work.try_send(Work::Start(a.domain.clone(), a.identity.clone()))
                        .is_ok()
                }),
                Action::Finished => {
                    recovering = None;
                    true
                }
                _ => true,
            };
            if !submitted {
                if let Some(a) = &mut state.active {
                    a.phase = Phase::Tripped;
                }
                store.save(&state)?;
                *authority
                    .lock()
                    .map_err(|_| anyhow::anyhow!("recovery authority unavailable"))? =
                    state.active.clone();
            }
            if pressure || degraded || state.active.is_some() {
                healthy_since = None;
            } else {
                healthy_since.get_or_insert(ms);
            }
            let inhibit = policy.mode == Mode::Enforce
                && (pressure
                    || degraded
                    || state.active.is_some()
                    || healthy_since
                        .is_none_or(|since| ms.saturating_sub(since) < policy.cooldown_ms));
            let status = Status {
                version: 1,
                boot_id: &boot,
                observed_boot_ms: ms,
                mode: policy.mode,
                inhibit,
                degraded,
                trace_drops,
                host: &host,
                domains: &reports,
                recovery: &state,
                policy: &policy,
            };
            atomic_json(&runtime.join("status.json"), &status, 0o644, false)?;
            atomic_json(
                &runtime.join("health.json"),
                &Health {
                    version: 1,
                    boot_id: boot.clone(),
                    observed_boot_ms: ms,
                    inhibit,
                    degraded,
                },
                0o644,
                false,
            )?;
            if trace.try_send(serde_json::to_vec(&status)?).is_err() {
                trace_drops = trace_drops.saturating_add(1);
            }
            if let Ok(mut k) = known.lock() {
                k.identities = sessions
                    .iter()
                    .map(|(id, s)| (id.clone(), s.identity.clone()))
                    .collect();
                if let Some(s) = &recovering {
                    k.identities.insert(s.domain.id.clone(), s.identity.clone());
                }
                k.priority = state.active.as_ref().map(|a| a.domain.clone());
            }
            thread::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed()));
        }
        Ok(())
    })();
    if let Ok(mut a) = authority.lock() {
        *a = None;
    }
    stop.store(true, Ordering::Relaxed);
    // A gracefully stopped enforce-mode supervisor immediately inhibits, too.
    let _ = atomic_json(
        &runtime.join("health.json"),
        &Health {
            version: 1,
            boot_id: boot,
            observed_boot_ms: boot_ms()?,
            inhibit: policy.mode == Mode::Enforce,
            degraded: true,
        },
        0o644,
        false,
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_psi_uses_the_canonical_telemetry_key_and_unknown_is_not_zero() {
        let mut h = amc_telemetry::host::snapshot(
            std::path::Path::new("/nonexistent-amc-fixture"),
            Some(1),
        );
        assert_eq!(super::psi(&h), None);
        h.files.insert(
            "pressure.memory".into(),
            amc_telemetry::Measurement::known(serde_json::json!({"full":{"avg10":2.0}})),
        );
        assert_eq!(super::psi(&h), Some(0.02));
    }

    #[test]
    fn queued_actions_cannot_inherit_a_new_phase_or_run_after_trip_deadline_or_replacement() {
        let domain = Domain {
            id: "fixture".into(),
            uid: None,
            unit: "fixture.service".into(),
            lifecycle: Lifecycle::Restart,
            memory_max: 1024,
            memory_swap_max: 0,
            priority: 0,
            expected: None,
        };
        let mut active = Recovery {
            domain: domain.clone(),
            identity: crate::recovery::Identity {
                invocation: "a".repeat(32),
                cgroup: "/fixture.service".into(),
                inode: 1,
                pid: 1,
                start_ticks: 1,
            }
            .into(),
            phase: Phase::Terminating { deadline_ms: 100 },
        };
        assert!(permitted(
            Some(&active),
            &domain,
            active.identity.invocation(),
            Intervention::Term,
            99
        ));
        assert!(!permitted(
            Some(&active),
            &domain,
            active.identity.invocation(),
            Intervention::Term,
            100
        ));
        active.phase = Phase::Starting { deadline_ms: 200 };
        assert!(!permitted(
            Some(&active),
            &domain,
            active.identity.invocation(),
            Intervention::Term,
            100
        ));
        assert!(permitted(
            Some(&active),
            &domain,
            active.identity.invocation(),
            Intervention::Start,
            100
        ));
        assert!(!permitted(
            Some(&active),
            &domain,
            &"b".repeat(32),
            Intervention::Start,
            100
        ));
        active.phase = Phase::Tripped;
        assert!(!permitted(
            Some(&active),
            &domain,
            active.identity.invocation(),
            Intervention::Start,
            100
        ));
        assert!(!permitted(
            None,
            &domain,
            active.identity.invocation(),
            Intervention::Kill,
            100
        ));
    }
}
