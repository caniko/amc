//! Production admission CLI. The native entry handshake gates execution.
use amc_admission::{
    ledger::Policy,
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
            use amc_admission::client::{Exec, ExecRequest, Mode};
            let mode = match (queue, max_ram_usage, protected_pool) {
                (true, memory_max, _) => Mode::Queued {
                    memory_max: memory_max
                        .ok_or_else(|| anyhow::anyhow!("queued work requires a memory ceiling"))?,
                    runtime_max_sec: runtime_max_sec.ok_or_else(|| {
                        anyhow::anyhow!("queued work requires an execution deadline")
                    })?,
                },
                (false, Some(memory_max), true) => Mode::Bounded {
                    memory_max,
                    runtime_max_sec: runtime_max_sec
                        .ok_or_else(|| anyhow::anyhow!("missing bounded runtime"))?,
                },
                (false, Some(memory_max), false) => Mode::Sized { memory_max },
                (false, None, _) => Mode::Plain,
            };
            let request = ExecRequest {
                contract,
                mode,
                wait: timeout.map(Duration::from_secs),
                runtime_max_sec,
                burst,
                ready_token,
                socket: socket_path(socket)?,
                helper: std::env::current_exe()?,
            };
            let signals = crate::control::Signals::install()?;
            Exec::new(request, command).run(|| signals.cancelled())
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
                    |reply| {
                        if queue
                            && extension.is_none_or(|last: Instant| {
                                last.elapsed() >= Duration::from_secs(5)
                            })
                        {
                            crate::native_start::notify(&format!(
                                "EXTEND_TIMEOUT_USEC=30000000\nSTATUS={}",
                                wait_status(reply)
                            ))?;
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

/// Why a queued reservation waits, for the unit's systemd STATUS.
fn wait_status(reply: &amc_admission::host_server::Response) -> String {
    let mut status = "Waiting for host capacity".to_owned();
    let Some(reason) = reply.waiting else {
        return status;
    };
    status += &format!(": {reason:?}");
    if let (Some(free), Some(reserve), Some(committed)) = (
        reply.swap_free_bytes,
        reply.swap_reserve_bytes,
        reply.swap_committed_bytes,
    ) {
        let gib = |bytes: u64| match bytes {
            0 => "0".to_owned(),
            b if b % (1 << 30) == 0 => format!("{} GiB", b >> 30),
            b => format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64),
        };
        status += &format!(
            " (swap free {}, reserve {}, committed {})",
            gib(free),
            gib(reserve),
            gib(committed)
        );
    }
    status
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

fn parse_ready_token(value: &str) -> std::result::Result<String, String> {
    amc_admission::client::validate_ready_token(value).map_err(|error| error.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn declared_decimal_memory_is_rounded_down() {
        assert_eq!(parse_memory("512MiB").unwrap(), 536870912);
        assert_eq!(parse_memory("3GB").unwrap(), 2999996416);
        assert!(parse_memory("-1GB").is_err());
        assert!(parse_memory("1KB").is_err());
    }

    #[test]
    fn wait_status_names_the_reason_and_swap_numbers() {
        use amc_admission::{host::WaitReason, host_server::Response};
        let mut reply = Response::default();
        assert_eq!(wait_status(&reply), "Waiting for host capacity");
        reply.waiting = Some(WaitReason::Budget);
        assert_eq!(wait_status(&reply), "Waiting for host capacity: Budget");
        reply.waiting = Some(WaitReason::SwapHeadroom);
        reply.swap_free_bytes = Some(2_501_000_000);
        reply.swap_reserve_bytes = Some(2 << 30);
        reply.swap_committed_bytes = Some(0);
        assert_eq!(
            wait_status(&reply),
            "Waiting for host capacity: SwapHeadroom (swap free 2.33 GiB, reserve 2 GiB, committed 0)"
        );
    }
}
