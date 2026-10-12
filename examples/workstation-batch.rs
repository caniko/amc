//! Bounded, opt-in B/C experiment driver. See docs/gaming-comparison.md.
use amc_admission::{
    ledger::{Contract, Entry, Phase},
    native::{Native, Systemd},
};
use amc_runner::{
    Config,
    provider::SharedMemoryProvider,
    providers::CgroupV2Provider,
    systemd::{self, LaunchRequest, Outcome, Runner},
    weighted::WeightedConfig,
};
use amc_runner::{MemoryDomain, MemoryProvider, MemoryStats, ProviderError};
use anyhow::{Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[path = "../src/control.rs"]
mod control;

struct BudgetProvider {
    native: SharedMemoryProvider,
    budget: u64,
}

impl MemoryProvider for BudgetProvider {
    fn used_fraction(&self) -> std::result::Result<f64, ProviderError> {
        Ok(self.stats()?.used_fraction())
    }

    fn stats(&self) -> std::result::Result<MemoryStats, ProviderError> {
        let native = self.native.stats()?;
        let mut domains = native.domains().to_vec();
        domains.push(MemoryDomain::new(self.budget, self.budget)?);
        let stats = MemoryStats::from_domains(domains, native.page_cache_opt())?;
        Ok(match native.infer_cache_total() {
            Some(total) => stats.with_cache_total(total),
            None => stats,
        })
    }
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
enum Arm {
    B,
    C,
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Execute an explicitly configured finite batch. All jobs are offered at once.
    Run {
        #[arg(long)]
        arm: Arm,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Validate the batch without launching anything.
    Check { manifest: PathBuf },
    #[command(hide = true)]
    Enter {
        manifest: PathBuf,
        output: PathBuf,
        index: usize,
        id: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    slice: String,
    aggregate_memory_max: u64,
    aggregate_memory_swap_max: u64,
    concurrency: usize,
    budget_bytes: u64,
    reserve_bytes: u64,
    max_ram_fraction: f64,
    runtime_seconds: u64,
    admission_seconds: u64,
    cpu_percent: u32,
    jobs: Vec<Job>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Job {
    id: String,
    weight_bytes: u64,
    memory_max: u64,
    memory_swap_max: u64,
    cwd: PathBuf,
    argv: Vec<String>,
    verify_argv: Vec<String>,
    useful_units: u64,
}

fn load(path: &Path) -> Result<Manifest> {
    let file = OpenOptions::new()
        .read(true)
        // Linux O_NOFOLLOW | O_NONBLOCK: reject special inputs without waiting.
        .custom_flags(0x20000 | 0x800)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "manifest must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "manifest exceeds 1 MiB");
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.version == 1 && (1..=32).contains(&manifest.jobs.len()),
        "invalid manifest version/job count"
    );
    ensure!(
        (1..=8).contains(&manifest.concurrency),
        "concurrency must be 1..8"
    );
    ensure!(
        (1..=1800).contains(&manifest.runtime_seconds)
            && (1..=1800).contains(&manifest.admission_seconds),
        "timeouts must be 1..1800 seconds"
    );
    ensure!(
        (1..=800).contains(&manifest.cpu_percent),
        "CPU quota must be 1..800 percent"
    );
    ensure!(
        manifest.budget_bytes > manifest.reserve_bytes,
        "reserve must leave an admission budget"
    );
    Config {
        max_ram_fraction: manifest.max_ram_fraction,
        resume_hysteresis: 0.0,
        ..Config::default()
    }
    .validate()?;
    let mut names = BTreeSet::new();
    for job in &manifest.jobs {
        ensure!(
            amc_admission::ledger::valid_name(&job.id) && names.insert(&job.id),
            "duplicate or invalid job ID"
        );
        ensure!(
            job.cwd.is_absolute() && job.cwd.is_dir(),
            "workload cwd must be an existing absolute directory"
        );
        ensure!(
            job.weight_bytes > 0
                && job.weight_bytes <= job.memory_max
                && job.weight_bytes <= manifest.budget_bytes - manifest.reserve_bytes,
            "invalid or impossible weight"
        );
        ensure!(
            job.memory_max <= manifest.aggregate_memory_max
                && job.memory_swap_max <= manifest.aggregate_memory_swap_max,
            "job exceeds aggregate limits"
        );
        ensure!(job.useful_units > 0, "useful units must be positive");
        for argv in [&job.argv, &job.verify_argv] {
            ensure!(
                !argv.is_empty()
                    && argv.len() <= 256
                    && Path::new(&argv[0]).is_absolute()
                    && argv.iter().all(|arg| !arg.contains('\0')),
                "commands require bounded literal argv and an absolute executable"
            );
        }
        contract(&manifest, job).validate()?;
    }
    Ok(manifest)
}

fn contract(manifest: &Manifest, job: &Job) -> Contract {
    Contract {
        burst: false,
        runtime_max_sec: None,
        slice: manifest.slice.clone(),
        memory_max: job.memory_max,
        memory_swap_max: job.memory_swap_max,
        max_running: manifest.concurrency,
        pause_file: None,
    }
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn unix_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

fn enter(manifest: &Manifest, output: &Path, index: usize, id: String) -> Result<()> {
    let job = manifest
        .jobs
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("unknown job index"))?;
    let entry = Entry {
        id,
        name: job.id.clone(),
        contract: contract(manifest, job),
        phase: Phase::Reserved,
        deadline_ms: 0,
        identity: None,
        client: None,
    };
    let identity = Systemd {
        systemctl: "systemctl".into(),
    }
    .identify(&entry, std::process::id().try_into()?)?;
    let started = unix_ms()?;
    save(
        &output.join(format!("job-{index}-entry.json")),
        &json!({"id": entry.id, "identity": identity, "startedUnixMs": started}),
    )?;
    let launch = |argv: &[String], suffix: &str| -> Result<i32> {
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output.join(format!("job-{index}-{suffix}.log")))?;
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&job.cwd)
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()?;
        Ok(status.code().unwrap_or(1))
    };
    let code = launch(&job.argv, "workload")?;
    let verified = if code == 0 {
        Some(launch(&job.verify_argv, "verify")?)
    } else {
        None
    };
    // This endpoint is sampled while the helper is still alive. It is not
    // claimed to include the helper's final write or the entire unit lifetime.
    let group = amc_admission::native::cgroup_directory(&identity.cgroup)?;
    let sampled: std::collections::BTreeMap<_, _> = [
        "memory.peak",
        "memory.swap.peak",
        "memory.events",
        "cpu.stat",
        "io.stat",
    ]
    .into_iter()
    .map(|key| (key, fs::read_to_string(group.join(key)).ok()))
    .collect();
    save(
        &output.join(format!("job-{index}-receipt.json")),
        &json!({"id": entry.id, "startedUnixMs": started, "finishedUnixMs": unix_ms()?, "commandExit": code, "verifyExit": verified, "sampledBeforeExit": sampled}),
    )?;
    ensure!(
        code == 0 && verified == Some(0),
        "workload or useful-output verification failed"
    );
    Ok(())
}

fn job_run(
    manifest: &Manifest,
    output: &Path,
    index: usize,
    id: &str,
    runner: Option<&Runner>,
    signals: &control::Signals,
    offered: u128,
) -> Result<Value> {
    let job = &manifest.jobs[index];
    let unit = format!("app-amc-job-{id}.service");
    // Every native setting is shared between B and C. The helper verifies
    // manager identity, placement and kernel memory limits before useful work.
    let properties = vec![
        format!("Slice={}", manifest.slice),
        format!("MemoryMax={}", job.memory_max),
        format!("MemorySwapMax={}", job.memory_swap_max),
        "MemoryAccounting=yes".into(),
        "CPUAccounting=yes".into(),
        "IOAccounting=yes".into(),
        format!("CPUQuota={}%", manifest.cpu_percent),
        "OOMPolicy=kill".into(),
        "KillMode=control-group".into(),
        "TimeoutStopSec=15s".into(),
        format!("RuntimeMaxSec={}s", manifest.runtime_seconds),
    ];
    let argv = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "enter".into(),
        output.join("manifest.json").display().to_string(),
        output.display().to_string(),
        index.to_string(),
        id.into(),
    ];
    let environment: Vec<_> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            systemd::valid_environment_name(name)
                && !name.starts_with("SYSTEMD_")
                && !name.starts_with("LISTEN_")
                && !matches!(
                    name.as_str(),
                    "INVOCATION_ID"
                        | "MANAGERPID"
                        | "JOURNAL_STREAM"
                        | "NOTIFY_SOCKET"
                        | "WATCHDOG_PID"
                        | "WATCHDOG_USEC"
                )
        })
        .collect();
    let start = Instant::now();
    let outcome = if let Some(runner) = runner {
        runner
            .run(
                LaunchRequest {
                    launcher: Path::new("systemd-run"),
                    unit: &unit,
                    weight: job.weight_bytes,
                    admission_timeout: Duration::from_secs(manifest.admission_seconds),
                    detached: false,
                    argv: &argv,
                    environment: &environment,
                    properties: &properties,
                },
                || signals.cancelled(),
            )
            .map_err(anyhow::Error::from)
    } else {
        let mut command = Command::new("systemd-run");
        command
            .args([
                "--user",
                "--quiet",
                "--expand-environment=no",
                "--same-dir",
                "--wait",
                "--pipe",
                "--service-type=exec",
                "--property=Restart=no",
            ])
            .arg(format!("--unit={unit}"));
        for name in &environment {
            command.arg(format!("--setenv={name}"));
        }
        for property in &properties {
            command.arg(format!("--property={property}"));
        }
        command.arg("--").args(&argv);
        Ok(systemd::execute(
            &mut command,
            Path::new("systemctl"),
            &unit,
            false,
            true,
            || signals.cancelled(),
            &Default::default(),
        ))
    };
    let accounting = control::capture_with_timeout(Command::new("systemctl").args(["--user", "show", "--no-pager", "--property=Id,LoadState,ActiveState,Result,InvocationID,ControlGroup,MemoryPeak,MemorySwapPeak,CPUUsageNSec,IOReadBytes,IOWriteBytes,ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic", "--", &unit]), control::QUERY_TIMEOUT).ok();
    save(
        &output.join(format!("job-{index}-native.json")),
        &accounting,
    )?;
    let receipt: Option<Value> = fs::read(output.join(format!("job-{index}-receipt.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let completed = matches!(outcome, Ok(Outcome::Completed(0)))
        && receipt
            .as_ref()
            .is_some_and(|r| r["id"] == id && r["commandExit"] == 0 && r["verifyExit"] == 0);
    let housekeeping = if let Some(runner) = runner {
        match runner.reconcile(&unit, true) {
            Err(systemd::RunError::NotTracked) => {
                format!("{:?}", systemd::cleanup(Path::new("systemctl"), &unit))
            }
            result => format!("{result:?}"),
        }
    } else {
        format!("{:?}", systemd::cleanup(Path::new("systemctl"), &unit))
    };
    Ok(
        json!({"job": job.id, "unit": unit, "offeredUnixMs": offered, "elapsedMs": start.elapsed().as_millis(), "outcome": format!("{outcome:?}"), "terminationConfirmed": outcome.as_ref().is_ok_and(|outcome| outcome.releases_reservation()), "housekeeping": housekeeping, "completed": completed, "usefulUnits": if completed { job.useful_units } else { 0 }, "receipt": receipt}),
    )
}

fn run(arm: Arm, manifest: Manifest, output: PathBuf) -> Result<()> {
    ensure!(output.is_absolute(), "output must be absolute and new");
    fs::DirBuilder::new().mode(0o700).create(&output)?;
    save(&output.join("manifest.json"), &manifest)?;
    let native = Systemd {
        systemctl: "systemctl".into(),
    };
    let observed = native.headroom(&contract(&manifest, &manifest.jobs[0]), 0)?;
    ensure!(
        observed.memory_max == manifest.aggregate_memory_max
            && observed.memory_swap_max == manifest.aggregate_memory_swap_max,
        "live aggregate limits differ from the manifest"
    );
    let directory = systemd::unit_cgroup_dir(Path::new("systemctl"), &manifest.slice)
        .ok_or_else(|| anyhow::anyhow!("worker domain unavailable"))?;
    let runner = if matches!(arm, Arm::C) {
        Some(Runner::new(
            WeightedConfig {
                base: Config {
                    max_ram_fraction: manifest.max_ram_fraction,
                    resume_hysteresis: 0.0,
                    ..Config::default()
                },
                safety_reserve_bytes: manifest.reserve_bytes,
                max_single_weight_bytes: u64::MAX,
                max_page_cache_fraction: 1.0,
                ..WeightedConfig::default()
            },
            Arc::new(BudgetProvider {
                native: Arc::new(CgroupV2Provider::for_dir(directory)?),
                budget: manifest.budget_bytes,
            }),
            "systemctl".into(),
        )?)
    } else {
        None
    };
    let signals = control::Signals::install()?;
    let offered = unix_ms()?;
    let run_id = amc_admission::store::fresh_id()?;
    save(
        &output.join("run.json"),
        &json!({"version": 1, "arm": arm, "runId": run_id, "offeredUnixMs": offered, "jobCount": manifest.jobs.len(), "bootId": fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(), "systemdVersion": control::capture(Command::new("systemctl").arg("--version")).ok()}),
    )?;
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..manifest.concurrency.min(manifest.jobs.len())).map(|_| scope.spawn(|| -> Result<()> {
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= manifest.jobs.len() { break; }
                let result = job_run(&manifest, &output, index, &format!("{run_id}-{index}"), runner.as_ref(), &signals, offered);
                save(&output.join(format!("job-{index}-result.json")), &match result { Ok(value) => value, Err(error) => json!({"job": manifest.jobs[index].id, "completed": false, "usefulUnits": 0, "error": error.to_string()}) })?;
            }
            Ok(())
        })).collect();
        for worker in workers {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("batch worker panicked; inspect recorded units"))??;
        }
        Ok(())
    })?;
    let results: Vec<Value> = (0..manifest.jobs.len())
        .map(|i| -> Result<Value> {
            Ok(serde_json::from_slice(&fs::read(
                output.join(format!("job-{i}-result.json")),
            )?)?)
        })
        .collect::<Result<_>>()?;
    let complete = results.iter().all(|result| result["completed"] == true);
    save(
        &output.join("summary.json"),
        &json!({"version": 1, "arm": arm, "offeredUnixMs": offered, "finishedUnixMs": unix_ms()?, "allJobsSucceeded": complete, "reservedBytes": runner.as_ref().map(Runner::committed_bytes), "jobs": results}),
    )?;
    ensure!(
        complete,
        "batch contains failed or unconfirmed work; retain its artifacts"
    );
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Action::Check { manifest } => {
            let manifest = load(&manifest)?;
            println!("Validated {} finite jobs", manifest.jobs.len());
            Ok(())
        }
        Action::Run {
            arm,
            manifest,
            output,
        } => run(arm, load(&manifest)?, output),
        Action::Enter {
            manifest,
            output,
            index,
            id,
        } => enter(&load(&manifest)?, &output, index, id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Observed(MemoryStats);
    impl MemoryProvider for Observed {
        fn used_fraction(&self) -> std::result::Result<f64, ProviderError> {
            Ok(self.0.used_fraction())
        }
        fn stats(&self) -> std::result::Result<MemoryStats, ProviderError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn budget_preserves_worker_ancestors_and_unknown_cache() {
        let original = MemoryStats::from_domains(
            vec![
                MemoryDomain::new(1000, 900).unwrap(),
                MemoryDomain::new(700, 600).unwrap(),
            ],
            None,
        )
        .unwrap();
        let provider = BudgetProvider {
            native: Arc::new(Observed(original)),
            budget: 500,
        };
        let observed = provider.stats().unwrap();
        assert_eq!(
            observed.domains(),
            &[
                MemoryDomain::new(1000, 900).unwrap(),
                MemoryDomain::new(700, 600).unwrap(),
                MemoryDomain::new(500, 500).unwrap(),
            ]
        );
        assert_eq!(observed.page_cache_opt(), None);
        assert!(
            BudgetProvider {
                native: Arc::new(|| Err(ProviderError::Unsupported)),
                budget: 500
            }
            .stats()
            .is_err()
        );
    }
}
