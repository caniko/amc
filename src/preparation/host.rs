//! The manager executes this small helper in the host namespaces. The payload
//! waits locally, so neither its environment nor its filesystem/PID view moves.
use super::{Intent, PrepareArgs, broker_call, exec_payload, prepare_ready};
use amc_admission::{
    host_native,
    host_server::{PreparationAuthorization, Request, Response},
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
        ffi::OsStrExt,
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
    #[serde(default)]
    authorize: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    consume_proof: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationAck {
    token: String,
    key: String,
}

fn verify_progress(reply: &Progress, token: &str, consume_proof: Option<&str>) -> Result<()> {
    ensure!(reply.token == token, "prepared helper session mismatch");
    ensure!(
        if reply.granted {
            !reply.authorize
                && consume_proof.is_some()
                && reply.consume_proof.as_deref() == consume_proof
        } else {
            reply.consume_proof.is_none()
        },
        "prepared grant has no matching broker consumption proof"
    );
    Ok(())
}

// Each frame is acknowledged before another is sent: read_frame is deliberately
// one-frame-only, and this prevents coalesced progress/ready frames being lost.
fn progress(
    stream: &mut UnixStream,
    token: &str,
    granted: bool,
    error: Option<String>,
    consume_proof: Option<String>,
) -> Result<()> {
    write_frame(
        stream,
        &Progress {
            token: token.into(),
            granted,
            error,
            authorize: false,
            consume_proof,
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
        let _ = fs::remove_file(self.0.join("authorize"));
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
    let authorization = UnixListener::bind(rendezvous.0.join("authorize"))?;
    fs::set_permissions(
        rendezvous.0.join("authorize"),
        fs::Permissions::from_mode(0o600),
    )?;
    authorization.set_nonblocking(true)?;
    // No payload, loader environment, or preparation capability is sent through
    // the manager. Its helper obtains the waiting peer's host PID from the kernel.
    crate::control::capture(&mut helper_command(&args, &rendezvous.0, &id)?)
        .context("start prepared helper through the host user bus")?;
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
    let mut consume_proof = None;
    loop {
        let reply: Progress = read_frame(&mut stream)?;
        verify_progress(&reply, &token, consume_proof.as_deref())?;
        if let Some(signal) = signals.cancelled() {
            return Ok(128 + signal);
        }
        if let Some(error) = reply.error {
            anyhow::bail!("prepared host helper: {error}");
        }
        if reply.authorize {
            ensure!(
                consume_proof.is_none(),
                "repeated prepared authorization frame"
            );
            let mut callback = loop {
                ensure!(
                    Instant::now() < deadline && signals.cancelled().is_none(),
                    "prepared authorization callback missing"
                );
                match authorization.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            // Use the socket owner's mapped UID, so this works where initial
            // host root is unmapped in the waiting process's user namespace.
            ensure!(
                getsockopt(&callback, PeerCredentials)?.uid()
                    == fs::symlink_metadata(&args.socket)?.uid(),
                "prepared authorization callback UID mismatch"
            );
            callback.set_read_timeout(Some(Duration::from_secs(2)))?;
            let receipt: PreparationAuthorization = read_frame(&mut callback)?;
            ensure!(
                receipt.version == 1
                    && receipt.id == id
                    && receipt.key.len() == 36
                    && amc_admission::ledger::valid_name(&receipt.key)
                    && receipt.consume_proof.len() == 36
                    && amc_admission::ledger::valid_name(&receipt.consume_proof)
                    && receipt.consume_proof != receipt.key,
                "invalid prepared authorization receipt"
            );
            consume_proof = Some(receipt.consume_proof);
            write_frame(
                &mut stream,
                &AuthorizationAck {
                    token: token.clone(),
                    key: receipt.key,
                },
            )?;
            continue;
        }
        write_frame(&mut stream, &token)?;
        if reply.granted {
            break;
        }
    }
    drop(stream);
    drop(listener);
    drop(authorization);
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
    ensure!(
        amc_admission::host_server::call(&args.socket, &Request::NamespaceRunner { version: 1 })?
            .granted,
        "preparation helper has no backed aggregate allowance"
    );
    let result = (|| -> Result<()> {
        let id = args
            .rendezvous
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("amc-"))
            .context("invalid prepared rendezvous name")?
            .to_owned();
        ensure!(
            amc_admission::ledger::valid_name(&id)
                && args.rendezvous
                    == std::path::Path::new(&format!("/run/user/{}/amc-{id}", peer.uid())),
            "prepared rendezvous must be in the host user's runtime directory"
        );
        let authorized = broker_call(
            &args.socket,
            &Request::AuthorizePreparation {
                version: 1,
                id: id.clone(),
                profile: args.profile.clone(),
                origin: origin.clone(),
            },
            Instant::now() + Duration::from_secs(30),
            || signals.cancelled().is_some(),
        )?;
        ensure!(
            authorized.granted,
            "broker did not authorize prepared rendezvous"
        );
        write_frame(
            &mut stream,
            &Progress {
                token: token.clone(),
                granted: false,
                error: None,
                authorize: true,
                consume_proof: None,
            },
        )?;
        let ack: AuthorizationAck = read_frame(&mut stream)?;
        ensure!(
            ack.token == token
                && ack.key.len() == 36
                && amc_admission::ledger::valid_name(&ack.key),
            "prepared rendezvous authorization acknowledgement mismatch"
        );
        let intent = Intent {
            socket: args.socket,
            id,
            key: ack.key,
        };
        let reply = prepare_ready(&intent, args.profile, &signals, || {
            progress(&mut stream, &token, false, None, None)
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
        progress(
            &mut stream,
            &token,
            true,
            None,
            reply.preparation_consume_proof,
        )
    })();
    if let Err(error) = &result {
        let _ = progress(&mut stream, &token, false, Some(format!("{error:#}")), None);
    }
    result.map(|()| 0)
}

fn manager_command(runtime: &std::path::Path) -> Command {
    // An explicit session-bus connection also works inside a private PID
    // namespace, where systemd's automatic private-peer credential check cannot
    // see the manager's PID. Escape the pathname as one D-Bus address value.
    let mut address = String::from("--address=unix:path=");
    for byte in runtime.join("bus").as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/_-.".contains(byte) {
            address.push(char::from(*byte));
        } else {
            const HEX: &[u8] = b"0123456789abcdef";
            address.push('%');
            address.push(char::from(HEX[usize::from(byte >> 4)]));
            address.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    let mut command = Command::new("busctl");
    command.arg(address).args([
        "--timeout=5s",
        // Stop busctl option parsing before the helper's literal --arguments.
        "--",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
    ]);
    command
}

pub(super) fn helper_command(
    args: &PrepareArgs,
    rendezvous: &std::path::Path,
    id: &str,
) -> Result<Command> {
    let runtime = rendezvous
        .parent()
        .context("missing prepared runtime directory")?;
    ensure!(
        runtime.is_absolute() && amc_admission::ledger::valid_name(id),
        "invalid prepared helper registration"
    );
    let executable = std::env::current_exe()?;
    let mut command = manager_command(runtime);
    command
        .arg(format!("app-amc-prepare-helper-{id}.service"))
        .args([
            "fail",
            "7",
            "Slice",
            "s",
            "app-amchostrunner.slice",
            "Type",
            "s",
            "exec",
            "MemoryMax",
            "t",
            "67108864",
            "MemorySwapMax",
            "t",
            "0",
            "RuntimeMaxUSec",
            "t",
            // Maximum one-hour wait, one-minute ready window, and two minutes
            // of startup/registration/consume grace remain natively bounded.
            "3780000000",
            "CollectMode",
            "s",
            "inactive-or-failed",
            "ExecStartEx",
            "a(sasas)",
            "1",
        ])
        .arg(&executable)
        .arg("7")
        .arg(&executable)
        .arg("prepared-host")
        .arg("--rendezvous")
        .arg(rendezvous)
        .arg("--socket")
        .arg(&args.socket)
        .arg(format!("--profile={}", args.profile))
        .args(["1", "no-env-expand", "0"]);
    Ok(command)
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
    let mut command = manager_command(&PathBuf::from(format!("/run/user/{}", geteuid())));
    command
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
    fn a_same_uid_progress_peer_cannot_grant_without_broker_consumption() {
        let (mut attacker, mut waiter) = UnixStream::pair().unwrap();
        write_frame(
            &mut attacker,
            &Progress {
                token: "known-rendezvous-token".into(),
                granted: true,
                error: None,
                authorize: false,
                consume_proof: None,
            },
        )
        .unwrap();
        let mut forged: Progress = read_frame(&mut waiter).unwrap();
        assert!(
            verify_progress(&forged, "known-rendezvous-token", None).is_err(),
            "the shared token must not open the payload gate without broker authority"
        );
        assert!(
            verify_progress(
                &forged,
                "known-rendezvous-token",
                Some("private-consume-proof")
            )
            .is_err(),
            "even after authorization, knowing the helper key must not prove consumption"
        );
        forged.consume_proof = Some("helper-authorization-key".into());
        assert!(
            verify_progress(
                &forged,
                "known-rendezvous-token",
                Some("private-consume-proof")
            )
            .is_err()
        );
        forged.consume_proof = Some("private-consume-proof".into());
        assert!(verify_progress(&forged, "known-rendezvous-token", None).is_err());
        verify_progress(
            &forged,
            "known-rendezvous-token",
            Some("private-consume-proof"),
        )
        .unwrap();
        forged.authorize = true;
        assert!(
            verify_progress(
                &forged,
                "known-rendezvous-token",
                Some("private-consume-proof")
            )
            .is_err()
        );
    }

    #[test]
    fn generated_helper_argv_parses_literal_hyphen_leading_profiles() {
        use clap::Parser;
        for profile in ["game-$literal", "-game", "--game", "--"] {
            let args = PrepareArgs {
                socket: "/run/amc-host/admission.sock".into(),
                profile: profile.into(),
                payload_env: vec![],
                command: vec!["payload".into()],
            };
            let command = helper_command(
                &args,
                std::path::Path::new("/run/user/1000/amc-intent"),
                "intent",
            )
            .unwrap();
            let arguments: Vec<_> = command.get_args().collect();
            let index = arguments
                .iter()
                .position(|arg| *arg == "ExecStartEx")
                .unwrap();
            let argc: usize = arguments[index + 4].to_str().unwrap().parse().unwrap();
            let parsed =
                crate::Cli::try_parse_from(arguments[index + 5..index + 5 + argc].iter().copied())
                    .unwrap();
            let crate::Command::PreparedHost(helper) = parsed.command else {
                panic!("wrong helper command")
            };
            assert_eq!(helper.profile, profile);
        }
    }

    #[test]
    fn progress_requires_each_matching_ack_and_rejects_disconnected_waiters() {
        let (mut host, mut waiter) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let thread = thread::spawn(move || {
            progress(&mut host, "session", false, None, None).unwrap();
            assert!(progress(&mut host, "session", true, None, Some("proof".into())).is_err());
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
        assert!(progress(&mut host, "session", false, None, None).is_err());
    }
}
