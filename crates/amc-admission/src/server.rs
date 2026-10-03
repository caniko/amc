//! Single-writer coordinator. A native pre-exec handshake closes the late
//! submission race: an expired reservation can never execute heavy work.
use crate::{
    ledger::{ClientIdentity, Decision, Ledger, Phase, Policy},
    native::Native,
    protocol::{Message, Request, Response, Status, read_frame, write_frame},
    store::{Store, fresh_id, private_directory},
};
use anyhow::{Result, ensure};
use nix::{
    sys::socket::{getsockopt, sockopt::PeerCredentials},
    unistd::geteuid,
};
use std::{
    fs,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::UnixListener,
    },
    path::Path,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub fn serve(
    policy: Policy,
    socket: &Path,
    state: &Path,
    native: impl Native,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    serve_with_host(policy, socket, state, native, None, stopping)
}

pub fn serve_with_host(
    policy: Policy,
    socket: &Path,
    state: &Path,
    native: impl Native,
    host_socket: Option<&Path>,
    stopping: impl Fn() -> bool,
) -> Result<()> {
    policy.validate()?;
    ensure!(
        socket.is_absolute() && state.is_absolute(),
        "admission paths must be absolute"
    );
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let (store, mut ledger) = Store::open(state, boot.trim())?;
    let parent = socket
        .parent()
        .ok_or_else(|| anyhow::anyhow!("socket needs a parent directory"))?;
    private_directory(parent)?;
    let socket_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(socket.with_extension("lock"))?;
    socket_lock.try_lock()?;
    // Unentered work has no permission to execute. Discard it on restart so
    // queued requests cannot inherit superseded policy; the helper fails closed.
    ledger.entries.retain(|entry| entry.phase == Phase::Running);
    // The durable-state lock is acquired first. A second server can never
    // unlink a live endpoint for this state directory.
    if socket.exists() {
        ensure!(
            std::os::unix::net::UnixStream::connect(socket).is_err(),
            "admission socket already has a listener"
        );
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    store.save(&ledger)?;
    let mut tick = Instant::now();
    let mut cursor = 0usize;
    let mut waiting = std::collections::BTreeMap::new();
    let mut unreconciled: std::collections::BTreeSet<_> =
        ledger.entries.iter().map(|e| e.id.clone()).collect();
    while !stopping() {
        if tick.elapsed() >= Duration::from_millis(250) {
            let before = serde_json::to_vec(&ledger)?;
            // One running identity per tick bounds manager work independently
            // of queue length. All unverified entries remain fully accounted.
            let running: Vec<_> = ledger
                .entries
                .iter()
                .filter(|e| e.phase == Phase::Running)
                .cloned()
                .collect();
            if !running.is_empty() {
                let entry = &running[cursor % running.len()];
                match native.terminated(entry) {
                    Some(terminated) => {
                        unreconciled.remove(&entry.id);
                        if terminated {
                            ledger.reconcile(|e| e.id == entry.id);
                        }
                    }
                    None => {
                        unreconciled.insert(entry.id.clone());
                    }
                }
                cursor = cursor.wrapping_add(1);
            }
            let mut capacity = std::collections::BTreeMap::new();
            waiting = ledger.advance(now_ms()?, &policy, |contract| {
                // At most one bounded native observation per slice/marker per
                // tick. Queued siblings cannot multiply manager requests.
                *capacity
                    .entry((contract.slice.clone(), contract.pause_file.clone()))
                    .or_insert_with(|| native.headroom(contract, policy.reserve_bytes).ok())
            });
            if before != serde_json::to_vec(&ledger)? {
                store.save(&ledger)?;
            }
            tick = Instant::now();
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_millis(250)))?;
                stream.set_write_timeout(Some(Duration::from_millis(250)))?;
                let credentials = getsockopt(&stream, PeerCredentials)?;
                if credentials.uid() != geteuid().as_raw() {
                    continue;
                }
                let Ok(request) = read_frame::<Request>(&mut stream) else {
                    continue;
                };
                let before = serde_json::to_vec(&ledger)?;
                let result = handle(
                    request,
                    credentials.pid(),
                    &policy,
                    &mut ledger,
                    &native,
                    host_socket,
                    Observations {
                        waiting: waiting.clone(),
                        unreconciled: unreconciled.iter().cloned().collect(),
                    },
                );
                // Never acknowledge an uncommitted grant or entry. Persistence
                // failure terminates the server, leaving native limits in force.
                if before != serde_json::to_vec(&ledger)? {
                    store.save(&ledger)?;
                }
                let mut response = match result {
                    Ok(response) => response,
                    Err(error) => Response {
                        version: 1,
                        error: Some(error.to_string()),
                        ..Response::default()
                    },
                };
                response.host_socket = host_socket.map(Path::to_path_buf);
                let _ = write_frame(&mut stream, &response);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
    fs::remove_file(socket)?;
    Ok(())
}

struct Observations {
    waiting: std::collections::BTreeMap<String, Decision>,
    unreconciled: Vec<String>,
}

fn handle(
    request: Request,
    pid: i32,
    policy: &Policy,
    ledger: &mut Ledger,
    native: &impl Native,
    host_socket: Option<&Path>,
    observations: Observations,
) -> Result<Response> {
    ensure!(
        request.version == 1,
        "unsupported admission protocol version"
    );
    let mut response = Response {
        version: 1,
        ..Response::default()
    };
    match request.message {
        Message::Enqueue { contract, wait_ms } => {
            ensure!(
                (1..=3_600_000).contains(&wait_ms),
                "admission wait must be 1ms..1h"
            );
            let id = fresh_id()?;
            let client = client_identity(pid)?;
            ledger.enqueue(
                id.clone(),
                &contract,
                policy,
                now_ms()?.saturating_add(wait_ms).saturating_add(30_000),
            )?;
            ledger.set_client(&id, client)?;
            response.entry = ledger.get(&id).cloned();
        }
        Message::Poll { id } => {
            response.entry = Some(
                ledger
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("ticket expired or unknown"))?
                    .clone(),
            );
        }
        Message::Enter { id } => {
            ensure!(
                native.can_enter(),
                "host supervision inhibits new execution"
            );
            let entry = ledger
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("ticket expired or unknown"))?;
            ensure!(
                entry.phase == Phase::Reserved && now_ms()? < entry.deadline_ms,
                "ticket cannot enter"
            );
            let identity = native
                .identify(entry, pid)
                .map_err(|_| anyhow::anyhow!("native workload identity or enforcement mismatch"))?;
            if let Some(socket) = host_socket {
                crate::host_server::verify_entry(socket, pid, &identity, &entry.contract)?;
            }
            ensure!(
                now_ms()? < entry.deadline_ms,
                "ticket expired during native verification"
            );
            ensure!(
                native.can_enter(),
                "host supervision changed during entry verification"
            );
            ledger.enter(&id, identity)?;
            response.entry = ledger.get(&id).cloned();
        }
        Message::Cancel { id } => {
            if let Some(entry) = ledger.get(&id) {
                ensure!(
                    entry.client.as_ref() == Some(&client_identity(pid)?),
                    "only the submitting client may cancel this ticket"
                );
            }
            if !ledger.cancel_pending(&id)
                && let Some(entry) = ledger.get(&id)
            {
                native.stop(entry).map_err(|_| {
                    anyhow::anyhow!("native stop unconfirmed; reservation retained")
                })?;
            }
        }
        Message::Status => {
            response.status = Some(Status {
                budget_bytes: policy.budget_bytes,
                reserve_bytes: policy.reserve_bytes,
                committed_bytes: ledger.committed(),
                entries: ledger.entries.clone(),
                waiting: observations.waiting,
                unreconciled: observations.unreconciled,
            })
        }
    }
    Ok(response)
}

fn client_identity(pid: i32) -> Result<ClientIdentity> {
    ensure!(pid > 0, "invalid submitting PID");
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| anyhow::anyhow!("client identity unavailable"))?;
    let start_ticks = fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow::anyhow!("client start time unavailable"))?
        .parse()?;
    Ok(ClientIdentity { pid, start_ticks })
}
