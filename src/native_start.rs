//! Exec-only startup helper. It is not a reservation or recovery authority.
use anyhow::{Context, Result, ensure};
use nix::{
    sys::socket::{getsockopt, sockopt::PeerCredentials},
    unistd::geteuid,
};
use std::{
    ffi::OsString,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
};

/// Native startup pinning needs the same PID/UID view as the user manager.
/// A namespace-local caller retains its streams and waits for a bounded host
/// runner; that runner performs the original admission and lifetime handshake.
pub fn host_execution() -> Result<Option<i32>> {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Ok(None);
    };
    let runtime = PathBuf::from(runtime);
    let Some(uid) = runtime
        .strip_prefix("/run/user")
        .ok()
        .and_then(|path| path.to_str())
        .and_then(|name| name.parse::<u32>().ok())
    else {
        return Ok(None);
    };
    let bus = runtime.join("bus");
    // The kernel reports PID zero when a host manager is invisible in the
    // caller's descendant PID namespace. This does not trust a caller PID map.
    let manager = match UnixStream::connect(&bus) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    let peer = getsockopt(&manager, PeerCredentials)?;
    if peer.pid() > 0 && uid == geteuid().as_raw() {
        return Ok(None);
    }
    let address = format!("unix:path={}", bus.display());
    ensure!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .ok()
            .is_none_or(|value| { value == address || value.starts_with(&format!("{address},")) }),
        "namespace launch requires the host user-manager session bus"
    );
    let unit = format!(
        "app-amc-host-runner-{}.service",
        amc_admission::store::fresh_id()?
    );
    let socket = runtime.join(format!(
        "amc-host-runner-{}.socket",
        amc_admission::store::fresh_id()?
    ));
    let barrier = amc_runner::systemd::StartupBarrier::host_runner(socket.clone())?;
    let start = amc_admission::host_native::process_start(std::process::id() as i32)?;
    let arguments = [
        OsString::from("native-host"),
        OsString::from("--socket"),
        socket.into_os_string(),
        OsString::from("--parent-start"),
        OsString::from(start.to_string()),
        OsString::from("--"),
    ];
    let command = host_command(
        &unit,
        &bus,
        &std::env::current_exe()?,
        arguments.into_iter().chain(std::env::args_os().skip(1)),
    )?;
    let signals = crate::control::Signals::install()?;
    let mut command = command;
    let outcome = amc_runner::systemd::execute(
        &mut command,
        Path::new("systemctl"),
        &unit,
        false,
        false,
        || signals.cancelled(),
        &amc_runner::systemd::ClientRecord::with_host_startup(barrier, uid),
    );
    match outcome {
        amc_runner::systemd::Outcome::Completed(code) => Ok(Some(code)),
        amc_runner::systemd::Outcome::Cancelled { signal, .. }
        | amc_runner::systemd::Outcome::NotSubmitted(
            amc_runner::systemd::NotSubmitted::Cancelled(signal),
        ) => Ok(Some(128 + signal)),
        outcome => anyhow::bail!("host-native runner {outcome:?} for {unit}"),
    }
}

#[derive(Debug, clap::Args)]
pub struct HostArgs {
    #[arg(long)]
    pub socket: PathBuf,
    #[arg(long)]
    pub parent_start: u64,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub arguments: Vec<OsString>,
}

fn host_command(
    unit: &str,
    bus: &Path,
    executable: &Path,
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Command> {
    let mut command = Command::new("systemd-run");
    let address = format!("unix:path={}", bus.display());
    command.env("DBUS_SESSION_BUS_ADDRESS", &address);
    command.args([
        "--user",
        "--quiet",
        "--wait",
        "--pipe",
        "--same-dir",
        "--expand-environment=no",
        "--service-type=exec",
    ]);
    command.arg(format!("--unit={unit}"));
    for property in [
        "MemoryMax=67108864",
        "MemorySwapMax=0",
        "MemoryAccounting=yes",
        "OOMPolicy=kill",
        "KillMode=control-group",
        "Restart=no",
        "TimeoutStartSec=30s",
        "TimeoutStopSec=30s",
    ] {
        command.arg(format!("--property={property}"));
    }
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if amc_runner::systemd::valid_environment_name(name)
            && !name.starts_with("SYSTEMD_")
            && !name.starts_with("LISTEN_")
            && !matches!(
                name,
                "INVOCATION_ID"
                    | "MANAGERPID"
                    | "JOURNAL_STREAM"
                    | "NOTIFY_SOCKET"
                    | "WATCHDOG_PID"
                    | "WATCHDOG_USEC"
                    | "DBUS_SESSION_BUS_ADDRESS"
            )
        {
            let mut assignment = OsString::from(format!("--setenv={name}="));
            assignment.push(value);
            command.arg(assignment);
        }
    }
    command.arg(format!("--setenv=DBUS_SESSION_BUS_ADDRESS={address}"));
    command.arg("--").arg(executable).args(arguments);
    Ok(command)
}

#[derive(Debug, clap::Args)]
pub struct NativeStartArgs {
    #[arg(long)]
    socket: String,
    #[arg(long)]
    parent: i32,
    #[arg(long)]
    parent_start: u64,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

pub fn execute(arguments: NativeStartArgs) -> Result<i32> {
    ensure!(
        arguments.parent > 0
            && amc_admission::host_native::process_start(arguments.parent)?
                == arguments.parent_start,
        "startup runner identity changed"
    );
    amc_runner::systemd::wait_for_startup(&arguments.socket, arguments.parent as u32)?;
    ensure!(
        amc_admission::host_native::process_start(arguments.parent)? == arguments.parent_start,
        "startup runner disappeared before exec"
    );
    Err(Command::new(&arguments.command[0])
        .args(&arguments.command[1..])
        .exec())
    .with_context(|| format!("execute native workload {:?}", arguments.command[0]))
}

/// The same outer helper wraps admitted entry and ordinary native tools. Both
/// exec in place after capture; existing admission checks still precede payload.
pub fn wrap(client: &mut Command) -> Result<amc_runner::systemd::ClientRecord> {
    let barrier = amc_runner::systemd::StartupBarrier::new(format!(
        "amc-start-{}",
        amc_admission::store::fresh_id()?
    ))?;
    let parent = std::process::id() as i32;
    let start = amc_admission::host_native::process_start(parent)?;
    client.arg("--").arg(std::env::current_exe()?).args([
        "native-start",
        "--socket",
        barrier.name(),
        "--parent",
        &parent.to_string(),
        "--parent-start",
        &start.to_string(),
        "--",
    ]);
    Ok(amc_runner::systemd::ClientRecord::with_startup(barrier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_runner_keeps_literal_command_streams_and_bounded_zero_swap_enforcement() {
        let argv = [
            "admission",
            "exec",
            "--contract",
            "small",
            "--",
            "/payload",
            "$literal; --unit=foreign.service",
        ];
        let command = host_command(
            "app-amc-host-runner-test.service",
            Path::new("/run/user/1000/bus"),
            Path::new("/nix/store/candidate/bin/amc"),
            argv.map(OsString::from),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        let end = args.iter().position(|a| *a == "--").unwrap();
        assert_eq!(args[end + 1], "/nix/store/candidate/bin/amc");
        assert_eq!(&args[end + 2..], argv.map(OsString::from));
        for required in [
            "--wait",
            "--pipe",
            "--expand-environment=no",
            "--property=MemoryMax=67108864",
            "--property=MemorySwapMax=0",
            "--property=Restart=no",
            "--property=KillMode=control-group",
        ] {
            assert!(args.iter().any(|a| *a == required), "{required}");
        }
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == "DBUS_SESSION_BUS_ADDRESS")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("unix:path=/run/user/1000/bus"))
        );
    }
}
