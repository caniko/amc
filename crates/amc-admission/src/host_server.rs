//! Single root-owned host broker. Socket loss never releases native capacity.
use crate::{
    host::{HostLedger, HostPolicy, Identity, Reservation, WaitReason},
    host_native,
    protocol::{read_frame_cancellable, write_frame_cancellable},
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

pub(crate) mod preparation_auth;
pub use preparation_auth::PreparationAuthorization;
mod recovery_manager;
mod transport;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    NamespaceRunner {
        version: u32,
    },
    Acquire {
        version: u32,
        /// Omission means patient resource waiting; legacy peers reject it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_ms: Option<u64>,
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
    AuthorizePreparation {
        version: u32,
        id: String,
        profile: String,
        origin: crate::ledger::ClientIdentity,
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
    pub namespace_runner_reserved_bytes: u64,
    #[serde(default)]
    pub burst_budget_bytes: u64,
    #[serde(default)]
    pub burst_committed_bytes: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub protected_pools: BTreeMap<String, crate::protected_pool::Backing>,
    pub reservations: Option<Vec<Reservation>>,
    pub error: Option<String>,
    #[serde(default)]
    pub preparation: Option<crate::preparation::Preparation>,
    /// Revealed only after a successful native Consume, never in status or Ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation_consume_proof: Option<String>,
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
    exchange(connect(socket)?, request)
}

fn connect(socket: &Path) -> Result<UnixStream> {
    let metadata = fs::symlink_metadata(socket)?;
    ensure!(
        metadata.uid() == 0,
        "host admission socket must be root-owned"
    );
    let stream = UnixStream::connect(socket)?;
    ensure!(
        getsockopt(&stream, PeerCredentials)?.uid() == 0,
        "host admission peer must be root"
    );
    Ok(stream)
}

fn exchange(stream: UnixStream, request: &Request) -> Result<Response> {
    exchange_cancellable(stream, request, None)
}

fn exchange_cancellable(
    mut stream: UnixStream,
    request: &Request,
    control: Option<(Instant, &dyn Fn() -> bool)>,
) -> Result<Response> {
    let timeout = match request {
        Request::AcquirePageReturn { .. } | Request::AcquireRecovery { .. } => {
            Duration::from_secs(90)
        }
        _ => Duration::from_secs(2),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write_frame_cancellable(&mut stream, request, control)?;
    let response: Response = read_frame_cancellable(&mut stream, control)?;
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
    wait: Option<Duration>,
    stopping: impl Fn() -> bool,
    mut waiting: impl FnMut() -> Result<()>,
) -> Result<Option<crate::continuation::ContinuationCapability>> {
    let began = Instant::now();
    let deadline = wait.map(|wait| Instant::now() + wait);
    let mut explained = false;
    loop {
        ensure!(
            !stopping() && deadline.is_none_or(|deadline| Instant::now() < deadline),
            "host admission cancelled or timed out"
        );
        waiting()?;
        let left = deadline.map(|deadline| {
            (deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(3_600_000) as u64)
                .max(1)
        });
        // A resource wait cannot inherit a fresh transport budget for each frame.
        // A grant whose acknowledgement is lost remains held until native cleanup.
        let rpc_deadline = deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(2))
            .min(Instant::now() + Duration::from_secs(2));
        let reply = exchange_cancellable(
            connect(socket)?,
            &Request::Acquire {
                version: 1,
                wait_ms: left,
                continuation: std::env::var("AMC_CONTINUATION")
                    .ok()
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
            },
            Some((rpc_deadline, &stopping)),
        )?;
        ensure!(
            !stopping() && deadline.is_none_or(|deadline| Instant::now() < deadline),
            "host admission cancelled or timed out"
        );
        if reply.granted {
            return Ok(reply.continuation);
        }
        if !explained {
            eprintln!("waiting for host capacity: {:?}", reply.waiting);
            explained = true;
        }
        thread::sleep(poll_interval(began));
    }
}

/// Short grants usually settle within one broker turn and a manager probe;
/// poll those quickly, then fall back to the idle interval for long waits.
pub fn poll_interval(began: Instant) -> Duration {
    Duration::from_millis(if began.elapsed() < Duration::from_secs(1) {
        25
    } else {
        250
    })
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
                && (!(r.burst || contract.protected_pool.is_some())
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
    reconcile_startup(
        &mut ledger,
        &policy,
        host_native::empty_reservation,
        crate::protected_pool::empty,
    )?;
    crate::namespace_runner::discover(&mut ledger, &policy)?;
    crate::namespace_runner::reconcile(&mut ledger, &policy);
    reconcile_preparation_helpers(&mut ledger, &policy);
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
    // A request that changed the ledger is advanced on the next turn rather
    // than after the idle interval, so a grantable request is not delayed.
    let mut due = false;
    let mut healthy_since = BTreeMap::new();
    let mut waiting = BTreeMap::new();
    let mut inventory: Option<PendingInventory> = None;
    let mut burst_manager = crate::burst_manager::Cache::default();
    // Separate bounded queues/workers: bulk/burst probes cannot consume the
    // protected lane's native deadline observations.
    let mut protected_manager = crate::burst_manager::Cache::default();
    let mut recovery_manager = recovery_manager::Cache::default();
    let mut connections = transport::Connections::default();
    while !stopping() {
        burst_manager.refresh(&ledger)?;
        protected_manager.refresh_protected(&ledger)?;
        recovery_manager.refresh(ledger.recovery.as_ref())?;
        let mut burst_evidence = burst_manager.snapshot();
        burst_evidence.extend(protected_manager.snapshot());
        if due || tick.elapsed() >= Duration::from_millis(250) {
            due = false;
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
            reconcile_recovery(&mut ledger, &recovery_manager);
            reconcile_preparation_helpers(&mut ledger, &policy);
            let capacity = observe(&mut ledger, &policy, health_file, &burst_evidence);
            waiting = ledger.advance_with_burst_runtime(
                crate::clock::boot_ms()?,
                &policy,
                capacity,
                &mut healthy_since,
                host_native::ancestor_headroom,
                |identity| burst_evidence.runtime(identity),
            );
            if before != serde_json::to_vec(&ledger)? {
                store.save_snapshot(&ledger)?;
            }
            tick = Instant::now();
        }
        if let Some(task) = take_finished_inventory(&mut inventory) {
            let before = serde_json::to_vec(&ledger)?;
            let stream = task.stream;
            let result = task
                .worker
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("native inventory worker failed")))
                .and_then(|(reply, _native_demand, completed)| {
                    let validated_lease = reply.recovery.clone();
                    InventoryReplay {
                        owner: &task.owner,
                        backing: &task.backing,
                        completed,
                        stream: &stream,
                    }
                    .apply(&mut ledger, &policy, reply, |ledger| {
                        let mut fresh = Response {
                            version: 1,
                            ..Default::default()
                        };
                        handle_advance_request(
                            task.request,
                            task.owner.pid,
                            0,
                            &policy,
                            ledger,
                            (
                                health_file,
                                Some(InventoryProof {
                                    safe: true,
                                    validated_lease: validated_lease.as_ref(),
                                }),
                                &burst_evidence,
                            ),
                            &mut fresh,
                        )?;
                        Ok(fresh)
                    })
                });
            if before != serde_json::to_vec(&ledger)? {
                store.save_snapshot(&ledger)?;
            }
            let reply = result.unwrap_or_else(|e| Response {
                version: 1,
                error: Some(e.to_string()),
                ..Default::default()
            });
            let _ = connections.respond(stream, 0, &reply);
        }
        match connections.next(&listener)? {
            Some(transport::Incoming {
                stream,
                credentials,
                request,
            }) => {
                if credentials.uid() == 0
                    && matches!(
                        &request,
                        Request::AcquirePageReturn { .. } | Request::AcquireRecovery { .. }
                    )
                {
                    if inventory.is_some() {
                        let _ = connections.respond(
                            stream,
                            credentials.uid(),
                            &Response {
                                version: 1,
                                waiting: Some(WaitReason::Recovery),
                                ..Default::default()
                            },
                        );
                    } else {
                        match PendingInventory::start(
                            stream.try_clone()?,
                            request,
                            credentials.pid(),
                            &policy,
                            &ledger,
                            health_file,
                            &burst_evidence,
                        ) {
                            Ok(task) => inventory = Some(task),
                            Err(e) => {
                                let _ = connections.respond(
                                    stream,
                                    credentials.uid(),
                                    &Response {
                                        version: 1,
                                        error: Some(e.to_string()),
                                        ..Default::default()
                                    },
                                );
                            }
                        }
                    }
                    continue;
                }
                let before = serde_json::to_vec(&ledger)?;
                let result = ledger.transaction(|ledger| -> Result<Response> {
                    let mut reply = Response {
                        version: 1,
                        committed_bytes: ledger.committed(),
                        budget_bytes: policy.budget_bytes,
                        namespace_runner_reserved_bytes: crate::namespace_runner::retained_claims(
                            ledger, &policy,
                        )
                        .iter()
                        .map(|r| r.memory_bytes)
                        .sum(),
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
                        Request::Acquire { version, .. } | Request::AcquirePool { version, .. } => {
                            let wait_ms = match &request {
                                Request::Acquire { wait_ms, .. } => *wait_ms,
                                Request::AcquirePool { wait_ms, .. } => Some(*wait_ms),
                                _ => unreachable!(),
                            };
                            ensure!(
                                version == 1
                                    && wait_ms.is_none_or(|ms| (1..=3_600_000).contains(&ms)),
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
                                    host_native::identify_acquire(
                                        credentials.pid(),
                                        credentials.uid(),
                                        &policy.domains,
                                        &ledger.reservations,
                                    )?
                                };
                            // An exact ordinary grant replay is already backed,
                            // even after removal or reduction of its policy domain.
                            // Pool joins still use current enrollment and ownership.
                            if matches!(request, Request::Acquire { .. })
                                && let Some(grant) = ledger
                                    .reservations
                                    .iter()
                                    .find(|r| r.granted && r.identity == identity)
                            {
                                if grant.runtime_max_ms.is_some() {
                                    let manager = if ledger.protected_owner(grant).is_some() {
                                        &mut protected_manager
                                    } else {
                                        &mut burst_manager
                                    };
                                    manager.register(&identity)?;
                                    if manager.runtime(&identity) != grant.runtime_max_ms {
                                        reply.waiting = Some(WaitReason::Unknown);
                                        return Ok(reply);
                                    }
                                }
                                let id = grant.id.clone();
                                reply.granted = true;
                                reply.ticket = Some(id.clone());
                                reply.continuation =
                                    ledger.continuation_capability(&id, &policy)?;
                                return Ok(reply);
                            }
                            let now = crate::clock::boot_ms()?;
                            let burst = policy.domains.iter().any(|d| d.name == domain && d.burst);
                            let short_call = burst
                                || policy
                                    .domains
                                    .iter()
                                    .any(|d| d.name == domain && d.protected_pool.is_some());
                            let runtime_max_ms = if short_call {
                                let manager = if burst {
                                    &mut burst_manager
                                } else {
                                    &mut protected_manager
                                };
                                manager.register(&identity)?;
                                let Some(runtime) = manager.runtime(&identity) else {
                                    reply.waiting = Some(WaitReason::Unknown);
                                    return Ok(reply);
                                };
                                Some(runtime)
                            } else {
                                None
                            };
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
                                        origin_continuation(origin, ledger, &burst_evidence)?
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
                            // A fresh ordinary serial is waiting behind this
                            // drain/recovery, even if the surviving pool still
                            // belongs to a completed continuation parent. Check
                            // its barrier before incompatible-parent lane lookup.
                            if credentials.uid() == 0
                                && !old_owner
                                && continuation.is_none()
                                && existing.is_some()
                                && (ledger.preparation_barrier() || ledger.recovery.is_some())
                            {
                                reply.waiting = Some(if ledger.recovery.is_some() {
                                    WaitReason::Recovery
                                } else {
                                    WaitReason::Preparation
                                });
                                return Ok(reply);
                            }
                            let CompletionAdmission::Parent(continuation_parent) =
                                acquire_completion(
                                    ledger,
                                    &identity,
                                    &domain,
                                    operation,
                                    (memory_bytes, swap_bytes),
                                    &call_id,
                                    continuation.take(),
                                )?
                            else {
                                reply.waiting = Some(WaitReason::ContinuationSlot);
                                return Ok(reply);
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
                                let capacity =
                                    observe(ledger, &policy, health_file, &burst_evidence);
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
                                    // Any enrolled user can reach this socket, so an
                                    // omitted (patient) wait still expires after the
                                    // legacy hour; the helper then re-requests.
                                    deadline_ms: now.saturating_add(wait_ms.unwrap_or(3_600_000)),
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
                                (health_file, None, &burst_evidence),
                                &mut reply,
                            )?;
                        }
                    }
                    Ok(reply)
                });
                if before != serde_json::to_vec(&ledger)? {
                    store.save_snapshot(&ledger)?;
                    due = true;
                }
                let reply = result.unwrap_or_else(|e| Response {
                    version: 1,
                    error: Some(e.to_string()),
                    ..Default::default()
                });
                let _ = connections.respond(stream, credentials.uid(), &reply);
            }
            None => {
                if !connections.progress() {
                    connections.wait(&listener, inventory.as_ref().map(|task| &task.completion))?;
                }
            }
        }
    }
    fs::remove_file(socket)?;
    Ok(())
}

// One bounded inventory may run while the accept loop continues serving status,
// existing-work completion and new waiters. It owns neither the store nor the
// live ledger; only the main loop may persist a freshly revalidated lease.
struct PendingInventory {
    stream: UnixStream,
    completion: UnixStream,
    request: Request,
    owner: crate::ledger::ClientIdentity,
    backing: Vec<u8>,
    worker: thread::JoinHandle<
        Result<(
            Response,
            Option<crate::swap::NativeReturnInventory>,
            Instant,
        )>,
    >,
}

fn take_finished_inventory(pending: &mut Option<PendingInventory>) -> Option<PendingInventory> {
    if pending
        .as_ref()
        .is_some_and(|task| task.worker.is_finished())
    {
        pending.take()
    } else {
        None
    }
}

fn inventory_current(
    owner: &crate::ledger::ClientIdentity,
    backing: &[u8],
    completed: Instant,
    stream: &UnixStream,
    ledger: &HostLedger,
    policy: &HostPolicy,
) -> Result<bool> {
    Ok(completed.elapsed() <= Duration::from_secs(1)
        && host_native::owner_alive(owner) == Some(true)
        && inventory_peer_connected(stream)
        && inventory_backing(ledger, policy)? == backing)
}

struct InventoryReplay<'a> {
    owner: &'a crate::ledger::ClientIdentity,
    backing: &'a [u8],
    completed: Instant,
    stream: &'a UnixStream,
}

fn finish_inventory(
    manager: impl FnOnce() -> Result<Response>,
    native: impl FnOnce(Response) -> Result<Response>,
) -> Result<(Response, Instant)> {
    let response = native(manager()?)?;
    Ok((response, Instant::now()))
}

#[derive(Clone, Copy)]
struct InventoryProof<'a> {
    safe: bool,
    // Present only for the main loop's replay of a completed off-loop result.
    // The worker checked the manager before revalidating the full frontier.
    validated_lease: Option<&'a crate::recovery::RecoveryLease>,
}

fn recovery_invocation(identity: &Identity, proof: Option<InventoryProof<'_>>) -> Result<String> {
    if let Some(lease) = proof.and_then(|p| p.validated_lease) {
        ensure!(
            lease.identity == *identity,
            "recovery helper changed after native validation"
        );
        return lease.invocation_id.clone().ok_or_else(|| {
            anyhow::anyhow!("completed inventory has no manager invocation identity")
        });
    }
    host_native::recovery_invocation(identity)
}

impl InventoryReplay<'_> {
    fn apply(
        self,
        ledger: &mut HostLedger,
        policy: &HostPolicy,
        mut scanned: Response,
        replay: impl FnOnce(&mut HostLedger) -> Result<Response>,
    ) -> Result<Response> {
        if !scanned.granted {
            return Ok(scanned);
        }
        // Results authorize one replay of the same authenticated request, never
        // replacement of the current ledger with the worker's scan clone.
        if !inventory_current(
            self.owner,
            self.backing,
            self.completed,
            self.stream,
            ledger,
            policy,
        )? {
            scanned.granted = false;
            scanned.recovery = None;
            scanned.waiting = Some(WaitReason::Unknown);
            return Ok(scanned);
        }
        ledger.transaction(|ledger| {
            let fresh = replay(ledger)?;
            if fresh.granted {
                ensure!(
                    self.completed.elapsed() <= Duration::from_secs(1)
                        && host_native::owner_alive(self.owner) == Some(true)
                        && inventory_peer_connected(self.stream),
                    "native recovery replay expired before persistence"
                );
                ensure!(
                    inventory_recovery_matches(&fresh.recovery, &scanned.recovery)?,
                    "recovery identity or demand changed during inventory"
                );
            }
            Ok(fresh)
        })
    }
}

fn inventory_recovery_matches(
    fresh: &Option<crate::recovery::RecoveryLease>,
    scanned: &Option<crate::recovery::RecoveryLease>,
) -> Result<bool> {
    use crate::recovery::RecoveryAction;
    let (Some(fresh), Some(scanned)) = (fresh, scanned) else {
        return Ok(false);
    };
    // Counter snapshots are settlement telemetry, not authorization. The
    // replay already checks fresh PTE demand and native target/reader backing;
    // the inventory must still authorize exactly the same helper and batch.
    let action_matches = match (&fresh.action, &scanned.action) {
        (
            RecoveryAction::Device {
                target: a,
                native_target: native_a,
                ..
            },
            RecoveryAction::Device {
                target: b,
                native_target: native_b,
                ..
            },
        ) => a == b && native_a.is_some() && native_a == native_b,
        (
            RecoveryAction::Pages {
                target: a,
                address: aa,
                bytes: ab,
                settled: settled_a,
                guards: guards_a,
                ..
            },
            RecoveryAction::Pages {
                target: b,
                address: ba,
                bytes: bb,
                settled: settled_b,
                guards: guards_b,
                ..
            },
        ) => a == b && aa == ba && ab == bb && settled_a == settled_b && guards_a == guards_b,
        _ => false,
    };
    Ok(action_matches
        && fresh.identity == scanned.identity
        && fresh.invocation_id == scanned.invocation_id
        && fresh.return_bytes == scanned.return_bytes
        && fresh.helper_bytes == scanned.helper_bytes)
}

fn inventory_backing(ledger: &HostLedger, policy: &HostPolicy) -> Result<Vec<u8>> {
    let claims = ledger
        .native_claims(policy, None)
        .ok_or_else(|| anyhow::anyhow!("native inventory backing unavailable"))?;
    // Queue churn owns no native capacity and must not starve recovery.
    // Keep the same effective fields used by the native headroom scans.
    let mut backing: Vec<_> = claims
        .iter()
        .filter(|r| r.granted)
        .map(|r| (&r.id, &r.identity, r.memory_bytes, r.swap_bytes))
        .collect();
    backing.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cgroup.cmp(&b.1.cgroup)));
    Ok(serde_json::to_vec(&(backing, &ledger.recovery))?)
}

fn inventory_peer_connected(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    matches!(
        nix::sys::socket::recv(
            stream.as_raw_fd(),
            &mut [0u8; 1],
            nix::sys::socket::MsgFlags::MSG_PEEK | nix::sys::socket::MsgFlags::MSG_DONTWAIT
        ),
        Err(nix::errno::Errno::EAGAIN)
    )
}

impl PendingInventory {
    fn start(
        stream: UnixStream,
        request: Request,
        pid: i32,
        policy: &HostPolicy,
        ledger: &HostLedger,
        health_file: Option<&Path>,
        burst_evidence: &crate::burst_manager::Snapshot,
    ) -> Result<Self> {
        let owner = crate::ledger::ClientIdentity {
            pid,
            start_ticks: host_native::process_start(pid)?,
        };
        let backing = inventory_backing(ledger, policy)?;
        let scan_request = request.clone();
        let scan_policy = policy.clone();
        let mut scan_ledger = ledger.clone();
        let health_file = health_file.map(Path::to_owned);
        let burst_evidence = burst_evidence.clone();
        let (completion, notifier) = UnixStream::pair()?;
        let worker = thread::Builder::new()
            .name("amc-native-inventory".into())
            .spawn(move || {
                // Closing this private peer wakes the broker even on error or
                // panic. Readiness is notification, never grant evidence.
                let _notifier = notifier;
                let (inventory_proof, native_demand) =
                    if matches!(&scan_request, Request::AcquireRecovery { .. }) {
                        let spec = scan_policy
                            .swap_recovery
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("swap recovery disabled"))?;
                        let helper = crate::swap::helper(pid, spec)?;
                        let (safe, proof) = crate::swap::native_return_inventory(
                            &scan_ledger,
                            &scan_policy,
                            &helper,
                            spec.helper_bytes,
                        )?;
                        (Some(safe), Some(proof))
                    } else if let Request::AcquirePageReturn {
                        pid: target_pid,
                        start_ticks,
                        bytes,
                        ..
                    } = &scan_request
                    {
                        let spec = scan_policy
                            .swap_recovery
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("swap recovery disabled"))?;
                        let helper = crate::swap::helper(pid, spec)?;
                        let target = crate::page_return::identity(*target_pid, *start_ticks, spec)?;
                        let (safe, proof) = crate::swap::page_return_inventory(
                            &scan_ledger,
                            &scan_policy,
                            &target,
                            &helper,
                            *bytes,
                            spec.helper_bytes,
                        )?;
                        (Some(safe), Some(proof))
                    } else {
                        (None, None)
                    };
                let (reply, completed) = finish_inventory(
                    || {
                        let mut reply = Response {
                            version: 1,
                            ..Default::default()
                        };
                        handle_advance_request(
                            scan_request,
                            pid,
                            0,
                            &scan_policy,
                            &mut scan_ledger,
                            (
                                health_file.as_deref(),
                                inventory_proof.map(|safe| InventoryProof {
                                    safe,
                                    validated_lease: None,
                                }),
                                &burst_evidence,
                            ),
                            &mut reply,
                        )?;
                        Ok(reply)
                    },
                    |reply| {
                        if reply.granted {
                            ensure!(
                                native_demand.as_ref().map_or(
                                    Ok(true),
                                    crate::swap::NativeReturnInventory::current
                                )?,
                                "native recovery demand or frontier changed during inventory replay"
                            );
                        }
                        Ok(reply)
                    },
                )?;
                Ok((reply, native_demand, completed))
            })?;
        Ok(Self {
            stream,
            completion,
            request,
            owner,
            backing,
            worker,
        })
    }
}

fn public_preparation(p: &crate::preparation::Preparation) -> crate::preparation::Preparation {
    let mut public = p.clone();
    public.key.clear();
    public.consume_proof = None;
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
    reply.protected_pools = ledger
        .protected_pools
        .iter()
        .filter(|(_, backing)| uid == 0 || backing.uid == uid)
        .map(|(name, backing)| (name.clone(), backing.clone()))
        .collect();
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
    burst_evidence: &crate::burst_manager::Snapshot,
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
        cached_enforcement(r, burst_evidence)
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

fn reconcile_startup(
    ledger: &mut HostLedger,
    policy: &HostPolicy,
    empty_reservation: impl FnMut(&Reservation) -> Option<bool>,
    empty_pool: impl FnMut(&crate::protected_pool::Backing) -> Option<bool>,
) -> Result<()> {
    ledger.reconcile(empty_reservation);
    ledger.reconcile_continuation_policy(policy);
    ledger.sync_protected_pools(policy, empty_pool)
}

fn observe(
    ledger: &mut HostLedger,
    policy: &HostPolicy,
    health_file: Option<&Path>,
    burst_evidence: &crate::burst_manager::Snapshot,
) -> Option<crate::host::Capacity> {
    // A nested scope can move the last process out of its previous boundary
    // between readiness and Consume. Settle positively empty native owners
    // before validating the remaining grants, rather than mistaking an already
    // collected scope for damaged enforcement until the next periodic tick.
    // Unknown cleanup and surviving descendants retain their claims.
    ledger.reconcile(host_native::empty_reservation);
    ledger.reconcile_continuation_policy(policy);
    ledger.retain_pool_operations();
    crate::namespace_runner::reconcile(ledger, policy);
    ledger
        .sync_protected_pools(policy, crate::protected_pool::empty)
        .ok()?;
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
    if crate::namespace_runner::enforcement(ledger, policy) == Some(true)
        && crate::protected_pool::enforcement(ledger, policy) == Some(true)
        && ledger
            .reservations
            .iter()
            .filter(|r| r.granted)
            .all(|r| cached_enforcement(r, burst_evidence))
    {
        host_native::capacity().ok().filter(|_| {
            health_file.is_none_or(|path| crate::health::permits(path).unwrap_or(false))
        })
    } else {
        None
    }
}

fn cached_enforcement(r: &Reservation, evidence: &crate::burst_manager::Snapshot) -> bool {
    host_native::memory_enforcement(r).is_some()
        && (r.runtime_max_ms.is_none()
            || evidence
                .runtime(&r.identity)
                .is_some_and(|runtime| Some(runtime) == r.runtime_max_ms))
}

fn reconcile_recovery(ledger: &mut HostLedger, manager: &recovery_manager::Cache) {
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
    if host_native::empty_reservation(&reservation) == Some(true)
        || manager.replaced(lease) == Some(true)
    {
        ledger.recovery = None;
    }
}

fn handle_advance_request(
    request: Request,
    pid: i32,
    uid: u32,
    policy: &HostPolicy,
    ledger: &mut HostLedger,
    evidence: (
        Option<&Path>,
        Option<InventoryProof<'_>>,
        &crate::burst_manager::Snapshot,
    ),
    reply: &mut Response,
) -> Result<()> {
    use crate::preparation::PreparationPhase;
    let (health_file, inventory_proof, burst_evidence) = evidence;
    let now = crate::clock::boot_ms()?;
    match request {
        Request::AuthorizePreparation {
            version,
            id,
            profile,
            origin,
        } => {
            ensure!(version == 1, "unsupported host protocol");
            let claims = ledger
                .native_claims(policy, None)
                .ok_or_else(|| anyhow::anyhow!("preparation helper backing unavailable"))?;
            crate::namespace_runner::verify(pid, uid, policy, &claims)?;
            let helper = crate::ledger::ClientIdentity {
                pid,
                start_ticks: host_native::process_start(pid)?,
            };
            preparation_auth::authorize(ledger, &id, &profile, (uid, helper), origin, now, policy)?;
            // The secret is delivered only to the kernel-authenticated waiting
            // origin. A helper's enrollment does not disclose the capability.
            reply.granted = true;
        }
        Request::NamespaceRunner { version } => {
            ensure!(version == 1, "unsupported host protocol");
            let claims = ledger
                .native_claims(policy, None)
                .ok_or_else(|| anyhow::anyhow!("namespace runner backing unavailable"))?;
            crate::namespace_runner::verify(pid, uid, policy, &claims)?;
            reply.granted = true;
        }
        Request::Prepare {
            version,
            id,
            key,
            profile,
        } => {
            ensure!(version == 1, "unsupported host protocol");
            let claims = ledger
                .native_claims(policy, None)
                .ok_or_else(|| anyhow::anyhow!("preparation helper backing unavailable"))?;
            crate::namespace_runner::verify(pid, uid, policy, &claims)?;
            let owner = crate::ledger::ClientIdentity {
                pid,
                start_ticks: host_native::process_start(pid)?,
            };
            ensure!(
                host_native::preparation_owner_alive(&owner) == Some(true),
                "preparation helper lifetime unavailable"
            );
            preparation_auth::prepare(ledger, &id, &key, (uid, owner), &profile, now, policy)?;
            preparation_reply(ledger, policy, &id, &key, uid, reply)?;
        }
        Request::Preparation { version, id, key } => {
            ensure!(version == 1, "unsupported host protocol");
            preparation_auth::verify_caller(ledger, &id, &key, uid, pid)?;
            preparation_reply(ledger, policy, &id, &key, uid, reply)?;
        }
        Request::CancelPreparation { version, id, key } => {
            ensure!(version == 1, "unsupported host protocol");
            if ledger.preparations.iter().any(|p| p.id == id) {
                preparation_auth::verify_caller(ledger, &id, &key, uid, pid)?;
            }
            ledger.cancel_preparation(&id, &key, uid)?;
        }
        Request::Consume {
            version,
            id,
            key,
            origin,
        } => {
            ensure!(version == 1, "unsupported host protocol");
            let caller = pid;
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
            if let Some(approved) = &p.origin {
                ensure!(
                    approved.pid == pid
                        && host_native::process_start(pid)? == approved.start_ticks
                        && origin.as_ref().is_none_or(|origin| origin == approved),
                    "prepared origin differs from the authorized rendezvous"
                );
            } else {
                ensure!(
                    origin.is_none()
                        && matches!(
                            p.phase,
                            PreparationPhase::Active | PreparationPhase::Reconciling
                        ),
                    "preparation rendezvous authorization missing"
                );
            }
            if origin.is_some() {
                preparation_auth::verify_caller(ledger, &id, &key, uid, caller)?;
            }
            verify_preparation_helper(ledger, policy, &p)?;
            let (domain, identity, memory_bytes, swap_bytes) = if matches!(
                p.phase,
                PreparationPhase::Active | PreparationPhase::Reconciling
            ) {
                let grant = ledger
                    .reservations
                    .iter()
                    .find(|r| r.id == id && r.granted)
                    .ok_or_else(|| anyhow::anyhow!("consumed preparation grant disappeared"))?;
                ensure!(
                    grant.domain == p.domain
                        && grant.memory_bytes == p.memory_bytes
                        && grant.swap_bytes == p.swap_bytes,
                    "consumed preparation envelope changed"
                );
                host_native::identify_prepared_replay(pid, uid, grant)?
            } else {
                host_native::identify_prepared(pid, uid, &policy.domains)?
            };
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
                ensure!(p.policy_matches(policy), "preparation policy changed");
                let capacity = observe(ledger, policy, health_file, burst_evidence)
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
            reply.preparation_consume_proof = p.consume_proof;
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
            let invocation_id = recovery_invocation(&identity, inventory_proof)?;
            let file = crate::swap::open_target(target)?;
            let native_target = crate::recovery::TargetIdentity::from_file(&file)?;
            if let Some(lease) = &ledger.recovery {
                ensure!(
                    lease.identity == identity
                        && matches!(&lease.action, crate::recovery::RecoveryAction::Device {target:t,native_target:Some(native),..}
                            if t.name == target.name && t.priority == target.priority && *native == native_target),
                    "another native recovery owns capacity"
                );
                reply.granted = true;
                reply.recovery = ledger.recovery.clone();
                return Ok(());
            }
            let device = crate::swap::device_from_file(&file)?
                .ok_or_else(|| anyhow::anyhow!("swap target is inactive; restore first"))?;
            if device.used_bytes < spec.minimum_bytes {
                reply.waiting = Some(WaitReason::SwapReturn);
                return Ok(());
            }
            let capacity = observe(ledger, policy, health_file, burst_evidence);
            let native_safe = match inventory_proof {
                Some(proven) => {
                    let mut claims = ledger
                        .native_claims(policy, None)
                        .ok_or_else(|| anyhow::anyhow!("native recovery claims are unproven"))?;
                    claims.push(crate::recovery::helper_claim(&identity, spec.helper_bytes));
                    proven.safe && crate::page_return::native_headroom(&identity, &claims, 0)?
                }
                None => {
                    crate::swap::native_return_safe(ledger, policy, &identity, spec.helper_bytes)?
                }
            };
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
                    invocation_id: Some(invocation_id),
                    action: crate::recovery::RecoveryAction::Device {
                        target,
                        before_used_bytes: device.used_bytes,
                        native_target: Some(native_target),
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
            let invocation_id = recovery_invocation(&helper, inventory_proof)?;
            let target = crate::page_return::identity(target_pid, start_ticks, spec)?;
            let target_guard = crate::page_return::open_memory(&target)?;
            let helper_guard = crate::page_return::open_memory(&helper)?;
            ensure!(
                crate::page_return::helper_holds_guards(pid, &target_guard, &helper_guard)?,
                "page return helper does not retain the kernel mm guards"
            );
            let guards = crate::page_return::PageReturnGuards::new(&target_guard, &helper_guard);
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
                        matches!(&lease.action,crate::recovery::RecoveryAction::Pages {target:t,address:a,bytes:b,guards:g,..}
                        if t == &target && *a == address && *b == bytes && g.as_ref() == Some(&guards)),
                        "previous page return is not settled"
                    );
                    reply.granted = true;
                    reply.recovery = ledger.recovery.clone();
                    return Ok(());
                }
            }
            let before_swap_bytes = crate::page_return::target_swap_bytes(&target)?;
            // The mm may contain swapped pages charged to another (or now
            // offlined) memcg. Destination swap counters are not page ownership.
            if !crate::page_return::range_swapped(
                &crate::page_return::open_pagemap(&target)?,
                address,
                bytes,
            )? {
                reply.waiting = Some(WaitReason::SwapReturn);
                return Ok(());
            }
            let capacity = observe(ledger, policy, health_file, burst_evidence);
            let claims = ledger
                .native_claims(policy, None)
                .ok_or_else(|| anyhow::anyhow!("completion native backing unavailable"))?;
            let native_safe = match inventory_proof {
                Some(proven) => {
                    let mut fresh_claims = claims;
                    fresh_claims.push(crate::recovery::helper_claim(&helper, spec.helper_bytes));
                    proven.safe
                        && crate::page_return::native_headroom(&target, &fresh_claims, bytes)?
                        && crate::page_return::native_headroom(&helper, &fresh_claims, bytes)?
                }
                None => crate::page_return::recovery_headroom(
                    &target,
                    &helper,
                    &claims,
                    bytes,
                    spec.helper_bytes,
                )?,
            };
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
                    invocation_id: Some(invocation_id),
                    action: crate::recovery::RecoveryAction::Pages {
                        target,
                        address,
                        bytes,
                        before_swap_bytes,
                        // Per-batch authority is the guarded PTE range and
                        // complete native backing, not aggregate cache deltas.
                        before_return_bytes: None,
                        guards: Some(guards),
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
                guards,
                ..
            } = &mut lease.action
            else {
                anyhow::bail!("native recovery is not a page return");
            };
            ensure!(
                crate::page_return::identity(target.pid, target.start_ticks, spec)? == *target,
                "page return target changed before settlement"
            );
            let target_guard = crate::page_return::open_memory(target)?;
            let helper_guard = crate::page_return::open_memory(&helper)?;
            ensure!(
                guards.as_ref()
                    == Some(&crate::page_return::PageReturnGuards::new(
                        &target_guard,
                        &helper_guard
                    ))
                    && crate::page_return::helper_holds_guards(pid, &target_guard, &helper_guard)?,
                "page return protected mm changed before settlement"
            );
            let (after_swap, after_return) = if before_return_bytes.is_some() {
                // Retain exact accounting for historical leases that recorded
                // that optional diagnostic; never invent a zero reduction.
                let after = crate::page_return::target_usage(target)?;
                (after.used_bytes, Some(after.return_bytes()))
            } else {
                (crate::page_return::target_swap_bytes(target)?, None)
            };
            reply.returned_bytes = Some(before_swap_bytes.saturating_sub(after_swap));
            reply.return_demand_reduction_bytes = before_return_bytes
                .zip(after_return)
                .map(|(before, after)| before.saturating_sub(after));
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
            let identity = ledger.pool_release_identity(
                &domain,
                pid,
                host_native::process_start(pid)?,
                operation,
            )?;
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

#[derive(Debug, PartialEq, Eq)]
enum CompletionAdmission {
    Parent(Option<String>),
    Waiting,
}

fn acquire_completion(
    ledger: &mut HostLedger,
    identity: &Identity,
    domain: &str,
    operation: Option<u64>,
    envelope: (u64, u64),
    call: &str,
    capability: Option<crate::continuation::ContinuationCapability>,
) -> Result<CompletionAdmission> {
    let existing = ledger.reservations.iter().find(|r| {
        r.domain == domain
            && r.identity.cgroup == identity.cgroup
            && r.identity.inode == identity.inode
    });
    if identity.uid == 0 && ledger.owns_pool_operation(domain, identity, operation) {
        // Exact persisted Worker ownership, not the continued existence of its
        // submitting process, authenticates a lost-response or dependency retry.
        return Ok(CompletionAdmission::Parent(
            existing.and_then(|r| r.continuation.clone()),
        ));
    }
    if identity.uid == 0
        && existing.is_some_and(|r| {
            r.continuation.as_deref() != capability.as_ref().map(|c| c.parent.as_str())
        })
    {
        // Waiting must neither reattribute queued owners nor consume a call.
        return Ok(CompletionAdmission::Waiting);
    }
    let Some(capability) = capability else {
        return Ok(CompletionAdmission::Parent(None));
    };
    let owner_uid = ledger
        .continuations
        .iter()
        .find(|c| c.capability.parent == capability.parent)
        .map_or(identity.uid, |c| c.uid);
    Ok(CompletionAdmission::Parent(Some(
        ledger.authorize_continuation(
            &capability,
            if identity.uid == 0 {
                owner_uid
            } else {
                identity.uid
            },
            domain,
            envelope.0,
            envelope.1,
            call,
        )?,
    )))
}

fn verify_preparation_helper(
    ledger: &HostLedger,
    policy: &HostPolicy,
    preparation: &crate::preparation::Preparation,
) -> Result<()> {
    if matches!(
        preparation.phase,
        crate::preparation::PreparationPhase::Active
            | crate::preparation::PreparationPhase::Reconciling
    ) {
        return Ok(());
    }
    let owner = preparation
        .owner
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("preparation helper runner lifetime missing"))?;
    let origin = preparation.origin.as_ref().ok_or_else(|| {
        anyhow::anyhow!("preparation helper runner rendezvous authorization missing")
    })?;
    host_native::prepared_origin(origin, preparation.uid)
        .map_err(|error| error.context("preparation helper runner rendezvous lifetime changed"))?;
    ensure!(
        host_native::preparation_owner_alive(origin) == Some(true),
        "preparation helper runner rendezvous exited"
    );
    ensure!(
        preparation.helper_cgroup_inode
            == Some(
                preparation_auth::helper_boundary(&preparation.id, preparation.uid, owner)
                    .map_err(
                        |error| error.context("preparation helper runner invocation changed")
                    )?
            ),
        "preparation helper runner invocation changed"
    );
    ensure!(
        host_native::preparation_owner_alive(owner) == Some(true)
            && fs::metadata(format!("/proc/{}", owner.pid))?.uid() == preparation.uid,
        "preparation helper runner lifetime or UID changed"
    );
    let claims = ledger
        .native_claims(policy, None)
        .ok_or_else(|| anyhow::anyhow!("preparation helper runner backing unavailable"))?;
    crate::namespace_runner::verify(owner.pid, preparation.uid, policy, &claims)
        .map_err(|error| error.context("preparation helper runner placement or limits changed"))?;
    ensure!(
        host_native::preparation_owner_alive(owner) == Some(true),
        "preparation helper runner lifetime changed during verification"
    );
    Ok(())
}

fn reconcile_preparation_helpers(ledger: &mut HostLedger, policy: &HostPolicy) {
    let invalid: std::collections::BTreeSet<_> = ledger
        .preparations
        .iter()
        .filter(|preparation| verify_preparation_helper(ledger, policy, preparation).is_err())
        .map(|preparation| preparation.id.clone())
        .collect();
    // Pending intents own no executing grant. Invalidate their barriers on
    // missing or changed backing; consumed native claims reconcile separately.
    ledger
        .preparations
        .retain(|preparation| !invalid.contains(&preparation.id));
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
    verify_preparation_helper(ledger, policy, p)?;
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
    use crate::protocol::{read_frame, write_frame};

    #[test]
    fn host_wait_is_optional_without_changing_legacy_finite_requests() {
        for wait in [None, Some(250u64)] {
            let mut value = serde_json::json!({"op": "acquire", "version": 1});
            if let Some(wait) = wait {
                value["wait_ms"] = serde_json::json!(wait);
            }
            let request: Request = serde_json::from_value(value).unwrap();
            let encoded = serde_json::to_value(request).unwrap();
            assert_eq!(encoded.get("wait_ms").and_then(|v| v.as_u64()), wait);
            assert_eq!(encoded.get("wait_ms").is_none(), wait.is_none());
        }
    }

    #[test]
    fn controlled_exchange_rejects_grants_after_a_wait_deadline_or_cancellation() {
        for cancelled in [false, true] {
            let (client, mut broker) = UnixStream::pair().unwrap();
            let worker = thread::spawn(move || {
                let request: Request = read_frame(&mut broker).unwrap();
                assert!(matches!(request, Request::Acquire { .. }));
                thread::sleep(Duration::from_millis(400));
                let _ = write_frame(
                    &mut broker,
                    &Response {
                        version: 1,
                        granted: true,
                        ..Default::default()
                    },
                );
            });
            let start = Instant::now();
            let stopping = || cancelled && start.elapsed() >= Duration::from_millis(50);
            let result = exchange_cancellable(
                client,
                &Request::Acquire {
                    version: 1,
                    wait_ms: Some(100),
                    continuation: None,
                },
                Some((
                    start + Duration::from_millis(if cancelled { 2000 } else { 100 }),
                    &stopping,
                )),
            );
            let elapsed = start.elapsed();
            worker.join().unwrap();
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(if cancelled {
                    "cancelled"
                } else {
                    "deadline exceeded"
                }),
                "{error}"
            );
            assert!(
                elapsed < Duration::from_millis(250),
                "{cancelled}: {elapsed:?}"
            );
        }
    }

    #[test]
    fn controlled_exchange_still_validates_granted_responses() {
        for (version, error) in [(1, None), (2, None), (1, Some("refused"))] {
            let (client, mut broker) = UnixStream::pair().unwrap();
            let valid = version == 1 && error.is_none();
            let worker = thread::spawn(move || {
                let _: Request = read_frame(&mut broker).unwrap();
                write_frame(
                    &mut broker,
                    &Response {
                        version,
                        granted: true,
                        error: error.map(str::to_owned),
                        ..Default::default()
                    },
                )
                .unwrap();
            });
            let result = exchange_cancellable(
                client,
                &Request::Acquire {
                    version: 1,
                    wait_ms: Some(1000),
                    continuation: None,
                },
                Some((Instant::now() + Duration::from_secs(2), &|| false)),
            );
            worker.join().unwrap();
            assert_eq!(result.is_ok(), valid, "{result:?}");
            if valid {
                assert!(result.unwrap().granted);
            }
        }
    }

    #[test]
    fn startup_reconciles_dead_children_before_reusing_a_pool_name() {
        let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version": 1, "budget_bytes": 100, "reserve_bytes": 20,
            "swap_reserve_bytes": 0, "max_memory_full_psi": 1.0, "max_io_full_psi": 20.0,
            "resume_ms": 250, "aging_ms": 1000, "queue_limit": 1,
            "domains": [{"name": "quick", "uid": 1000, "cgroup": "/quick", "ceiling_bytes": 10,
              "swap_bytes": 0, "fair_share_bytes": 100,
              "protected_pool": {"source_contract": "tool", "memory_bytes": 30, "max_job_bytes": 10,
                "max_running": 2, "runtime_max_sec": 15, "queue_limit": 2, "wait_max_sec": 15,
                "cpu_quota_usec": 50000, "tasks_max": 32}}]
        }))
        .unwrap();
        let mut ledger = HostLedger::new("boot".into());
        ledger
            .sync_protected_pools(&policy, |_| Some(true))
            .unwrap();
        ledger.reservations.push(serde_json::from_value(serde_json::json!({
            "id": "stale", "domain": "quick", "identity": {"uid": 1000, "cgroup": "/quick/old", "inode": 1, "pid": 1, "start_ticks": 1},
            "memory_bytes": 10, "swap_bytes": 0, "requested_ms": 0, "deadline_ms": 15000, "granted": true, "runtime_max_ms": 15000
        })).unwrap());
        policy.domains[0].uid = 2000;
        policy.domains[0].cgroup = "/replacement".into();
        for cleanup in [None, Some(false)] {
            let mut unknown = ledger.clone();
            assert!(reconcile_startup(&mut unknown, &policy, |_| cleanup, |_| Some(true)).is_err());
            assert_eq!(unknown.reservations.len(), 1);
            assert_eq!(unknown.protected_pools["quick"].uid, 1000);
        }
        reconcile_startup(&mut ledger, &policy, |_| Some(true), |_| Some(true)).unwrap();
        assert!(ledger.reservations.is_empty());
        assert_eq!(ledger.protected_pools["quick"].uid, 2000);
        assert_eq!(ledger.protected_pools["quick"].cgroup, "/replacement");
        assert_eq!(ledger.committed(), 30);
    }

    #[test]
    fn device_inventory_replay_requires_the_same_bound_native_object() {
        let value = serde_json::json!({
            "identity":{"cgroup":"/recovery.service","inode":1,"uid":0,"pid":1,"start_ticks":1},
            "invocation_id":"11111111111111111111111111111111",
            "action":{"kind":"device","target":{"name":"swap","path":"/swap","priority":17},
                "before_used_bytes":1024,"native_target":{"file":[12,34]}},
            "return_bytes":4096,"helper_bytes":1024
        });
        let original =
            Some(serde_json::from_value::<crate::recovery::RecoveryLease>(value.clone()).unwrap());
        assert!(inventory_recovery_matches(&original, &original).unwrap());
        let mut changed = value.clone();
        changed["action"]["native_target"]["file"][1] = serde_json::json!(35);
        let replacement = Some(serde_json::from_value(changed).unwrap());
        assert!(!inventory_recovery_matches(&replacement, &original).unwrap());
        let mut legacy = value;
        legacy["action"]
            .as_object_mut()
            .unwrap()
            .remove("native_target");
        let legacy = Some(serde_json::from_value(legacy).unwrap());
        assert!(!inventory_recovery_matches(&legacy, &legacy).unwrap());
    }

    #[test]
    fn public_preparations_never_disclose_completion_proof() {
        let key = "11111111-1111-1111-1111-111111111111";
        let proof = "22222222-2222-2222-2222-222222222222";
        let preparation: crate::preparation::Preparation = serde_json::from_value(serde_json::json!({
            "id":"intent","key":key,"consume_proof":proof,"uid":1000,
            "owner":{"pid":42,"start_ticks":7},"origin":{"pid":43,"start_ticks":8},"helper_cgroup_inode":1,
            "profile":"game","domain":"game","memory_bytes":40,"swap_bytes":0,
            "requested_ms":0,"expires_ms":15000,"wait_ms":10000,"ready_ms":15000,
            "phase":"ready","drain_domains":[],"drain":[],"waiting":null
        })).unwrap();
        let mut ledger = HostLedger::new("boot".into());
        ledger.preparations.push(preparation);
        for uid in [0, 1000, 1001] {
            let mut reply = Response {
                version: 1,
                ..Default::default()
            };
            populate_status(&mut reply, &ledger, uid);
            reply.preparation = Some(public_preparation(&ledger.preparations[0]));
            let public = serde_json::to_string(&reply).unwrap();
            assert!(!public.contains(key));
            assert!(!public.contains(proof));
            assert!(reply.preparation_consume_proof.is_none());
        }
        assert_eq!(ledger.preparations[0].consume_proof.as_deref(), Some(proof));
    }

    #[test]
    fn a_live_preparation_helper_outside_its_runner_cannot_poll_or_consume() {
        let uid = nix::unistd::geteuid().as_raw();
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":67108864,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "namespace_runner_bytes":67108864,
            "domains":[{"name":"game","uid":uid,"cgroup":format!("/user.slice/user-{uid}.slice/user@{uid}.service/game.slice"),"ceiling_bytes":134217728,"swap_bytes":0,"fair_share_bytes":134217728}],
            "preparations":[{"name":"game","domain":"game","memory_bytes":134217728,"swap_bytes":0,
                "drain_domains":[],"wait_ms":3600000,"ready_ms":15000}]
        })).unwrap();
        let pid = std::process::id() as i32;
        let owner = crate::ledger::ClientIdentity {
            pid,
            start_ticks: host_native::process_start(pid).unwrap(),
        };
        assert_eq!(host_native::preparation_owner_alive(&owner), Some(true));
        for phase in ["draining", "ready"] {
            let mut ledger = HostLedger::new("boot".into());
            ledger.preparations.push(
                serde_json::from_value(serde_json::json!({
                    "id":"intent", "key":"secret", "uid":uid, "owner":owner,
                    "origin":owner, "helper_cgroup_inode":1,
                    "profile":"game", "domain":"game", "memory_bytes":134217728, "swap_bytes":0,
                    "requested_ms":0, "expires_ms":3600000, "ready_ms":15000,
                    "phase":phase, "drain":[], "waiting":null
                }))
                .unwrap(),
            );
            let mut restored: HostLedger =
                serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
            let before = serde_json::to_vec(&restored).unwrap();
            let evidence = crate::burst_manager::Snapshot::default();
            for request in [
                Request::Preparation {
                    version: 1,
                    id: "intent".into(),
                    key: "secret".into(),
                },
                Request::Consume {
                    version: 1,
                    id: "intent".into(),
                    key: "secret".into(),
                    origin: None,
                },
            ] {
                let result = handle_advance_request(
                    request,
                    pid,
                    uid,
                    &policy,
                    &mut restored,
                    (None, None, &evidence),
                    &mut Response::default(),
                );
                assert!(
                    result.is_err(),
                    "a live helper outside its backed runner must not retain a {phase} claim"
                );
                assert!(format!("{:#}", result.unwrap_err()).contains("preparation helper runner"));
                assert_eq!(serde_json::to_vec(&restored).unwrap(), before);
            }
            reconcile_preparation_helpers(&mut restored, &policy);
            assert!(
                !restored.preparation_barrier(),
                "changed runner placement must release the pending barrier after restart"
            );
            assert!(restored.preparations.is_empty());
        }
    }

    #[test]
    fn preparation_request_outside_the_backed_runner_cannot_create_a_barrier() {
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":67108864,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "namespace_runner_bytes":67108864,
            "domains":[{"name":"game","uid":1000,"cgroup":"/user.slice/user-1000.slice/user@1000.service/game.slice","ceiling_bytes":134217728,"swap_bytes":0,"fair_share_bytes":134217728}],
            "preparations":[{"name":"game","domain":"game","memory_bytes":134217728,"swap_bytes":0,
                "drain_domains":[],"wait_ms":3600000,"ready_ms":15000}]
        })).unwrap();
        policy.validate().unwrap();
        let mut ledger = HostLedger::new("boot".into());
        let before = serde_json::to_vec(&ledger).unwrap();
        let result = handle_advance_request(
            Request::Prepare {
                version: 1,
                id: "outside".into(),
                key: "secret".into(),
                profile: "game".into(),
            },
            std::process::id() as i32,
            1000,
            &policy,
            &mut ledger,
            (None, None, &crate::burst_manager::Snapshot::default()),
            &mut Response::default(),
        );
        assert!(
            result.is_err(),
            "an enrolled UID must not skip the runner boundary handshake"
        );
        assert!(!ledger.preparation_barrier());
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
    }

    #[test]
    fn root_pool_release_uses_persisted_worker_ownership_after_policy_removal() {
        let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":100,"reserve_bytes":10,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "domains":[{"name":"builders","uid":0,"cgroup":"/builders","ceiling_bytes":10,"swap_bytes":0,"fair_share_bytes":10}]
        })).unwrap();
        let pid = std::process::id() as i32;
        let start = host_native::process_start(pid).unwrap();
        let pool: Reservation = serde_json::from_value(serde_json::json!({
            "id":"pool", "domain":"builders", "memory_bytes":20, "swap_bytes":0,
            "requested_ms":0, "deadline_ms":10000, "granted":true,
            "identity":{"cgroup":"/builders", "inode":1,"uid":0,"pid":pid,"start_ticks":start},
            "owners":[{"pid":pid,"start_ticks":start}]
        }))
        .unwrap();
        let mut ledger = HostLedger::new("boot".into());
        ledger.reservations.push(pool.clone());
        ledger
            .bind_pool_operation("builders", &pool.identity, 1)
            .unwrap();
        ledger
            .bind_pool_operation("builders", &pool.identity, 2)
            .unwrap();
        let mut ledger: HostLedger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        ledger.validate().unwrap();
        let evidence = crate::burst_manager::Snapshot::default();
        let release = |operation| Request::ReleasePool {
            version: 1,
            domain: "builders".into(),
            operation,
        };
        // Reduced ceilings must not change an already-owned operation's release.
        let mut reply = Response::default();
        handle_advance_request(
            release(1),
            pid,
            0,
            &policy,
            &mut ledger,
            (None, None, &evidence),
            &mut reply,
        )
        .unwrap();
        assert!(reply.granted);
        assert_eq!(ledger.committed(), 20);
        assert!(
            !ledger.reservations[0].owners_finished,
            "the nested outer Worker still owns backing"
        );
        let before = serde_json::to_vec(&ledger).unwrap();
        assert!(
            handle_advance_request(
                release(3),
                pid,
                0,
                &policy,
                &mut ledger,
                (None, None, &evidence),
                &mut Response::default()
            )
            .is_err()
        );
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
        policy.domains.clear();
        handle_advance_request(
            release(2),
            pid,
            0,
            &policy,
            &mut ledger,
            (None, None, &evidence),
            &mut reply,
        )
        .unwrap();
        assert!(ledger.reservations[0].owners_finished);
        assert!(ledger.pool_operations.is_empty());
        assert_eq!(
            ledger.committed(),
            20,
            "native descendants remain charged until positive cleanup"
        );
    }

    #[test]
    fn inventory_poll_never_waits_and_stale_or_disconnected_results_cannot_grant() {
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":100,"reserve_bytes":10,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "domains":[{"name":"builders","uid":0,"cgroup":"/builders","ceiling_bytes":20,"swap_bytes":0,"fair_share_bytes":20}]
        })).unwrap();
        let mut ledger = HostLedger::new("boot".into());
        let (client, stream) = UnixStream::pair().unwrap();
        let pid = std::process::id() as i32;
        let owner = crate::ledger::ClientIdentity {
            pid,
            start_ticks: host_native::process_start(pid).unwrap(),
        };
        let backing = inventory_backing(&ledger, &policy).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (completion, notifier) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let _notifier = notifier;
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok((
                Response {
                    version: 1,
                    granted: true,
                    ..Default::default()
                },
                None,
                Instant::now(),
            ))
        });
        let mut pending = Some(PendingInventory {
            stream,
            completion,
            request: Request::AcquireRecovery {
                version: 1,
                target: "swap".into(),
            },
            owner: owner.clone(),
            backing: backing.clone(),
            worker,
        });
        let began = Instant::now();
        for _ in 0..10 {
            assert!(take_finished_inventory(&mut pending).is_none());
            let mut reply = Response {
                version: 1,
                ..Default::default()
            };
            populate_status(&mut reply, &ledger, 0);
            assert!(reply.reservations.unwrap().is_empty());
        }
        assert!(began.elapsed() < Duration::from_millis(500));
        release.send(()).unwrap();
        let task = pending.take().unwrap();
        task.worker.join().unwrap().unwrap();
        assert!(
            inventory_current(
                &owner,
                &backing,
                Instant::now(),
                &task.stream,
                &ledger,
                &policy
            )
            .unwrap()
        );
        ledger.reservations.push(
            serde_json::from_value(serde_json::json!({
                "id":"pool", "domain":"builders", "memory_bytes":20, "swap_bytes":0,
                "requested_ms":0, "deadline_ms":10000, "granted":false,
                "identity":{"cgroup":"/builders", "inode":1,"uid":0,"pid":42,"start_ticks":7}
            }))
            .unwrap(),
        );
        assert!(
            inventory_current(
                &owner,
                &backing,
                Instant::now(),
                &task.stream,
                &ledger,
                &policy
            )
            .unwrap(),
            "an ungranted waiter must not invalidate proven native backing"
        );
        ledger.reservations[0].granted = true;
        assert!(
            !inventory_current(
                &owner,
                &backing,
                Instant::now(),
                &task.stream,
                &ledger,
                &policy
            )
            .unwrap()
        );
        ledger.reservations.clear();
        assert!(
            !inventory_current(
                &owner,
                &backing,
                Instant::now() - Duration::from_secs(2),
                &task.stream,
                &ledger,
                &policy
            )
            .unwrap()
        );
        drop(client);
        assert!(
            !inventory_current(
                &owner,
                &backing,
                Instant::now(),
                &task.stream,
                &ledger,
                &policy
            )
            .unwrap()
        );
    }

    #[test]
    fn inventory_completion_allows_manager_latency_before_final_native_validation() {
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":10,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "domains":[{"name":"builders","uid":0,"cgroup":"/builders","ceiling_bytes":20,"swap_bytes":0,"fair_share_bytes":20}]
        })).unwrap();
        let mut ledger = HostLedger::new("boot".into());
        let backing = inventory_backing(&ledger, &policy).unwrap();
        let (_client, stream) = UnixStream::pair().unwrap();
        let pid = std::process::id() as i32;
        let owner = crate::ledger::ClientIdentity {
            pid,
            start_ticks: host_native::process_start(pid).unwrap(),
        };
        let lease: crate::recovery::RecoveryLease = serde_json::from_value(serde_json::json!({
            "identity":{"cgroup":"/system.slice/amc-recovery.service","inode":1,"uid":0,"pid":pid,"start_ticks":owner.start_ticks},
            "invocation_id":"11111111111111111111111111111111",
            "action":{"kind":"pages","target":{"cgroup":"/target","inode":2,"uid":0,"pid":43,"start_ticks":8},
                "address":4096,"bytes":4096,"before_swap_bytes":8192,"before_return_bytes":8192,"settled":false,
                "guards":{"target":{"cookie":10,"inode":2},"helper":{"cookie":11,"inode":1}}},
            "return_bytes":4096,"helper_bytes":1024
        })).unwrap();
        let scanned = Response {
            version: 1,
            granted: true,
            recovery: Some(lease.clone()),
            ..Default::default()
        };
        let (scanned, completed) = finish_inventory(
            || {
                // Successful systemd queries may take two seconds, off-loop.
                thread::sleep(Duration::from_millis(1100));
                Ok(scanned)
            },
            Ok,
        )
        .unwrap();
        let result = InventoryReplay {
            owner: &owner,
            backing: &backing,
            completed,
            stream: &stream,
        }
        .apply(&mut ledger, &policy, scanned, |candidate| {
            candidate.recovery = Some(lease.clone());
            Ok(Response {
                version: 1,
                granted: true,
                recovery: Some(lease.clone()),
                ..Default::default()
            })
        });
        assert!(
            result.is_ok(),
            "a healthy bounded manager query expired the fresh replay: {result:?}"
        );
        assert!(result.unwrap().granted);
        assert!(inventory_recovery_matches(&ledger.recovery, &Some(lease)).unwrap());
    }

    #[test]
    fn inventory_replay_keeps_exact_authority_with_fresh_native_counter_telemetry() {
        let scanned: crate::recovery::RecoveryLease = serde_json::from_value(serde_json::json!({
            "identity":{"cgroup":"/helper.service","inode":1,"uid":0,"pid":42,"start_ticks":7},
            "invocation_id":"11111111111111111111111111111111",
            "action":{"kind":"pages","target":{"cgroup":"/target","inode":2,"uid":0,"pid":43,"start_ticks":8},
                "address":4096,"bytes":4096,"before_swap_bytes":8192,"before_return_bytes":8192,"settled":false,
                "guards":{"target":{"cookie":10,"inode":2},"helper":{"cookie":11,"inode":1}}},
            "return_bytes":4096,"helper_bytes":1024
        })).unwrap();
        let mut fresh = serde_json::to_value(&scanned).unwrap();
        fresh["action"]["before_swap_bytes"] = 16384.into();
        fresh["action"]["before_return_bytes"] = 12288.into();
        let fresh: crate::recovery::RecoveryLease = serde_json::from_value(fresh).unwrap();
        assert!(
            inventory_recovery_matches(&Some(fresh.clone()), &Some(scanned.clone())).unwrap(),
            "live counters are telemetry; grant identity and full batch demand did not change"
        );
        let mut unmeasured = serde_json::to_value(&fresh).unwrap();
        unmeasured["action"]["before_return_bytes"] = serde_json::Value::Null;
        let unmeasured: crate::recovery::RecoveryLease =
            serde_json::from_value(unmeasured).unwrap();
        unmeasured.validate().unwrap();
        assert!(
            inventory_recovery_matches(&Some(unmeasured), &Some(scanned.clone())).unwrap(),
            "absent optional cache telemetry must not change guarded batch authority"
        );
        for (field, value) in [
            (
                "identity",
                serde_json::json!({"cgroup":"/new-helper","inode":3,"uid":0,"pid":44,"start_ticks":9}),
            ),
            (
                "invocation_id",
                serde_json::json!("22222222222222222222222222222222"),
            ),
            ("return_bytes", serde_json::json!(8192)),
            ("helper_bytes", serde_json::json!(2048)),
        ] {
            let mut changed = serde_json::to_value(&fresh).unwrap();
            changed[field] = value;
            assert!(
                !inventory_recovery_matches(
                    &Some(serde_json::from_value(changed).unwrap()),
                    &Some(scanned.clone())
                )
                .unwrap(),
                "{field}"
            );
        }
        for (field, value) in [
            (
                "target",
                serde_json::json!({"cgroup":"/new-target","inode":4,"uid":0,"pid":45,"start_ticks":10}),
            ),
            ("address", serde_json::json!(8192)),
            ("bytes", serde_json::json!(8192)),
            ("settled", serde_json::json!(true)),
            (
                "guards",
                serde_json::json!({"target":{"cookie":12,"inode":2},"helper":{"cookie":11,"inode":1}}),
            ),
            ("guards", serde_json::json!(null)),
        ] {
            let mut changed = serde_json::to_value(&fresh).unwrap();
            changed["action"][field] = value;
            assert!(
                !inventory_recovery_matches(
                    &Some(serde_json::from_value(changed).unwrap()),
                    &Some(scanned.clone())
                )
                .unwrap(),
                "{field}"
            );
        }
    }

    #[test]
    fn maintenance_inventory_reply_can_exceed_the_ordinary_two_second_deadline() {
        let (client, mut broker) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let request: Request = read_frame(&mut broker).unwrap();
            assert!(matches!(request, Request::AcquirePageReturn { .. }));
            thread::sleep(Duration::from_millis(2100));
            write_frame(
                &mut broker,
                &Response {
                    version: 1,
                    granted: true,
                    ..Default::default()
                },
            )
            .unwrap();
        });
        let reply = exchange(
            client,
            &Request::AcquirePageReturn {
                version: 1,
                pid: 42,
                start_ticks: 7,
                address: 4096,
                bytes: 4096,
            },
        )
        .unwrap();
        assert!(reply.granted);
        worker.join().unwrap();
    }

    #[test]
    fn owned_pool_replay_preserves_its_parent_after_restart_and_parent_exit() {
        let mut ledger = HostLedger::new("boot".into());
        let pool: Reservation = serde_json::from_value(serde_json::json!({
            "id":"pool", "domain":"builders", "memory_bytes":20, "swap_bytes":0,
            "requested_ms":0, "deadline_ms":10000, "granted":true,
            "identity":{"cgroup":"/builders", "inode":1,"uid":0,"pid":42,"start_ticks":7},
            "owners":[{"pid":42,"start_ticks":7}], "continuation":"first"
        }))
        .unwrap();
        ledger.reservations.push(pool.clone());
        ledger
            .bind_pool_operation("builders", &pool.identity, 1)
            .unwrap();
        let mut ledger: HostLedger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        ledger.validate().unwrap();
        let before = serde_json::to_vec(&ledger).unwrap();
        assert_eq!(
            acquire_completion(
                &mut ledger,
                &pool.identity,
                "builders",
                Some(1),
                (20, 0),
                "owner-42-7-1",
                None
            )
            .unwrap(),
            CompletionAdmission::Parent(Some("first".into()))
        );
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
        assert_eq!(
            acquire_completion(
                &mut ledger,
                &pool.identity,
                "builders",
                Some(2),
                (20, 0),
                "owner-42-7-2",
                None
            )
            .unwrap(),
            CompletionAdmission::Waiting
        );
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
    }

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
    #[test]
    fn inventory_completion_rejects_owner_headroom_consumed_during_manager_query() {
        use std::cell::Cell;
        let headroom = Cell::new(8192u64);
        let result = finish_inventory(
            || {
                headroom.set(0);
                Ok(Response {
                    version: 1,
                    granted: true,
                    ..Default::default()
                })
            },
            |response| {
                ensure!(
                    headroom.get() >= 4096,
                    "possible charge owner lost finite headroom"
                );
                Ok(response)
            },
        );
        assert!(
            result.is_err(),
            "a pre-query owner frontier must not authorize faults after headroom was consumed"
        );
    }
}
