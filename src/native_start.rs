//! Exec-only startup helper. It is not a reservation or recovery authority.
use anyhow::{Context, Result, ensure};
use std::{ffi::OsString, os::unix::process::CommandExt, process::Command};

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
