//! The manager executes this small helper in the host namespaces. The payload
//! waits locally, so neither its environment nor its filesystem/PID view moves.
use super::{Intent, PrepareArgs, broker_call, exec_payload, prepare_ready};
use amc_admission::{
    host_native,
    host_server::{Request, Response},
    ledger::ClientIdentity,
    protocol::{read_frame, write_frame},
};
use anyhow::{Context, Result, ensure};
use nix::{
    sys::socket::{getsockopt, sockopt::PeerCredentials},
    unistd::geteuid,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, clap::Args)]
pub struct HostArgs {
    #[arg(long)]
    rendezvous: PathBuf,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    profile: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    token: String,
    granted: bool,
    error: Option<String>,
}

// Each frame is acknowledged before another is sent: read_frame is deliberately
// one-frame-only, and this prevents coalesced progress/ready frames being lost.
fn progress(
    stream: &mut UnixStream,
    token: &str,
    granted: bool,
    error: Option<String>,
) -> Result<()> {
    write_frame(
        stream,
        &Progress {
            token: token.into(),
            granted,
            error,
        },
    )?;
    let ack: String = read_frame(stream)?;
    ensure!(ack == token, "prepared rendezvous acknowledgement mismatch");
    Ok(())
}

struct Rendezvous(PathBuf);
impl Drop for Rendezvous {
    fn drop(&mut self) {
        // Never recursively delete a pathname that another process may replace.
        let _ = fs::remove_file(self.0.join("socket"));
        let _ = fs::remove_file(self.0.join("token"));
        let _ = fs::remove_dir(&self.0);
    }
}

pub(super) fn execute(args: PrepareArgs) -> Result<i32> {
    let signals = crate::control::Signals::install()?;
    let runtime = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").context("prepared launch needs XDG_RUNTIME_DIR")?,
    );
    let metadata = fs::symlink_metadata(&runtime)?;
    ensure!(
        runtime.is_absolute()
            && metadata.is_dir()
            && metadata.uid() == geteuid().as_raw()
            && metadata.mode() & 0o077 == 0,
        "untrusted prepared runtime directory"
    );
    let id = amc_admission::store::fresh_id()?;
    let directory = runtime.join(format!("amc-{id}"));
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let rendezvous = Rendezvous(directory);
    let token = amc_admission::store::fresh_id()?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(rendezvous.0.join("token"))?
        .write_all(token.as_bytes())?;
    let listener = UnixListener::bind(rendezvous.0.join("socket"))?;
    fs::set_permissions(
        rendezvous.0.join("socket"),
        fs::Permissions::from_mode(0o600),
    )?;
    listener.set_nonblocking(true)?;
    // No payload, loader environment, or preparation capability is sent through
    // the manager. Its helper obtains the waiting peer's host PID from the kernel.
    crate::control::capture(
        Command::new("systemd-run")
            .args([
                "--user",
                "--quiet",
                "--collect",
                "--service-type=exec",
                "--expand-environment=no",
                "--property=MemoryMax=64M",
                "--property=MemorySwapMax=0",
                "--property=RuntimeMaxSec=3600",
            ])
            .arg(format!("--unit=app-amc-prepare-helper-{id}.service"))
            .arg("--")
            .arg(std::env::current_exe()?)
            .arg("prepared-host")
            .arg("--rendezvous")
            .arg(&rendezvous.0)
            .arg("--socket")
            .arg(&args.socket)
            .arg("--profile")
            .arg(&args.profile),
    )?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        if let Some(signal) = signals.cancelled() {
            return Ok(128 + signal);
        }
        ensure!(
            Instant::now() < deadline,
            "prepared host helper did not connect"
        );
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25))
            }
            Err(e) => return Err(e.into()),
        }
    };
    // In a user namespace the host user's credential maps to our effective UID.
    // Host root is never authenticated here; only the host helper talks to it.
    ensure!(
        getsockopt(&stream, PeerCredentials)?.uid() == geteuid().as_raw(),
        "prepared helper UID mismatch"
    );
    stream.set_read_timeout(Some(Duration::from_secs(35)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    loop {
        let reply: Progress = read_frame(&mut stream)?;
        ensure!(reply.token == token, "prepared helper session mismatch");
        if let Some(signal) = signals.cancelled() {
            return Ok(128 + signal);
        }
        if let Some(error) = reply.error {
            anyhow::bail!("prepared host helper: {error}");
        }
        write_frame(&mut stream, &token)?;
        if reply.granted {
            break;
        }
    }
    drop(stream);
    drop(listener);
    drop(rendezvous);
    drop(signals);
    exec_payload(args.command, args.payload_env)
}

pub(super) fn serve(args: HostArgs) -> Result<i32> {
    let signals = crate::control::Signals::install()?;
    let meta = fs::symlink_metadata(&args.rendezvous)?;
    ensure!(
        args.rendezvous.is_absolute()
            && meta.is_dir()
            && meta.uid() == geteuid().as_raw()
            && meta.mode() & 0o077 == 0,
        "untrusted prepared rendezvous directory"
    );
    let token_path = args.rendezvous.join("token");
    let meta = fs::symlink_metadata(&token_path)?;
    ensure!(
        meta.is_file() && meta.uid() == geteuid().as_raw() && meta.mode() & 0o077 == 0,
        "untrusted prepared session token"
    );
    let mut token = String::new();
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(token_path)?
        .take(37)
        .read_to_string(&mut token)?;
    ensure!(
        token.len() == 36 && amc_admission::ledger::valid_name(&token),
        "invalid prepared session token"
    );
    let mut stream = UnixStream::connect(args.rendezvous.join("socket"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let peer = getsockopt(&stream, PeerCredentials)?;
    ensure!(
        peer.uid() == geteuid().as_raw(),
        "prepared waiting peer UID mismatch"
    );
    let origin = ClientIdentity {
        pid: peer.pid(),
        start_ticks: host_native::process_start(peer.pid())?,
    };
    progress(&mut stream, &token, false, None)?;
    let result = (|| -> Result<()> {
        let intent = Intent {
            socket: args.socket,
            id: amc_admission::store::fresh_id()?,
            key: amc_admission::store::fresh_id()?,
        };
        let reply = prepare_ready(&intent, args.profile, &signals, || {
            progress(&mut stream, &token, false, None)
        })?;
        // Pin the original host PID/start pair around native registration. The
        // waiting process stays alive and cannot execute its command before ack.
        host_native::prepared_origin(&origin, peer.uid())?;
        crate::control::capture(&mut scope_command(&reply, &intent.id, origin.pid)?)
            .context("register prepared scope through the host user bus")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let suffix = format!("/app-amc-prepared-{}.scope", intent.id);
        loop {
            host_native::prepared_origin(&origin, peer.uid())?;
            if fs::read_to_string(format!("/proc/{}/cgroup", origin.pid))?
                .lines()
                .any(|line| {
                    line.strip_prefix("0::")
                        .is_some_and(|group| group.ends_with(&suffix))
                })
            {
                break;
            }
            ensure!(
                Instant::now() < deadline && signals.cancelled().is_none(),
                "prepared native scope did not start"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let reply = broker_call(
            &intent.socket,
            &Request::Consume {
                version: 1,
                id: intent.id.clone(),
                key: intent.key.clone(),
                origin: Some(origin),
            },
            Instant::now() + Duration::from_secs(30),
            || signals.cancelled().is_some(),
        )?;
        ensure!(reply.granted, "broker did not transfer prepared capacity");
        progress(&mut stream, &token, true, None)
    })();
    if let Err(error) = &result {
        let _ = progress(&mut stream, &token, false, Some(format!("{error:#}")));
    }
    result.map(|()| 0)
}

pub(super) fn scope_command(reply: &Response, id: &str, pid: i32) -> Result<Command> {
    let p = reply
        .preparation
        .as_ref()
        .context("broker omitted ready envelope")?;
    let slice = reply
        .launch_slice
        .as_deref()
        .context("broker omitted launch slice")?;
    ensure!(
        pid > 0
            && amc_admission::ledger::valid_name(id)
            && slice
                .strip_suffix(".slice")
                .is_some_and(amc_admission::ledger::valid_name),
        "invalid prepared native registration"
    );
    let mut command = Command::new("busctl");
    // busctl uses the D-Bus bus-client protocol (including Hello). The manager's
    // systemd/private socket is a peer endpoint and rejects those requests.
    command
        .arg(format!("--address=unix:path=/run/user/{}/bus", geteuid()))
        .args([
            "--timeout=5s",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
        ])
        .arg(format!("app-amc-prepared-{id}.scope"))
        .args(["fail", "8", "Slice", "s", slice, "MemoryMax", "t"])
        .arg(p.memory_bytes.to_string())
        .args(["MemorySwapMax", "t"])
        .arg(p.swap_bytes.to_string())
        .args([
            "MemoryAccounting",
            "b",
            "true",
            "OOMPolicy",
            "s",
            "kill",
            "TimeoutStopUSec",
            "t",
            "15000000",
            "CollectMode",
            "s",
            "inactive-or-failed",
            "PIDs",
            "au",
            "1",
        ])
        .arg(pid.to_string())
        .arg("0");
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_requires_each_matching_ack_and_rejects_disconnected_waiters() {
        let (mut host, mut waiter) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let thread = thread::spawn(move || {
            progress(&mut host, "session", false, None).unwrap();
            assert!(progress(&mut host, "session", true, None).is_err());
        });
        let first: Progress = read_frame(&mut waiter).unwrap();
        assert!(!first.granted);
        write_frame(&mut waiter, &"session").unwrap();
        let second: Progress = read_frame(&mut waiter).unwrap();
        assert!(second.granted);
        write_frame(&mut waiter, &"stale-session").unwrap();
        thread.join().unwrap();

        let (mut host, waiter) = UnixStream::pair().unwrap();
        drop(waiter);
        assert!(progress(&mut host, "session", false, None).is_err());
    }
}
