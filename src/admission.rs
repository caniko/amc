//! Production admission CLI. The native entry handshake gates execution.
use amc_admission::{
    ledger::{Backpressure, Phase, Policy, QueueRequest},
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
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Subcommand)]
pub enum AdmissionCommand {
    /// Validate a composed host policy without starting a broker or native work.
    HostPolicyCheck {
        #[arg(long)]
        policy: PathBuf,
    },
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
        /// Maximum admission wait; defaults to 120s unless --queue is selected.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: Option<u64>,
        /// Queue bounded work without an implicit resource-wait deadline.
        #[arg(long, requires_all = ["max_ram_usage", "runtime_max_sec"])]
        queue: bool,
        /// Emit native-entry or nonacceptance control frames on stderr.
        #[arg(long, value_parser = parse_ready_token)]
        ready_token: Option<String>,
        /// Optional systemd execution deadline for a disposable job, in seconds.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86400))]
        runtime_max_sec: Option<u64>,
        /// Request the policy-defined short-call burst lane.
        #[arg(long, requires = "max_ram_usage")]
        burst: bool,
        /// Explicit per-call hard ceiling, bounded by the selected contract.
        #[arg(long, value_parser = parse_memory)]
        max_ram_usage: Option<u64>,
        /// Allow policy selection of protected backing for an explicitly bounded call.
        #[arg(long, requires_all = ["max_ram_usage", "runtime_max_sec"], conflicts_with = "burst")]
        protected_pool: bool,
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
        /// Patient native startup uses notify readiness, not exec readiness.
        #[arg(long)]
        queue: bool,
        #[arg(long, value_parser = parse_ready_token)]
        ready_token: Option<String>,
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
        AdmissionCommand::HostPolicyCheck { policy } => {
            let policy = host_policy(&policy)?;
            policy.validate()?;
            println!("host policy valid");
            Ok(0)
        }
        AdmissionCommand::HostServe {
            policy,
            socket,
            state,
            health_file,
        } => {
            let policy = host_policy(&policy)?;
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
            queue,
            ready_token,
            runtime_max_sec,
            burst,
            max_ram_usage,
            protected_pool,
            command,
        } => {
            let wait = timeout.or((!queue).then_some(120));
            let wait_ms = timeout.unwrap_or(120) * 1000;
            let request = if queue {
                Message::EnqueueWaiting {
                    contract,
                    request: QueueRequest {
                        memory_max: max_ram_usage.ok_or_else(|| {
                            anyhow::anyhow!("queued work requires a memory ceiling")
                        })?,
                        runtime_max_sec: runtime_max_sec.ok_or_else(|| {
                            anyhow::anyhow!("queued work requires an execution deadline")
                        })?,
                        wait_ms: wait.map(|seconds| seconds * 1000),
                    },
                }
            } else {
                match (max_ram_usage, protected_pool) {
                    (Some(memory_max), true) => Message::EnqueueBounded {
                        contract,
                        wait_ms,
                        memory_max,
                        runtime_max_sec: runtime_max_sec
                            .ok_or_else(|| anyhow::anyhow!("missing bounded runtime"))?,
                    },
                    (Some(memory_max), false) => Message::EnqueueSized {
                        contract,
                        wait_ms,
                        memory_max,
                    },
                    (None, _) => Message::Enqueue { contract, wait_ms },
                }
            };
            run(
                &socket_path(socket)?,
                wait.map(Duration::from_secs),
                runtime_max_sec,
                burst,
                request,
                &command,
                ready_token.as_deref(),
            )
        }
        AdmissionCommand::Enter {
            socket,
            ticket,
            entry_key,
            host_socket,
            queue,
            ready_token,
            command,
        } => {
            // Authorization is persisted before this reply. If the reply is
            // lost, nothing executes and native reconciliation retires us.
            let signals = (host_socket.is_some() || queue)
                .then(crate::control::Signals::install)
                .transpose()?;
            let mut continuation = None;
            let starting = if queue {
                Some(
                    call(
                        &socket,
                        Message::Start {
                            id: ticket.clone(),
                            key: entry_key.clone(),
                        },
                    )?
                    .entry
                    .ok_or_else(|| anyhow::anyhow!("missing native startup ticket"))?,
                )
            } else {
                None
            };
            if let Some(host_socket) = host_socket {
                let entry = if let Some(entry) = starting.as_ref() {
                    entry.clone()
                } else {
                    call(&socket, Message::Poll { id: ticket.clone() })?
                        .entry
                        .ok_or_else(|| anyhow::anyhow!("missing entry ticket"))?
                };
                let deadline = if queue {
                    entry
                        .queue
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("missing resource-wait clock"))?
                        .deadline_ms
                } else {
                    Some(entry.deadline_ms.saturating_sub(1000))
                };
                let now = server::now_ms()?;
                let remaining =
                    deadline.map(|deadline| Duration::from_millis(deadline.saturating_sub(now)));
                let mut extension = None;
                continuation = amc_admission::host_server::acquire(
                    &host_socket,
                    remaining,
                    || {
                        signals
                            .as_ref()
                            .is_some_and(|signals| signals.cancelled().is_some())
                    },
                    || {
                        if queue
                            && extension.is_none_or(|last: Instant| {
                                last.elapsed() >= Duration::from_secs(5)
                            })
                        {
                            crate::native_start::notify(
                                "EXTEND_TIMEOUT_USEC=30000000\nSTATUS=Waiting for host capacity",
                            )?;
                            extension = Some(Instant::now());
                        }
                        Ok(())
                    },
                )?;
            }
            if let Some(signal) = signals.as_ref().and_then(|signals| signals.cancelled()) {
                return Ok(128 + signal);
            }
            call(
                &socket,
                Message::Enter {
                    id: ticket,
                    key: entry_key,
                },
            )?;
            let mut payload = Command::new(&command[0]);
            payload.args(&command[1..]);
            if let Some(continuation) = continuation {
                payload.env("AMC_CONTINUATION", serde_json::to_string(&continuation)?);
            } else {
                payload.env_remove("AMC_CONTINUATION");
            }
            if queue {
                crate::native_start::notify("READY=1\nSTATUS=Executing admitted workload")?;
                payload.env_remove("NOTIFY_SOCKET");
            }
            if let Some(signal) = signals.as_ref().and_then(|signals| signals.cancelled()) {
                return Ok(128 + signal);
            }
            if let Some(token) = ready_token {
                std::io::stderr()
                    .lock()
                    .write_all(format!("\x1e{token}\x1f").as_bytes())
                    .context("notify caller of native entry")?;
            }
            Err(payload.exec())
                .with_context(|| format!("execute admitted workload {:?}", command[0]))
        }
    }
}

fn host_policy(path: &Path) -> Result<amc_admission::host::HostPolicy> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_STATE_BYTES,
        "host policy too large"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn run(
    socket: &Path,
    wait: Option<Duration>,
    runtime_max_sec: Option<u64>,
    burst: bool,
    request: Message,
    argv: &[OsString],
    ready_token: Option<&str>,
) -> Result<i32> {
    use amc_runner::systemd::execute;
    let signals = crate::control::Signals::install()?;
    let deadline = wait.map(|wait| Instant::now() + wait);
    let queued = matches!(request, Message::EnqueueWaiting { .. });
    let response = match call(socket, request) {
        Ok(response) => response,
        Err(error) => {
            if queued
                && let Some(token) = ready_token
                && let Some(backpressure) = error.downcast_ref::<Backpressure>()
            {
                // Only explicit nonacceptance permits the intent owner to try
                // again. Exit, rather than retaining one process per overflow.
                std::io::stderr().lock().write_all(
                    format!(
                        "\x1e{token}:backpressure:{}\x1f",
                        backpressure.retry_after_ms
                    )
                    .as_bytes(),
                )?;
                return Ok(75);
            }
            return Err(error);
        }
    };
    let host_socket = response.host_socket;
    if let Some(host_socket) = &host_socket
        && let Err(error) = crate::native_start::verify_host_runner(host_socket)
    {
        if let Some(entry) = &response.entry {
            let _ = call(
                socket,
                Message::Cancel {
                    id: entry.id.clone(),
                },
            );
        }
        return Err(error);
    }
    let entry_key = response
        .entry_key
        .ok_or_else(|| anyhow::anyhow!("missing admission entry capability"))?;
    let mut entry = response
        .entry
        .ok_or_else(|| anyhow::anyhow!("missing admission ticket"))?;
    let runtime_max_sec = match execution_runtime(&entry.contract, burst, runtime_max_sec) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = call(socket, Message::Cancel { id: entry.id });
            return Err(error);
        }
    };
    let short_call = burst || entry.contract.protected_pool.is_some();
    while entry.phase == Phase::Queued {
        if signals.cancelled().is_some()
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
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
    ]);
    client.arg(if entry.queue.is_some() {
        "--service-type=notify"
    } else {
        "--service-type=exec"
    });
    if entry.queue.is_some() {
        client.arg("--property=NotifyAccess=main");
    }
    client.arg(format!("--unit={}", entry.unit()));
    for property in [
        format!("Slice={}", entry.contract.slice),
        format!("MemoryMax={}", entry.contract.memory_max),
        format!("MemorySwapMax={}", entry.contract.memory_swap_max),
        "MemoryAccounting=yes".into(),
        "OOMPolicy=kill".into(),
        "KillMode=control-group".into(),
        "Restart=no".into(),
        "TimeoutStartSec=30s".into(),
        format!("TimeoutStopSec={}s", if short_call { 1 } else { 15 }),
    ] {
        client.arg(format!("--property={property}"));
    }
    if let Some(seconds) = runtime_max_sec {
        client.arg(format!("--property=RuntimeMaxSec={seconds}s"));
        client.arg("--property=RuntimeRandomizedExtraSec=0");
    }
    if short_call {
        client.arg("--property=SendSIGKILL=yes");
        client.arg("--property=FinalKillSignal=SIGKILL");
    }
    if burst {
        eprintln!(
            "AMC burst: {} bytes reserved, {}s native runtime ceiling",
            entry.contract.memory_max,
            runtime_max_sec.unwrap_or_default()
        );
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
    let record = crate::native_start::wrap(&mut client)?;
    client
        .arg(std::env::current_exe()?)
        .args(["admission", "enter", "--socket"])
        .arg(socket)
        .args(["--ticket", &entry.id, "--entry-key", &entry_key]);
    if entry.queue.is_some() {
        client.arg("--queue");
    }
    if let Some(token) = ready_token {
        client.arg("--ready-token").arg(token);
    }
    if let Some(host_socket) = host_socket {
        client.arg("--host-socket").arg(host_socket);
    }
    client.arg("--").args(argv);
    // A reservation reply can arrive after the client's wait has expired.
    // The coordinator's extra entry grace must not authorize a late submission.
    if signals.cancelled().is_some() || deadline.is_some_and(|deadline| Instant::now() >= deadline)
    {
        let _ = call(
            socket,
            Message::Cancel {
                id: entry.id.clone(),
            },
        );
        anyhow::bail!("admission cancelled or timed out before submission");
    }
    let outcome = execute(
        &mut client,
        Path::new("systemctl"),
        &entry.unit(),
        false,
        true,
        || signals.cancelled(),
        &record,
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

fn parse_ready_token(value: &str) -> std::result::Result<String, String> {
    if !(32..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("ready token must contain 32..=64 ASCII letters, digits or hyphens".into());
    }
    Ok(value.to_owned())
}

pub fn parse_memory(value: &str) -> std::result::Result<u64, String> {
    let bytes = if let Some((digits, scale)) =
        [("GB", 1_000_000_000u64), ("MB", 1_000_000), ("KB", 1000)]
            .into_iter()
            .find_map(|(suffix, scale)| value.strip_suffix(suffix).map(|digits| (digits, scale)))
    {
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err("memory size must be an integral non-negative value".into());
        }
        digits
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .ok_or("memory size overflow")?
    } else {
        crate::config::parse_bytes(value).map_err(|e| e.to_string())?
    };
    let bytes = bytes / 4096 * 4096;
    if bytes == 0 || bytes > i64::MAX as u64 {
        return Err(
            "memory ceiling must be positive, <= i64::MAX and 4096-byte aligned (use MiB/GiB)"
                .into(),
        );
    }
    Ok(bytes)
}

fn execution_runtime(
    contract: &amc_admission::ledger::Contract,
    burst: bool,
    requested: Option<u64>,
) -> Result<Option<u64>> {
    ensure!(
        contract.burst == burst,
        "burst flag must match the selected contract"
    );
    if let (Some(requested), Some(maximum)) = (requested, contract.runtime_max_sec) {
        ensure!(requested <= maximum, "requested runtime exceeds contract");
    }
    Ok(requested.or(contract.runtime_max_sec))
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
    fn native_ready_tokens_are_bounded_and_cannot_inject_framing() {
        assert!(parse_ready_token("0123456789abcdef0123456789abcdef").is_ok());
        for token in [
            "short".to_owned(),
            "a".repeat(65),
            "\x1e".repeat(32),
            "\n".repeat(32),
        ] {
            assert!(parse_ready_token(&token).is_err());
        }
    }

    #[test]
    fn declared_decimal_memory_is_rounded_down_and_burst_runtime_cannot_grow() {
        assert_eq!(parse_memory("512MiB").unwrap(), 536870912);
        assert_eq!(parse_memory("3GB").unwrap(), 2999996416);
        assert!(parse_memory("-1GB").is_err());
        assert!(parse_memory("1KB").is_err());
        let contract: amc_admission::ledger::Contract = serde_json::from_value(serde_json::json!({
            "slice": "agent-burst.slice", "memory_max": 536870912, "memory_swap_max": 0,
            "max_running": 2, "pause_file": null, "burst": true, "runtime_max_sec": 5
        }))
        .unwrap();
        assert_eq!(execution_runtime(&contract, true, None).unwrap(), Some(5));
        assert_eq!(
            execution_runtime(&contract, true, Some(2)).unwrap(),
            Some(2)
        );
        assert!(execution_runtime(&contract, true, Some(6)).is_err());
        assert!(execution_runtime(&contract, false, None).is_err());
    }

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
