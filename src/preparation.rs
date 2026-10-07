//! Gate payload creation before native launch, then transfer once inside it.
mod host;
pub use host::HostArgs;

use amc_admission::host_server::{Request, Response, call, transient_transport};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, clap::Args)]
pub struct PrepareArgs {
    #[arg(long, default_value = "/run/amc-host/admission.sock")]
    socket: PathBuf,
    #[arg(long)]
    profile: String,
    /// Apply NAME=VALUE only to the admitted payload, after the launch barrier.
    #[arg(long, value_parser = payload_environment)]
    payload_env: Vec<String>,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

#[derive(Debug, clap::Args)]
pub struct EnterArgs {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    ticket: String,
    #[arg(long)]
    key: String,
    #[arg(long, value_parser = payload_environment)]
    payload_env: Vec<String>,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

struct Intent {
    socket: PathBuf,
    id: String,
    key: String,
}

impl Drop for Intent {
    fn drop(&mut self) {
        // After transfer this merely marks reconciliation; native cleanup owns release.
        let _ = call(
            &self.socket,
            &Request::CancelPreparation {
                version: 1,
                id: self.id.clone(),
                key: self.key.clone(),
            },
        );
    }
}

pub fn execute(args: PrepareArgs) -> Result<i32> {
    host::execute(args)
}

pub fn host(args: HostArgs) -> Result<i32> {
    host::serve(args)
}

fn prepare_ready(
    intent: &Intent,
    profile: String,
    signals: &crate::control::Signals,
    mut pulse: impl FnMut() -> Result<()>,
) -> Result<Response> {
    let mut deadline = Instant::now() + Duration::from_secs(30);
    let mut reply = broker_call(
        &intent.socket,
        &Request::Prepare {
            version: 1,
            id: intent.id.clone(),
            key: intent.key.clone(),
            profile,
        },
        deadline,
        || signals.cancelled().is_some(),
    )?;
    if let Some(p) = &reply.preparation {
        deadline = Instant::now()
            + Duration::from_millis(
                p.expires_ms
                    .saturating_sub(amc_admission::clock::boot_ms()?),
            );
    }
    let mut previous = None;
    while !reply.granted {
        ensure!(signals.cancelled().is_none(), "preparation cancelled");
        pulse()?;
        if previous != reply.waiting {
            eprintln!("preparing foreground capacity: {:?}", reply.waiting);
            previous = reply.waiting;
        }
        thread::sleep(Duration::from_millis(250));
        reply = broker_call(
            &intent.socket,
            &Request::Preparation {
                version: 1,
                id: intent.id.clone(),
                key: intent.key.clone(),
            },
            deadline,
            || signals.cancelled().is_some(),
        )?;
    }
    ensure!(signals.cancelled().is_none(), "preparation cancelled");
    Ok(reply)
}

pub fn enter(args: EnterArgs) -> Result<i32> {
    let reply = broker_call(
        &args.socket,
        &Request::Consume {
            version: 1,
            id: args.ticket,
            key: args.key,
            origin: None,
        },
        Instant::now() + Duration::from_secs(30),
        || false,
    )?;
    ensure!(reply.granted, "broker did not transfer prepared capacity");
    exec_payload(args.command, args.payload_env)
}

fn exec_payload(command: Vec<OsString>, payload_env: Vec<String>) -> Result<i32> {
    let mut payload = Command::new(&command[0]);
    payload.args(&command[1..]);
    payload.env_remove("AMC_CONTINUATION");
    for entry in payload_env {
        let (name, value) = entry
            .split_once('=')
            .context("invalid payload environment")?;
        payload.env(name, value);
    }
    Err(payload.exec()).context("exec prepared payload")
}

fn broker_call(
    socket: &Path,
    request: &Request,
    deadline: Instant,
    stopping: impl Fn() -> bool,
) -> Result<Response> {
    loop {
        ensure!(
            !stopping() && Instant::now() < deadline,
            "preparation cancelled or timed out"
        );
        match call(socket, request) {
            Err(e) if transient_transport(&e) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(100))
            }
            result => return result,
        }
    }
}

fn payload_environment(value: &str) -> std::result::Result<String, String> {
    let (name, _) = value
        .split_once('=')
        .ok_or_else(|| "payload environment requires NAME=VALUE".to_owned())?;
    if !amc_runner::systemd::valid_environment_name(name)
        || name.starts_with("AMC_")
        || name.starts_with("SYSTEMD_")
        || name.starts_with("LISTEN_")
        || matches!(
            name,
            "INVOCATION_ID"
                | "MANAGERPID"
                | "JOURNAL_STREAM"
                | "NOTIFY_SOCKET"
                | "WATCHDOG_PID"
                | "WATCHDOG_USEC"
        )
    {
        return Err("invalid or reserved payload environment name".into());
    }
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_registration_uses_the_waiting_host_pid_and_only_the_broker_envelope() {
        let reply: Response = serde_json::from_value(serde_json::json!({
            "version":1,"granted":true,"ticket":"intent","waiting":null,"committed_bytes":64,"reservations":null,"error":null,
            "launch_slice":"app-amcforeground.slice",
            "preparation":{"id":"intent","key":"","uid":1000,"profile":"game","domain":"foreground",
                "memory_bytes":64,"swap_bytes":0,"requested_ms":0,"expires_ms":1000,"ready_ms":1000,"phase":"ready","drain":[],"waiting":null}
        })).unwrap();
        let command = host::scope_command(&reply, "intent", 42).unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.contains(&"app-amc-prepared-intent.scope".into()),
            "{args:?}"
        );
        assert!(args.contains(&format!(
            "--address=unix:path=/run/user/{}/bus",
            nix::unistd::geteuid()
        )));
        assert!(
            args.windows(3)
                .any(|parts| parts == ["MemoryMax", "t", "64"])
        );
        assert!(
            args.windows(4)
                .any(|parts| parts == ["PIDs", "au", "1", "42"])
        );
        assert!(!args.iter().any(|s| s == "ExecStart" || s == "Environment"));
        assert!(host::scope_command(&reply, "intent", 0).is_err());
        assert!(host::scope_command(&reply, "bad/name", 42).is_err());
    }
    #[test]
    fn gamemode_environment_is_payload_only_and_cannot_replace_native_authority() {
        assert!(payload_environment("LD_PRELOAD=libgamemodeauto.so.0").is_ok());
        assert!(payload_environment("DISPLAY=:0").is_ok());
        for value in [
            "LD_PRELOAD",
            "=bad",
            "AMC_CONTINUATION=forged",
            "SYSTEMD_UNIT=other",
            "NOTIFY_SOCKET=other",
        ] {
            assert!(payload_environment(value).is_err());
        }
    }
    #[test]
    fn restarting_broker_transport_is_retryable_but_a_refused_capability_is_not() {
        assert!(transient_transport(
            &std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into()
        ));
        assert!(transient_transport(&anyhow::anyhow!(
            "incomplete admission frame"
        )));
        assert!(!transient_transport(&anyhow::anyhow!(
            "preparation owner mismatch"
        )));
        assert!(!transient_transport(&anyhow::anyhow!(
            "unsupported host admission response"
        )));
    }
}
