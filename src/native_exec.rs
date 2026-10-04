//! Native per-call isolation, with opt-in admission through the existing broker.
use amc_runner::systemd::{Outcome, valid_environment_name};
use anyhow::{Result, ensure};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, clap::Args)]
pub struct ExecArgs {
    /// Legacy isolation-only mode; requires explicit native bounds.
    #[arg(long, requires_all = ["memory_max", "memory_swap_max", "runtime_max_sec"],
        conflicts_with_all = ["burst", "max_ram_usage", "contract", "socket"])]
    slice: Option<String>,
    #[arg(long, requires = "slice", value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    memory_max: Option<u64>,
    #[arg(long, requires = "slice", value_parser = clap::value_parser!(u64).range(0..=i64::MAX as u64))]
    memory_swap_max: Option<u64>,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86400))]
    runtime_max_sec: Option<u64>,
    /// Suggest short-call burst admission; the broker owns eligibility.
    #[arg(long, requires = "max_ram_usage")]
    burst: bool,
    #[arg(long, value_parser = crate::admission::parse_memory)]
    max_ram_usage: Option<u64>,
    #[arg(long)]
    contract: Option<String>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

pub fn execute(arguments: ExecArgs) -> Result<i32> {
    let Some(slice) = arguments.slice else {
        return crate::admission::execute(crate::admission::AdmissionCommand::Exec {
            socket: arguments.socket,
            contract: arguments.contract.unwrap_or_else(|| {
                if arguments.burst {
                    "tool-burst"
                } else {
                    "tool"
                }
                .into()
            }),
            timeout: arguments.timeout,
            runtime_max_sec: arguments.runtime_max_sec,
            burst: arguments.burst,
            max_ram_usage: arguments.max_ram_usage,
            command: arguments.command,
        });
    };
    let unit = format!(
        "app-amc-native-{}.service",
        amc_admission::store::fresh_id()?
    );
    ensure!(
        slice
            .strip_suffix(".slice")
            .is_some_and(amc_admission::ledger::valid_name),
        "invalid native tool slice"
    );
    let mut client = Command::new("systemd-run");
    client.args([
        "--user",
        "--quiet",
        "--collect",
        "--wait",
        "--pipe",
        "--same-dir",
        "--expand-environment=no",
        "--service-type=exec",
    ]);
    client.arg(format!("--unit={unit}"));
    for property in [
        format!("Slice={slice}"),
        format!(
            "MemoryMax={}",
            arguments
                .memory_max
                .ok_or_else(|| anyhow::anyhow!("missing native memory ceiling"))?
        ),
        format!(
            "MemorySwapMax={}",
            arguments
                .memory_swap_max
                .ok_or_else(|| anyhow::anyhow!("missing native swap ceiling"))?
        ),
        format!(
            "RuntimeMaxSec={}s",
            arguments
                .runtime_max_sec
                .ok_or_else(|| anyhow::anyhow!("missing native deadline"))?
        ),
        "MemoryAccounting=yes".into(),
        "OOMPolicy=kill".into(),
        "KillMode=control-group".into(),
        "Restart=no".into(),
        "TimeoutStartSec=30s".into(),
        "TimeoutStopSec=15s".into(),
    ] {
        client.arg(format!("--property={property}"));
    }
    for (name, _) in std::env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if valid_environment_name(name)
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
            )
        {
            client.arg(format!("--setenv={name}"));
        }
    }
    client.arg("--").args(arguments.command);
    let signals = crate::control::Signals::install()?;
    match amc_runner::systemd::execute(
        &mut client,
        Path::new("systemctl"),
        &unit,
        false,
        false,
        || signals.cancelled(),
        &Default::default(),
    ) {
        Outcome::Completed(code) => Ok(code),
        Outcome::Cancelled { signal, .. }
        | Outcome::NotSubmitted(amc_runner::systemd::NotSubmitted::Cancelled(signal)) => {
            Ok(128 + signal)
        }
        outcome => anyhow::bail!("native tool execution {outcome:?} for {unit}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        arguments: ExecArgs,
    }

    #[test]
    fn burst_and_legacy_isolation_are_explicit_and_cannot_be_mixed() {
        assert!(
            Cli::try_parse_from([
                "exec",
                "--burst",
                "--max-ram-usage",
                "512MiB",
                "--runtime-max-sec",
                "5",
                "--",
                "true"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "exec",
                "--slice",
                "agent-tools.slice",
                "--memory-max",
                "67108864",
                "--memory-swap-max",
                "0",
                "--runtime-max-sec",
                "30",
                "--",
                "true"
            ])
            .is_ok()
        );
        for args in [
            vec!["exec", "--burst", "--", "true"],
            vec!["exec", "--max-ram-usage", "0B", "--", "true"],
            vec!["exec", "--slice", "agent-tools.slice", "--", "true"],
            vec!["exec", "--memory-max", "67108864", "--", "true"],
            vec![
                "exec",
                "--slice",
                "agent-tools.slice",
                "--memory-max",
                "67108864",
                "--memory-swap-max",
                "0",
                "--runtime-max-sec",
                "5",
                "--burst",
                "--max-ram-usage",
                "512MiB",
                "--",
                "true",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}
