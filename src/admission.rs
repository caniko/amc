//! Production admission CLI. The native entry handshake gates execution.
use amc_admission::{
    ledger::{Phase, Policy},
    native::Systemd,
    protocol::{Message, call},
    server,
    store::MAX_STATE_BYTES,
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use std::{
    ffi::OsString,
    fs::File,
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Subcommand)]
pub enum AdmissionCommand {
    /// Serve ceiling-backed reservations shared by enrolled execution domains.
    HostServe {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long, default_value = "/run/amc-host/admission.sock")]
        socket: PathBuf,
        #[arg(long, default_value = "/var/lib/amc-host")]
        state: PathBuf,
        #[arg(long)]
        health_file: Option<PathBuf>,
    },
    /// Inspect the root broker's reservations and commitments.
    HostStatus {
        #[arg(long, default_value = "/run/amc-host/admission.sock")]
        socket: PathBuf,
    },
    /// Serve a durable private per-user admission endpoint.
    Serve {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long, default_value = "systemctl")]
        systemctl: PathBuf,
        /// Fail closed when this host supervisor heartbeat is missing/stale/inhibited.
        #[arg(long)]
        health_file: Option<PathBuf>,
        /// Require shared host capacity before a native entry can execute.
        #[arg(long)]
        host_socket: Option<PathBuf>,
    },
    /// Show reservations and admission state without changing workloads.
    Status {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Admit and execute one native service using an operator-defined contract.
    Exec {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        contract: String,
        /// Maximum admission wait; does not bound workload execution.
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        /// Optional systemd execution deadline for a disposable job, in seconds.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86400))]
        runtime_max_sec: Option<u64>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    #[command(hide = true)]
    Enter {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        ticket: String,
        #[arg(long)]
        entry_key: String,
        #[arg(long)]
        host_socket: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
}

fn socket_path(path: Option<PathBuf>) -> Result<PathBuf> {
    path.map(Ok).unwrap_or_else(|| {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is required for admission"))?;
        Ok(PathBuf::from(runtime).join("amc/admission.sock"))
    })
}

pub fn execute(command: AdmissionCommand) -> Result<i32> {
    match command {
        AdmissionCommand::HostServe {
            policy,
            socket,
            state,
            health_file,
        } => {
            let mut bytes = Vec::new();
            File::open(policy)?
                .take(MAX_STATE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_STATE_BYTES,
                "host policy too large"
            );
            let policy = serde_json::from_slice(&bytes)?;
            let signals = crate::control::Signals::install()?;
            amc_admission::host_server::serve_supervised(
                policy,
                &socket,
                &state,
                health_file.as_deref(),
                || signals.cancelled().is_some(),
            )?;
            Ok(0)
        }
        AdmissionCommand::HostStatus { socket } => {
            let status = amc_admission::host_server::call(
                &socket,
                &amc_admission::host_server::Request::Status { version: 1 },
            )?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(0)
        }
        AdmissionCommand::Serve {
            policy,
            socket,
            state,
            systemctl,
            health_file,
            host_socket,
        } => {
            let mut bytes = Vec::new();
            File::open(policy)?
                .take(MAX_STATE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_STATE_BYTES,
                "admission policy too large"
            );
            let policy: Policy = serde_json::from_slice(&bytes)?;
            let state = state.unwrap_or_else(|| {
                std::env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                            .join(".local/state")
                    })
                    .join("amc/admission")
            });
            ensure!(
                state.is_absolute(),
                "admission state directory must be absolute"
            );
            let signals = crate::control::Signals::install()?;
            server::serve_with_host(
                policy,
                &socket_path(socket)?,
                &state,
                amc_admission::native::Supervised {
                    native: Systemd { systemctl },
                    health_file,
                },
                host_socket.as_deref(),
                || signals.cancelled().is_some(),
            )?;
            Ok(0)
        }
        AdmissionCommand::Status { socket, json } => {
            let status = call(&socket_path(socket)?, Message::Status)?
                .status
                .ok_or_else(|| anyhow::anyhow!("missing admission status"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "Admission: {} / {} bytes committed; {} tracked jobs",
                    status.committed_bytes,
                    status.budget_bytes,
                    status.entries.len()
                );
                for entry in status.entries {
                    println!(
                        "{} {} {:?} {} bytes",
                        entry.unit(),
                        entry.name,
                        entry.phase,
                        entry.contract.memory_max
                    );
                }
                for (id, decision) in status.waiting {
                    println!("Waiting: {id} {:?}", decision.reason);
                }
                for id in status.unreconciled {
                    println!("Unreconciled: {id}");
                }
            }
            Ok(0)
        }
        AdmissionCommand::Exec {
            socket,
            contract,
            timeout,
            runtime_max_sec,
            command,
        } => run(
            &socket_path(socket)?,
            &contract,
            timeout,
            runtime_max_sec,
            &command,
        ),
        AdmissionCommand::Enter {
            socket,
            ticket,
            entry_key,
            host_socket,
            command,
        } => {
            // Authorization is persisted before this reply. If the reply is
            // lost, nothing executes and native reconciliation retires us.
            if let Some(host_socket) = host_socket {
                let signals = crate::control::Signals::install()?;
                let remaining = call(&socket, Message::Poll { id: ticket.clone() })?
                    .entry
                    .ok_or_else(|| anyhow::anyhow!("missing entry ticket"))?
                    .deadline_ms
                    .saturating_sub(server::now_ms()?)
                    .saturating_sub(1000);
                amc_admission::host_server::acquire(
                    &host_socket,
                    Duration::from_millis(remaining),
                    || signals.cancelled().is_some(),
                )?;
            }
            call(
                &socket,
                Message::Enter {
                    id: ticket,
                    key: entry_key,
                },
            )?;
            Err(Command::new(&command[0]).args(&command[1..]).exec())
                .with_context(|| format!("execute admitted workload {:?}", command[0]))
        }
    }
}

fn run(
    socket: &Path,
    contract: &str,
    timeout: u64,
    runtime_max_sec: Option<u64>,
    argv: &[OsString],
) -> Result<i32> {
    use amc_runner::systemd::execute;
    let signals = crate::control::Signals::install()?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let response = call(
        socket,
        Message::Enqueue {
            contract: contract.into(),
            wait_ms: timeout * 1000,
        },
    )?;
    let host_socket = response.host_socket;
    let entry_key = response
        .entry_key
        .ok_or_else(|| anyhow::anyhow!("missing admission entry capability"))?;
    let mut entry = response
        .entry
        .ok_or_else(|| anyhow::anyhow!("missing admission ticket"))?;
    while entry.phase == Phase::Queued {
        if signals.cancelled().is_some() || Instant::now() >= deadline {
            let _ = call(socket, Message::Cancel { id: entry.id });
            anyhow::bail!("admission cancelled or timed out before submission");
        }
        thread::sleep(Duration::from_millis(250));
        entry = call(
            socket,
            Message::Poll {
                id: entry.id.clone(),
            },
        )?
        .entry
        .ok_or_else(|| anyhow::anyhow!("missing admission ticket"))?;
    }
    ensure!(
        entry.phase == Phase::Reserved,
        "ticket has already been submitted"
    );
    let mut client = Command::new("systemd-run");
    client.args([
        "--user",
        "--quiet",
        "--wait",
        "--pipe",
        "--same-dir",
        "--expand-environment=no",
        "--service-type=exec",
    ]);
    client.arg(format!("--unit={}", entry.unit()));
    for property in [
        format!("Slice={}", entry.contract.slice),
        format!("MemoryMax={}", entry.contract.memory_max),
        format!("MemorySwapMax={}", entry.contract.memory_swap_max),
        "MemoryAccounting=yes".into(),
        "OOMPolicy=kill".into(),
        "KillMode=control-group".into(),
        "Restart=no".into(),
        format!("TimeoutStartSec={}s", timeout + 30),
        "TimeoutStopSec=15s".into(),
    ] {
        client.arg(format!("--property={property}"));
    }
    if let Some(seconds) = runtime_max_sec {
        client.arg(format!("--property=RuntimeMaxSec={seconds}s"));
    }
    for (name, _) in std::env::vars_os() {
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
            )
        {
            client.arg(format!("--setenv={name}"));
        }
    }
    client
        .arg("--")
        .arg(std::env::current_exe()?)
        .args(["admission", "enter", "--socket"])
        .arg(socket)
        .args(["--ticket", &entry.id, "--entry-key", &entry_key]);
    if let Some(host_socket) = host_socket {
        client.arg("--host-socket").arg(host_socket);
    }
    client.arg("--").args(argv);
    let outcome = execute(
        &mut client,
        Path::new("systemctl"),
        &entry.unit(),
        false,
        true,
        || signals.cancelled(),
        &Default::default(),
    );
    // Cancel only this owned attempt. The server retains an entered workload's
    // reservation until native termination, including on ambiguous outcomes.
    let _ = call(
        socket,
        Message::Cancel {
            id: entry.id.clone(),
        },
    );
    outcome_exit(outcome, &entry.unit())
}

fn outcome_exit(outcome: amc_runner::systemd::Outcome, unit: &str) -> Result<i32> {
    use amc_runner::systemd::Outcome;
    match outcome {
        Outcome::Completed(code) => Ok(code),
        Outcome::Unknown { exit_code, .. } => {
            anyhow::bail!(
                "native outcome unresolved for {unit} (launcher exit {exit_code}); reservation remains coordinator-owned; inspect admission status before retrying"
            )
        }
        Outcome::Cancelled { signal, .. } => Ok(128 + signal),
        Outcome::TimedOut { .. } => {
            anyhow::bail!(
                "native submission timed out for {}; inspect admission status before retrying",
                unit
            )
        }
        _ => {
            anyhow::bail!(
                "native submission {:?} for {}; inspect admission status before retrying",
                outcome,
                unit
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amc_runner::systemd::{Cleanup, Outcome};

    #[test]
    fn unresolved_native_outcome_never_reports_success() {
        for cleanup in [Cleanup::Unknown, Cleanup::Stopped] {
            assert!(
                outcome_exit(
                    Outcome::Unknown {
                        exit_code: 0,
                        cleanup
                    },
                    "fixture.service"
                )
                .is_err()
            );
        }
        assert_eq!(
            outcome_exit(Outcome::Completed(0), "fixture.service").unwrap(),
            0
        );
        assert_eq!(
            outcome_exit(Outcome::Completed(42), "fixture.service").unwrap(),
            42
        );
    }
}
