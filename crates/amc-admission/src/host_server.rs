//! Single root-owned host broker. Socket loss never releases native capacity.
use crate::{
    host::{HostLedger, HostPolicy, Reservation, WaitReason},
    host_native,
    protocol::{read_frame, write_frame},
    store::{Store, fresh_id, private_directory},
};
use anyhow::{Result, ensure};
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Acquire {
        version: u32,
        wait_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continuation: Option<crate::continuation::ContinuationCapability>,
    },
    AcquirePool {
        version: u32,
        wait_ms: u64,
        domain: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<crate::ledger::ClientIdentity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation: Option<u64>,
    },
    Status {
        version: u32,
    },
    Prepare {
        version: u32,
        id: String,
        key: String,
        profile: String,
    },
    Preparation {
        version: u32,
        id: String,
        key: String,
    },
    CancelPreparation {
        version: u32,
        id: String,
        key: String,
    },
    Consume {
        version: u32,
        id: String,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<crate::ledger::ClientIdentity>,
    },
    RecoveryTargets {
        version: u32,
    },
    AcquireRecovery {
        version: u32,
        target: String,
    },
    AcquirePageReturn {
        version: u32,
        pid: i32,
        start_ticks: u64,
        address: u64,
        bytes: u64,
    },
    FinishPageReturn {
        version: u32,
    },
    ReleasePool {
        version: u32,
        domain: String,
        operation: u64,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    pub granted: bool,
    pub ticket: Option<String>,
    pub waiting: Option<WaitReason>,
    pub committed_bytes: u64,
    #[serde(default)]
    pub budget_bytes: u64,
    #[serde(default)]
    pub burst_budget_bytes: u64,
    #[serde(default)]
    pub burst_committed_bytes: u64,
    pub reservations: Option<Vec<Reservation>>,
    pub error: Option<String>,
    #[serde(default)]
    pub preparation: Option<crate::preparation::Preparation>,
    #[serde(default)]
    pub preparations: Option<Vec<crate::preparation::Preparation>>,
    #[serde(default)]
    pub launch_slice: Option<String>,
    #[serde(default)]
    pub swap_return_bytes: Option<u64>,
    #[serde(default)]
    pub preparation_barrier: bool,
    #[serde(default)]
    pub recovery_targets: Option<Vec<crate::recovery::RecoveryTarget>>,
    #[serde(default)]
    pub recovery_policy: Option<crate::recovery::RecoveryPolicy>,
    #[serde(default)]
    pub recovery: Option<crate::recovery::RecoveryLease>,
    #[serde(default)]
    pub continuation: Option<crate::continuation::ContinuationCapability>,
    #[serde(default)]
    pub returned_bytes: Option<u64>,
    #[serde(default)]
    pub resident_bytes: Option<u64>,
    #[serde(default)]
    pub return_demand_reduction_bytes: Option<u64>,
}

pub fn call(socket: &Path, request: &Request) -> Result<Response> {
    let metadata = fs::symlink_metadata(socket)?;
    ensure!(
        metadata.uid() == 0,
        "host admission socket must be root-owned"
    );
    let mut stream = UnixStream::connect(socket)?;
    ensure!(
        getsockopt(&stream, PeerCredentials)?.uid() == 0,
        "host admission peer must be root"
    );
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write_frame(&mut stream, request)?;
    let response: Response = read_frame(&mut stream)?;
    ensure!(response.version == 1, "unsupported host admission response");
    if let Some(error) = &response.error {
        anyhow::bail!("{error}");
    }
    Ok(response)
}

/// Protocol failures and broker refusals are terminal; only transport loss
/// can be retried with the original idempotent capability and native identity.
pub fn transient_transport(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            )
        }) || cause
            .downcast_ref::<nix::errno::Errno>()
            .is_some_and(|e| matches!(e, nix::errno::Errno::ECONNRESET | nix::errno::Errno::EPIPE))
            || matches!(
                cause.to_string().as_str(),
                "incomplete admission frame" | "admission frame deadline exceeded"
            )
    })
}

/// Helper calls this before its private coordinator's native entry handshake.
pub fn acquire(
    socket: &Path,
    wait: Duration,
    stopping: impl Fn() -> bool,
) -> Result<Option<crate::continuation::ContinuationCapability>> {
    let deadline = Instant::now() + wait;
    let mut explained = false;
    loop {
        ensure!(
            !stopping() && Instant::now() < deadline,
            "host admission cancelled or timed out"
        );
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(3_600_000) as u64;
        let reply = call(
            socket,
            &Request::Acquire {
                version: 1,
                wait_ms: left.max(1),
                continuation: std::env::var("AMC_CONTINUATION")
                    .ok()
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
            },
        )?;
        if reply.granted {
            return Ok(reply.continuation);
        }
        if !explained {
            eprintln!("waiting for host capacity: {:?}", reply.waiting);
            explained = true;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub fn verify_entry(
    socket: &Path,
    pid: i32,
    identity: &crate::ledger::Identity,
    contract: &crate::ledger::Contract,
) -> Result<()> {
    let response = call(socket, &Request::Status { version: 1 })?;
    let start = host_native::process_start(pid)?;
    ensure!(
        response
            .reservations
            .as_ref()
            .is_some_and(|entries| entries.iter().any(|r| r.granted
                && r.identity.pid == pid
                && r.identity.start_ticks == start
                && r.identity.cgroup == identity.cgroup
                && r.identity.inode == identity.inode
                && r.memory_bytes == contract.memory_max
                && r.swap_bytes == contract.memory_swap_max
                && r.burst == contract.burst
                && (!r.burst
                    || r.runtime_max_ms.is_some_and(|ms| contract
                        .runtime_max_sec
                        .is_some_and(|seconds| ms <= seconds * 1000))))),
        "native entry has no matching durable host reservation"
    );
    Ok(())
}

pub fn serve(
    policy: HostPolicy,
    socket: &Path,
    state: &Path,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    serve_supervised(policy, socket, state, None, stopping)
}

pub fn serve_supervised(
    policy: HostPolicy,
    socket: &Path,
    state: &Path,
    health_file: Option<&Path>,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    ensure!(
        nix::unistd::geteuid().is_root(),
        "host admission requires root"
    );
    policy.validate()?;
    ensure!(
        socket.is_absolute() && state.is_absolute(),
        "host admission paths must be absolute"
    );
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let (store, mut ledger) = Store::open_snapshot::<HostLedger>(state, boot.trim())?;
    ledger.swap_return_bytes = None;
    let parent = socket
        .parent()
        .ok_or_else(|| anyhow::anyhow!("socket has no parent"))?;
    // The broker's private state is distinct from its public runtime socket.
    private_directory(state)?;
    fs::create_dir_all(parent)?;
    let meta = fs::symlink_metadata(parent)?;
    ensure!(
        meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0,
        "untrusted host socket directory"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(socket.with_extension("lock"))?;
    lock.try_lock()?;
    if socket.exists() {
        ensure!(
            UnixStream::connect(socket).is_err(),
            "host socket already has a listener"
        );
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    store.save_snapshot(&ledger)?;
    let mut tick = Instant::now();
    let mut healthy_since = BTreeMap::new();
    let mut waiting = BTreeMap::new();
    while !stopping() {
        if tick.elapsed() >= Duration::from_millis(250) {
            let before = serde_json::to_vec(&ledger)?;
            for r in &mut ledger.reservations {
                if r.identity.uid == 0 {
                    let had_owners = !r.owners.is_empty();
                    r.owners
                        .retain(|o| host_native::owner_alive(o) != Some(false));
                    if had_owners && r.owners.is_empty() {
                        r.owners_finished = true;
                    }
                }
            }
            reconcile_recovery(&mut ledger);
            let capacity = observe(&mut ledger, &policy, health_file);
            waiting = ledger.advance(
                crate::clock::boot_ms()?,
                &policy,
                capacity,
                &mut healthy_since,
                host_native::ancestor_headroom,
            );
            if before != serde_json::to_vec(&ledger)? {
                store.save_snapshot(&ledger)?;
            }
            tick = Instant::now();
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_millis(250)))?;
                stream.set_write_timeout(Some(Duration::from_millis(250)))?;
                let credentials = getsockopt(&stream, PeerCredentials)?;
                let Ok(request) = read_frame::<Request>(&mut stream) else {
                    continue;
                };
                let before = serde_json::to_vec(&ledger)?;
                let result = ledger.transaction(|ledger| -> Result<Response> {
                    let mut reply = Response {
                        version: 1,
                        committed_bytes: ledger.committed(),
                        budget_bytes: policy.budget_bytes,
                        burst_budget_bytes: policy.burst.as_ref().map_or(0, |b| b.budget_bytes),
                        burst_committed_bytes: ledger.burst_committed(),
                        swap_return_bytes: ledger.swap_return_bytes,
                        preparation_barrier: ledger.preparation_barrier(),
                        ..Default::default()
                    };
                    match request {
                        Request::Status { version } => {
                            ensure!(version == 1, "unsupported host protocol");
                            populate_status(&mut reply, ledger, credentials.uid());
                        }
                        Request::Acquire {
                            version, wait_ms, ..
                        }
                        | Request::AcquirePool {
                            version, wait_ms, ..
                        } => {
                            ensure!(
                                version == 1 && (1..=3_600_000).contains(&wait_ms),
                                "invalid host request"
                            );
                            let (domain, identity, memory_bytes, swap_bytes) =
                                if let Request::AcquirePool { ref domain, .. } = request {
                                    host_native::identify_pool(
                                        credentials.pid(),
                                        credentials.uid(),
                                        domain,
                                        &policy.domains,
                                    )?
                                } else {
                                    host_native::identify(
                                        credentials.pid(),
                                        credentials.uid(),
                                        &policy.domains,
                                    )?
                                };
                            let now = crate::clock::boot_ms()?;
                            let owner_key =
                                format!("owner-{}-{}", identity.pid, identity.start_ticks);
                            let operation = root_operation(
                                &request,
                                credentials.uid(),
                                !policy.preparations.is_empty(),
                            )?;
                            let existing = ledger.reservations.iter().find(|r| {
                                r.identity.cgroup == identity.cgroup
                                    && r.identity.inode == identity.inode
                            });
                            let request_id =
                                existing.map_or_else(fresh_id, |r| Ok(r.id.clone()))?;
                            let pool_granted = existing.is_some_and(|r| r.granted);
                            let old_owner =
                                ledger.owns_pool_operation(&domain, &identity, operation);
                            let mut continuation = if credentials.uid() == 0 && old_owner {
                                None
                            } else {
                                match &request {
                                    Request::Acquire { continuation, .. } => continuation.clone(),
                                    Request::AcquirePool {
                                        origin: Some(origin),
                                        ..
                                    } => {
                                        ensure!(
                                            credentials.uid() == 0,
                                            "only root execution owners can nominate an origin"
                                        );
                                        origin_continuation(origin, ledger)?
                                    }
                                    _ => None,
                                }
                            };
                            let call_id = if credentials.uid() == 0 {
                                operation.map_or_else(
                                    || owner_key.clone(),
                                    |op| format!("{owner_key}-{op}"),
                                )
                            } else {
                                request_id.clone()
                            };
                            let continuation_parent = if let Some(capability) = continuation.take()
                            {
                                let owner_uid = ledger
                                    .continuations
                                    .iter()
                                    .find(|c| c.capability.parent == capability.parent)
                                    .map_or(credentials.uid(), |c| c.uid);
                                Some(ledger.authorize_continuation(
                                    &capability,
                                    if credentials.uid() == 0 {
                                        owner_uid
                                    } else {
                                        credentials.uid()
                                    },
                                    &domain,
                                    memory_bytes,
                                    swap_bytes,
                                    &call_id,
                                )?)
                            } else {
                                None
                            };
                            if continuation_parent.is_none()
                                && (if credentials.uid() == 0 {
                                    !old_owner
                                        && (ledger.preparation_barrier()
                                            || ledger.recovery.is_some())
                                } else {
                                    !ledger.may_join(&identity)
                                })
                                && ledger.reservations.iter().any(|r| {
                                    r.identity.cgroup == identity.cgroup
                                        && r.identity.inode == identity.inode
                                })
                            {
                                reply.waiting = Some(if ledger.recovery.is_some() {
                                    WaitReason::Recovery
                                } else {
                                    WaitReason::Preparation
                                });
                                return Ok(reply);
                            }
                            if credentials.uid() == 0
                                && pool_granted
                                && !old_owner
                                && continuation_parent.is_none()
                            {
                                let capacity = observe(ledger, &policy, health_file);
                                let d = policy
                                    .domains
                                    .iter()
                                    .find(|d| d.name == domain)
                                    .ok_or_else(|| anyhow::anyhow!("domain disappeared"))?;
                                reply.waiting = pool_join_wait(
                                    ledger,
                                    &policy,
                                    d,
                                    capacity,
                                    healthy_since.get(&domain).is_some_and(|since| {
                                        now.saturating_sub(*since) >= policy.resume_ms
                                    }),
                                );
                                if reply.waiting.is_some() {
                                    return Ok(reply);
                                }
                            }
                            let burst = policy
                                .domains
                                .iter()
                                .find(|d| d.name == domain)
                                .is_some_and(|d| d.burst);
                            let runtime_max_ms = if burst {
                                Some(host_native::burst_runtime(&identity)?)
                            } else {
                                None
                            };
                            if let Some(operation) = operation {
                                ledger.bind_pool_operation(&domain, &identity, operation)?;
                            }
                            let id = ledger.request(
                                Reservation {
                                    id: request_id.clone(),
                                    domain,
                                    identity,
                                    memory_bytes,
                                    swap_bytes,
                                    requested_ms: now,
                                    deadline_ms: now.saturating_add(wait_ms),
                                    granted: false,
                                    owners: vec![],
                                    owners_finished: false,
                                    burst,
                                    runtime_max_ms,
                                    continuation: continuation_parent.clone(),
                                },
                                &policy,
                            )?;
                            let r = ledger
                                .reservations
                                .iter()
                                .find(|r| r.id == id)
                                .ok_or_else(|| anyhow::anyhow!("missing host reservation"))?;
                            reply.granted = r.granted;
                            reply.waiting = waiting.get(&id).copied();
                            reply.ticket = Some(id);
                            if r.granted {
                                reply.continuation = ledger.continuation_capability(
                                    reply.ticket.as_deref().unwrap_or_default(),
                                    &policy,
                                )?;
                            }
                        }
                        other => {
                            handle_advance_request(
                                other,
                                credentials.pid(),
                                credentials.uid(),
                                &policy,
                                ledger,
                                health_file,
                                &mut reply,
                            )?;
                        }
                    }
                    Ok(reply)
                });
                if before != serde_json::to_vec(&ledger)? {
                    store.save_snapshot(&ledger)?;
                }
                let reply = result.unwrap_or_else(|e| Response {
                    version: 1,
                    error: Some(e.to_string()),
                    ..Default::default()
                });
                let _ = write_frame(&mut stream, &reply);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(e.into()),
        }
    }
    fs::remove_file(socket)?;
    Ok(())
}

fn public_preparation(p: &crate::preparation::Preparation) -> crate::preparation::Preparation {
    let mut public = p.clone();
    public.key.clear();
    public
}

fn root_operation(request: &Request, uid: u32, preparations: bool) -> Result<Option<u64>> {
    let operation = match request {
        Request::AcquirePool { operation, .. } => *operation,
        _ => None,
    };
    if uid == 0 && preparations && matches!(request, Request::AcquirePool { .. }) {
        ensure!(
            operation.is_some(),
            "root pool requires a finite worker operation"
        );
    }
    Ok(operation)
}

fn populate_status(reply: &mut Response, ledger: &HostLedger, uid: u32) {
    reply.reservations = Some(
        ledger
            .reservations
            .iter()
            .filter(|r| uid == 0 || r.identity.uid == uid)
            .cloned()
            .collect(),
    );
    reply.preparations = Some(
        ledger
            .preparations
            .iter()
            .filter(|p| uid == 0 || p.uid == uid)
            .map(public_preparation)
            .collect(),
    );
    // The public socket must not expose another process's identity or address
    // space through the root-owned recovery lease.
    reply.recovery = if uid == 0 {
        ledger.recovery.clone()
    } else {
        None
    };
}

fn origin_continuation(
    origin: &crate::ledger::ClientIdentity,
    ledger: &HostLedger,
) -> Result<Option<crate::continuation::ContinuationCapability>> {
    ensure!(
        host_native::process_start(origin.pid)? == origin.start_ticks,
        "Nix client origin identity changed"
    );
    let uid = fs::metadata(format!("/proc/{}", origin.pid))?.uid();
    let group = fs::read_to_string(format!("/proc/{}/cgroup", origin.pid))?;
    let group = group
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| anyhow::anyhow!("missing Nix origin placement"))?;
    let reservation = ledger.reservations.iter().find(|r| {
        r.granted
            && r.identity.uid == uid
            && (r.identity.cgroup == group || group.starts_with(&format!("{}/", r.identity.cgroup)))
    });
    let Some(r) = reservation else {
        return Ok(None);
    };
    ensure!(
        host_native::enforcement(r).is_some()
            && host_native::process_start(origin.pid)? == origin.start_ticks,
        "Nix origin native boundary changed"
    );
    let parent = r.continuation.as_deref().unwrap_or(&r.id);
    Ok(ledger
        .continuations
        .iter()
        .find(|c| c.capability.parent == parent)
        .map(|c| c.capability.clone()))
}

fn observe(
    ledger: &mut HostLedger,
    policy: &HostPolicy,
    health_file: Option<&Path>,
) -> Option<crate::host::Capacity> {
    // A nested scope can move the last process out of its previous boundary
    // between readiness and Consume. Settle positively empty native owners
    // before validating the remaining grants, rather than mistaking an already
    // collected scope for damaged enforcement until the next periodic tick.
    // Unknown cleanup and surviving descendants retain their claims.
    ledger.reconcile(host_native::empty_reservation);
    ledger.retain_pool_operations();
    if ledger.recovery.is_none()
        && policy.swap_recovery.as_ref().is_some_and(|p| {
            p.targets
                .iter()
                .any(|t| !matches!(crate::swap::device(t), Ok(Some(_))))
        })
    {
        return None;
    }
    if policy.reserve_swap_return {
        ledger.swap_return_bytes = crate::swap::return_bytes(ledger).ok();
    }
    if ledger
        .reservations
        .iter()
        .filter(|r| r.granted)
        .all(|r| host_native::enforcement(r).is_some())
    {
        host_native::capacity().ok().filter(|_| {
            health_file.is_none_or(|path| crate::health::permits(path).unwrap_or(false))
        })
    } else {
        None
    }
}

fn reconcile_recovery(ledger: &mut HostLedger) {
    let Some(lease) = &ledger.recovery else {
        return;
    };
    let reservation = Reservation {
        id: "swap-recovery".into(),
        domain: "swap-recovery".into(),
        identity: lease.identity.clone(),
        memory_bytes: lease.helper_bytes,
        swap_bytes: 0,
        requested_ms: 0,
        deadline_ms: u64::MAX,
        granted: true,
        owners: vec![crate::ledger::ClientIdentity {
            pid: lease.identity.pid,
            start_ticks: lease.identity.start_ticks,
        }],
        burst: false,
        runtime_max_ms: None,
        continuation: None,
        owners_finished: false,
    };
    if host_native::empty_reservation(&reservation) == Some(true) {
        ledger.recovery = None;
    }
}

fn handle_advance_request(
    request: Request,
    pid: i32,
    uid: u32,
    policy: &HostPolicy,
    ledger: &mut HostLedger,
    health_file: Option<&Path>,
    reply: &mut Response,
) -> Result<()> {
    use crate::preparation::PreparationPhase;
    let now = crate::clock::boot_ms()?;
    match request {
        Request::Prepare {
            version,
            id,
            key,
            profile,
        } => {
            ensure!(version == 1, "unsupported host protocol");
            ledger.prepare(id.clone(), key.clone(), uid, &profile, now, policy)?;
            preparation_reply(ledger, policy, &id, &key, uid, reply)?;
        }
        Request::Preparation { version, id, key } => {
            ensure!(version == 1, "unsupported host protocol");
            preparation_reply(ledger, policy, &id, &key, uid, reply)?;
        }
        Request::CancelPreparation { version, id, key } => {
            ensure!(version == 1, "unsupported host protocol");
            ledger.cancel_preparation(&id, &key, uid)?;
        }
        Request::Consume {
            version,
            id,
            key,
            origin,
        } => {
            ensure!(version == 1, "unsupported host protocol");
            let pid = if let Some(origin) = &origin {
                host_native::prepared_origin(origin, uid)?
            } else {
                pid
            };
            let p = ledger
                .preparations
                .iter()
                .find(|p| p.id == id && p.uid == uid && p.key == key)
                .ok_or_else(|| anyhow::anyhow!("unknown preparation capability"))?
                .clone();
            let (domain, identity, memory_bytes, swap_bytes) =
                host_native::identify_prepared(pid, uid, &policy.domains)?;
            if let Some(origin) = &origin {
                ensure!(
                    identity.start_ticks == origin.start_ticks,
                    "prepared origin replaced"
                );
                host_native::prepared_origin(origin, uid)?;
            }
            let native = Reservation {
                id: id.clone(),
                domain,
                identity,
                memory_bytes,
                swap_bytes,
                requested_ms: now,
                deadline_ms: now.saturating_add(1000),
                granted: false,
                owners: vec![],
                burst: false,
                runtime_max_ms: None,
                continuation: None,
                owners_finished: false,
            };
            if p.phase == PreparationPhase::Ready {
                ensure!(
                    policy.preparations.iter().any(|spec| spec.name == p.profile
                        && spec.domain == p.domain
                        && spec.memory_bytes == p.memory_bytes
                        && spec.swap_bytes == p.swap_bytes),
                    "preparation policy changed"
                );
                let capacity = observe(ledger, policy, health_file)
                    .ok_or_else(|| anyhow::anyhow!("fresh host evidence unavailable"))?;
                ensure!(
                    ledger.recovery.is_none()
                        && ledger.committed() <= policy.budget_bytes
                        && (0.0..policy.max_memory_full_psi).contains(&capacity.memory_full_psi)
                        && capacity.swap_free_bytes
                            >= policy
                                .swap_reserve_bytes
                                .saturating_add(ledger.swap_committed())
                        && capacity.available_bytes
                            >= policy
                                .reserve_bytes
                                .saturating_add(ledger.committed())
                                .saturating_add(ledger.return_claim(policy)),
                    "prepared capacity is no longer available"
                );
                let d = policy
                    .domains
                    .iter()
                    .find(|d| d.name == native.domain)
                    .ok_or_else(|| anyhow::anyhow!("domain disappeared"))?;
                ensure!(
                    capacity.available_bytes >= d.min_available_bytes
                        && (d.io_pressure == crate::host::IoPressure::Diagnostic
                            || (0.0..policy.max_io_full_psi).contains(&capacity.io_full_psi))
                        && ledger
                            .native_claims(policy, Some(&native))
                            .and_then(|claims| host_native::ancestor_headroom(&native, &claims))
                            .is_some_and(|bytes| bytes >= memory_bytes),
                    "prepared ancestor or pressure headroom changed"
                );
            }
            ledger.consume(&id, &key, uid, native, now)?;
            reply.granted = true;
            reply.ticket = Some(id);
        }
        Request::RecoveryTargets { version } => {
            ensure!(version == 1 && uid == 0, "recovery requires root");
            reply.recovery_targets = Some(
                policy
                    .swap_recovery
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("swap recovery disabled"))?
                    .targets
                    .clone(),
            );
            reply.recovery_policy = policy.swap_recovery.clone();
        }
        Request::AcquireRecovery { version, target } => {
            ensure!(version == 1 && uid == 0, "recovery requires root");
            let spec = policy
                .swap_recovery
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("swap recovery disabled"))?;
            let target = spec
                .targets
                .iter()
                .find(|t| t.name == target)
                .ok_or_else(|| anyhow::anyhow!("unknown recovery target"))?;
            let identity = crate::swap::helper(pid, spec)?;
            if let Some(lease) = &ledger.recovery {
                ensure!(
                    lease.identity == identity
                        && matches!(&lease.action, crate::recovery::RecoveryAction::Device {target:t,..} if t.name == target.name),
                    "another native recovery owns capacity"
                );
                reply.granted = true;
                reply.recovery = ledger.recovery.clone();
                return Ok(());
            }
            let device = crate::swap::device(target)?
                .ok_or_else(|| anyhow::anyhow!("swap target is inactive; restore first"))?;
            if device.used_bytes < spec.minimum_bytes {
                reply.waiting = Some(WaitReason::SwapReturn);
                return Ok(());
            }
            let capacity = observe(ledger, policy, health_file);
            let native_safe = crate::swap::native_return_safe(ledger, policy)?;
            reply.waiting = ledger.recovery_wait(
                policy,
                capacity,
                device.size_bytes,
                spec.helper_bytes,
                native_safe,
            );
            if reply.waiting.is_none() {
                let mut target = target.clone();
                target.path = device.path.to_string_lossy().into_owned();
                ledger.recovery = Some(crate::recovery::RecoveryLease {
                    identity,
                    action: crate::recovery::RecoveryAction::Device {
                        target,
                        before_used_bytes: device.used_bytes,
                    },
                    return_bytes: device.size_bytes,
                    helper_bytes: spec.helper_bytes,
                });
                reply.granted = true;
                reply.recovery = ledger.recovery.clone();
            }
        }
        Request::AcquirePageReturn {
            version,
            pid: target_pid,
            start_ticks,
            address,
            bytes,
        } => {
            ensure!(version == 1 && uid == 0, "page return requires root");
            let spec = policy
                .swap_recovery
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("swap recovery disabled"))?;
            let helper = crate::swap::helper(pid, spec)?;
            let target = crate::page_return::identity(target_pid, start_ticks, spec)?;
            let page = crate::page_return::page_size()?;
            ensure!(
                bytes > 0
                    && bytes <= spec.batch_bytes
                    && bytes.is_multiple_of(page)
                    && address.is_multiple_of(page)
                    && address.checked_add(bytes).is_some(),
                "page return exceeds bounded batch"
            );
            if let Some(lease) = &ledger.recovery {
                ensure!(
                    lease.identity == helper,
                    "another native recovery owns capacity"
                );
                if !matches!(
                    &lease.action,
                    crate::recovery::RecoveryAction::Pages { settled: true, .. }
                ) {
                    ensure!(
                        matches!(&lease.action,crate::recovery::RecoveryAction::Pages {target:t,address:a,bytes:b,..}
                        if t == &target && *a == address && *b == bytes),
                        "previous page return is not settled"
                    );
                    reply.granted = true;
                    reply.recovery = ledger.recovery.clone();
                    return Ok(());
                }
            }
            let usage = crate::page_return::target_usage(&target)?;
            let before_swap_bytes = usage.used_bytes;
            if before_swap_bytes == 0 {
                reply.waiting = Some(WaitReason::SwapReturn);
                return Ok(());
            }
            if !crate::page_return::range_swapped(
                &crate::page_return::open_pagemap(&target)?,
                address,
                bytes,
            )? {
                reply.waiting = Some(WaitReason::SwapReturn);
                return Ok(());
            }
            let capacity = observe(ledger, policy, health_file);
            let claims = ledger
                .native_claims(policy, None)
                .ok_or_else(|| anyhow::anyhow!("completion native backing unavailable"))?;
            let native_safe = crate::page_return::native_headroom(&target, &claims, bytes)?;
            let mut eligibility = ledger.clone();
            eligibility.recovery = None;
            reply.waiting = eligibility.page_return_wait(
                policy,
                capacity,
                bytes,
                spec.helper_bytes,
                native_safe,
            );
            if reply.waiting.is_none() {
                ledger.recovery = Some(crate::recovery::RecoveryLease {
                    identity: helper,
                    action: crate::recovery::RecoveryAction::Pages {
                        target,
                        address,
                        bytes,
                        before_swap_bytes,
                        before_return_bytes: Some(usage.return_bytes()),
                        settled: false,
                    },
                    return_bytes: bytes,
                    helper_bytes: spec.helper_bytes,
                });
                reply.granted = true;
                reply.recovery = ledger.recovery.clone();
            }
        }
        Request::FinishPageReturn { version } => {
            ensure!(version == 1 && uid == 0, "page return requires root");
            let spec = policy
                .swap_recovery
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("swap recovery disabled"))?;
            let helper = crate::swap::helper(pid, spec)?;
            let lease = ledger
                .recovery
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no native page return"))?;
            ensure!(lease.identity == helper, "page return helper changed");
            let crate::recovery::RecoveryAction::Pages {
                target,
                before_swap_bytes,
                before_return_bytes,
                bytes,
                address,
                settled,
                ..
            } = &mut lease.action
            else {
                anyhow::bail!("native recovery is not a page return");
            };
            ensure!(
                crate::page_return::identity(target.pid, target.start_ticks, spec)? == *target,
                "page return target changed before settlement"
            );
            let after = crate::page_return::target_usage(target)?;
            reply.returned_bytes = Some(before_swap_bytes.saturating_sub(after.used_bytes));
            reply.return_demand_reduction_bytes =
                before_return_bytes.map(|before| before.saturating_sub(after.return_bytes()));
            reply.resident_bytes = Some(
                if crate::page_return::range_resident(
                    &crate::page_return::open_pagemap(target)?,
                    *address,
                    *bytes,
                )? {
                    *bytes
                } else {
                    0
                },
            );
            if reply.resident_bytes != Some(*bytes) {
                reply.waiting = Some(WaitReason::Unknown);
                return Ok(());
            }
            *settled = true;
            lease.return_bytes = 0;
            reply.granted = true;
        }
        Request::ReleasePool {
            version,
            domain,
            operation,
        } => {
            ensure!(version == 1 && uid == 0, "pool release requires root");
            let (_, identity, _, _) =
                host_native::identify_pool(pid, uid, &domain, &policy.domains)?;
            ledger.release_pool_operation(&domain, &identity, operation);
            reply.granted = true;
        }
        _ => anyhow::bail!("unexpected advance request"),
    }
    reply.committed_bytes = ledger.committed();
    reply.swap_return_bytes = ledger.swap_return_bytes;
    reply.preparation_barrier = ledger.preparation_barrier();
    Ok(())
}

fn pool_join_wait(
    ledger: &HostLedger,
    policy: &HostPolicy,
    domain: &crate::host::Domain,
    capacity: Option<crate::host::Capacity>,
    healthy: bool,
) -> Option<WaitReason> {
    let Some(c) = capacity else {
        return Some(WaitReason::Unknown);
    };
    if !healthy
        || !(0.0..policy.max_memory_full_psi).contains(&c.memory_full_psi)
        || (domain.io_pressure == crate::host::IoPressure::Enforce
            && !(0.0..policy.max_io_full_psi).contains(&c.io_full_psi))
    {
        return Some(WaitReason::Pressure);
    }
    if c.swap_free_bytes
        < policy
            .swap_reserve_bytes
            .saturating_add(ledger.swap_committed())
    {
        return Some(WaitReason::SwapHeadroom);
    }
    if ledger.committed() > policy.budget_bytes {
        return Some(WaitReason::Budget);
    }
    if c.available_bytes
        < domain.min_available_bytes.max(
            policy
                .reserve_bytes
                .saturating_add(ledger.committed())
                .saturating_add(ledger.return_claim(policy)),
        )
    {
        return Some(WaitReason::SwapReturn);
    }
    None
}

fn preparation_reply(
    ledger: &HostLedger,
    policy: &HostPolicy,
    id: &str,
    key: &str,
    uid: u32,
    reply: &mut Response,
) -> Result<()> {
    let p = ledger
        .preparations
        .iter()
        .find(|p| p.id == id && p.uid == uid && p.key == key)
        .ok_or_else(|| anyhow::anyhow!("unknown or expired preparation capability"))?;
    reply.granted = p.phase == crate::preparation::PreparationPhase::Ready;
    reply.waiting = p.waiting;
    reply.ticket = Some(p.id.clone());
    reply.preparation = Some(public_preparation(p));
    reply.launch_slice = policy
        .domains
        .iter()
        .find(|d| d.name == p.domain)
        .and_then(|d| Path::new(&d.cgroup).file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_root_services_do_not_require_an_aggregate_worker_serial() {
        let ordinary: Request = serde_json::from_value(serde_json::json!({
            "op":"acquire", "version":1, "wait_ms":1000
        }))
        .unwrap();
        assert_eq!(root_operation(&ordinary, 0, true).unwrap(), None);
        let pool: Request = serde_json::from_value(serde_json::json!({
            "op":"acquire_pool", "version":1, "wait_ms":1000, "domain":"builders"
        }))
        .unwrap();
        assert!(root_operation(&pool, 0, true).is_err());
        assert_eq!(root_operation(&pool, 0, false).unwrap(), None);
        let worker: Request = serde_json::from_value(serde_json::json!({
            "op":"acquire_pool", "version":1, "wait_ms":1000,
            "domain":"builders", "operation":7
        }))
        .unwrap();
        assert_eq!(root_operation(&worker, 0, true).unwrap(), Some(7));
    }

    #[test]
    fn page_return_target_identity_and_addresses_are_only_visible_to_root() {
        let mut ledger = HostLedger::new("boot".into());
        ledger.recovery = Some(
            serde_json::from_value(serde_json::json!({
                "identity":{"uid":0,"pid":42,"start_ticks":7,"inode":1,
                    "cgroup":"/recovery"},
                "return_bytes":4096,"helper_bytes":1048576,
                "action":{"kind":"pages", "target":{"uid":2000,"pid":99,
                    "start_ticks":11,"inode":2,"cgroup":"/private-target"},
                    "address":305418240,"bytes":4096,"before_swap_bytes":4096,
                    "before_return_bytes":4096,"settled":false}
            }))
            .unwrap(),
        );
        let mut reply = Response::default();
        populate_status(&mut reply, &ledger, 0);
        assert!(
            serde_json::to_string(&reply)
                .unwrap()
                .contains("private-target")
        );
        // Reusing a root response must clear every recovery identity field.
        for uid in [1000, 2000] {
            populate_status(&mut reply, &ledger, uid);
            assert!(reply.recovery.is_none());
            let wire = serde_json::to_string(&reply).unwrap();
            assert!(!wire.contains("private-target") && !wire.contains("305418240"));
        }
    }
}
