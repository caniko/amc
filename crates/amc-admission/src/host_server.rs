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
    },
    AcquirePool {
        version: u32,
        wait_ms: u64,
        domain: String,
    },
    Status {
        version: u32,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    pub granted: bool,
    pub ticket: Option<String>,
    pub waiting: Option<WaitReason>,
    pub committed_bytes: u64,
    pub reservations: Option<Vec<Reservation>>,
    pub error: Option<String>,
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

/// Helper calls this before its private coordinator's native entry handshake.
pub fn acquire(socket: &Path, wait: Duration, stopping: impl Fn() -> bool) -> Result<()> {
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
            },
        )?;
        if reply.granted {
            return Ok(());
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
                && r.swap_bytes == contract.memory_swap_max)),
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
                    r.owners
                        .retain(|o| host_native::owner_alive(o) != Some(false));
                    // Keep a sentinel if all owners died until the populated
                    // subtree is empty; validation must never discard capacity.
                    if r.owners.is_empty() {
                        r.owners.push(crate::ledger::ClientIdentity {
                            pid: r.identity.pid,
                            start_ticks: r.identity.start_ticks,
                        });
                    }
                }
            }
            ledger.reconcile(host_native::empty_reservation);
            let capacity = if ledger
                .reservations
                .iter()
                .filter(|r| r.granted)
                .all(|r| host_native::enforcement(r).is_some())
            {
                host_native::capacity().ok()
            } else {
                None
            };
            waiting = ledger.advance(
                crate::clock::boot_ms()?,
                &policy,
                capacity,
                &mut healthy_since,
                |r, entries| host_native::ancestor_headroom(&r.identity, entries),
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
                let result = (|| -> Result<Response> {
                    let mut reply = Response {
                        version: 1,
                        committed_bytes: ledger.committed(),
                        ..Default::default()
                    };
                    match request {
                        Request::Status { version } => {
                            ensure!(version == 1, "unsupported host protocol");
                            reply.reservations = Some(
                                ledger
                                    .reservations
                                    .iter()
                                    .filter(|r| {
                                        credentials.uid() == 0
                                            || r.identity.uid == credentials.uid()
                                    })
                                    .cloned()
                                    .collect(),
                            );
                        }
                        Request::Acquire { version, wait_ms }
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
                            let id = ledger.request(
                                Reservation {
                                    id: fresh_id()?,
                                    domain,
                                    identity,
                                    memory_bytes,
                                    swap_bytes,
                                    requested_ms: now,
                                    deadline_ms: now.saturating_add(wait_ms),
                                    granted: false,
                                    owners: vec![],
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
                        }
                    }
                    Ok(reply)
                })();
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
