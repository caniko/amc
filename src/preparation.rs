//! Gate payload creation before native launch, then transfer once inside it.
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
    let signals = crate::control::Signals::install()?;
    let intent = Intent {
        socket: args.socket,
        id: amc_admission::store::fresh_id()?,
        key: amc_admission::store::fresh_id()?,
    };
    let mut deadline = Instant::now() + Duration::from_secs(30);
    let mut reply = broker_call(
        &intent.socket,
        &Request::Prepare {
            version: 1,
            id: intent.id.clone(),
            key: intent.key.clone(),
            profile: args.profile,
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
        if let Some(signal) = signals.cancelled() {
            return Ok(128 + signal);
        }
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
    if let Some(signal) = signals.cancelled() {
        return Ok(128 + signal);
    }
    let unit = format!("app-amc-prepared-{}.scope", intent.id);
    let mut client = launch_client(&reply, &unit)?;
    client
        .arg("--")
        .arg(std::env::current_exe()?)
        .args(["prepared-enter", "--socket"])
        .arg(&intent.socket)
        .args(["--ticket", &intent.id, "--key", &intent.key]);
    for env in args.payload_env {
        client.args(["--payload-env", &env]);
    }
    client.arg("--").args(args.command);
    // Scope mode executes locally after the manager has installed the native
    // envelope. It preserves Steam's mount/PID namespaces and stdio. The entry
    // helper then authenticates Consume before exec; only the broker's native
    // cgroup reconciliation can release transferred capacity or surviving children.
    Err(client.exec()).context("exec prepared native scope")
}

fn launch_client(reply: &Response, unit: &str) -> Result<Command> {
    let p = reply
        .preparation
        .as_ref()
        .context("broker omitted ready envelope")?;
    let slice = reply
        .launch_slice
        .as_ref()
        .context("broker omitted launch slice")?;
    ensure!(
        slice
            .strip_suffix(".slice")
            .is_some_and(amc_admission::ledger::valid_name),
        "invalid prepared slice"
    );
    let mut client = Command::new("systemd-run");
    client
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--scope",
            "--expand-environment=no",
        ])
        .arg(format!("--unit={unit}"));
    for property in [
        format!("Slice={slice}"),
        format!("MemoryMax={}", p.memory_bytes),
        format!("MemorySwapMax={}", p.swap_bytes),
        "MemoryAccounting=yes".into(),
        "OOMPolicy=kill".into(),
        "TimeoutStopSec=15s".into(),
    ] {
        client.arg(format!("--property={property}"));
    }
    Ok(client)
}

pub fn enter(args: EnterArgs) -> Result<i32> {
    let reply = broker_call(
        &args.socket,
        &Request::Consume {
            version: 1,
            id: args.ticket,
            key: args.key,
        },
        Instant::now() + Duration::from_secs(30),
        || false,
    )?;
    ensure!(reply.granted, "broker did not transfer prepared capacity");
    let mut payload = Command::new(&args.command[0]);
    payload.args(&args.command[1..]);
    payload.env_remove("AMC_CONTINUATION");
    for entry in args.payload_env {
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
    fn foreground_launch_preserves_the_callers_namespace_and_native_scope_envelope() {
        let reply: Response = serde_json::from_value(serde_json::json!({
            "version":1,"granted":true,"ticket":"intent","waiting":null,"committed_bytes":64,"reservations":null,"error":null,
            "launch_slice":"app-amcforeground.slice",
            "preparation":{"id":"intent","key":"","uid":1000,"profile":"game","domain":"foreground",
                "memory_bytes":64,"swap_bytes":0,"requested_ms":0,"expires_ms":1000,"ready_ms":1000,"phase":"ready","drain":[],"waiting":null}
        })).unwrap();
        let command = launch_client(&reply, "app-amc-prepared-intent.scope").unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--scope".into()), "{args:?}");
        assert!(args.contains(&"--property=MemoryMax=64".into()));
        assert!(!args.iter().any(|s| s == "--wait"
            || s == "--pipe"
            || s.starts_with("--service-type")
            || s.contains("Restart=")
            || s.contains("ExitType=")));
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
