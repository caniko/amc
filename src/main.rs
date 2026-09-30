mod admission;
mod config;
mod control;
mod helpers;
mod systemd;
mod telemetry;
mod watch;

use std::{path::PathBuf, thread, time::Duration};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::{
    config::{Config, ResolutionSource},
    helpers::ExpectedLimits,
    systemd::{SCOPE_STATEMENT, StartMode, StartSpec, UNIT_PATTERN},
};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Inspect Linux memory controls, capture unit telemetry, and test fixture policies"
)]
struct Cli {
    /// Use this configuration file instead of searching standard locations.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Coordinate production workload reservations across local processes.
    Admission {
        #[command(subcommand)]
        command: admission::AdmissionCommand,
    },
    /// Actively probe fixture capabilities using a short-lived transient unit.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Display requested private fixture settings (effective policy is unverified).
    Explain {
        #[arg(long)]
        id: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run a command synchronously in a transient service.
    Run(ManagedCommand),
    /// Launch a detached transient service and return after startup.
    Launch(ManagedCommand),
    /// Passively inspect a native service or scope.
    Inspect {
        unit: String,
        /// Query a system unit rather than a unit in the current user manager.
        #[arg(long)]
        system: bool,
        #[arg(long)]
        json: bool,
    },
    /// Observe one unit's cgroup telemetry for a finite interval (read-only).
    Watch {
        unit: String,
        /// Query a system unit rather than a unit in the current user manager.
        #[arg(long)]
        system: bool,
        /// Passive long capture with host context (default 1800s, 1000ms; no policy changes).
        #[arg(long)]
        production: bool,
        /// Observation length: 1..=60 (default 40), or 1..=86400 with --production.
        #[arg(long)]
        seconds: Option<u64>,
        /// Sampling interval: 10..=1000 (default 20), or 1000..=60000 with --production.
        #[arg(long)]
        interval_ms: Option<u64>,
        /// Output directory (must not exist; created 0700).
        #[arg(long)]
        output: PathBuf,
    },
    /// Offline comparison of two Snapshot JSON files.
    Diff { before: PathBuf, after: PathBuf },
    /// Validate and summarize one capture offline (Markdown by default).
    Report {
        capture: PathBuf,
        /// Emit machine-readable JSON instead of Markdown.
        #[arg(long)]
        json: bool,
    },
    /// Internal deterministic helpers used by proof tests.
    Test {
        #[command(subcommand)]
        command: TestCommand,
    },
}

#[derive(Debug, Args)]
struct ManagedCommand {
    #[arg(long)]
    id: String,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    retain_unit: bool,
    /// Write the owned attempt's unit identity privately before submission.
    #[arg(long, value_name = "PATH")]
    unit_file: Option<PathBuf>,
    /// Manager-enforced runtime bound for a disposable fixture, not a startup deadline.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
    runtime_max_sec: Option<u64>,
    /// Copy one additional named variable from the caller without logging its value.
    #[arg(long, value_name = "NAME")]
    inherit_env: Vec<String>,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum TestCommand {
    /// Allocate and touch memory in bounded chunks (max-only fixture).
    Hog(HogArgs),
    /// Finite useful-work task with completion + latency accounting.
    Work(WorkArgs),
    /// Measure monotonic scheduling delay.
    Heartbeat {
        #[arg(long)]
        samples: PathBuf,
        #[arg(long)]
        summary: PathBuf,
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
        #[arg(long, default_value_t = 30_000)]
        duration_ms: u64,
    },
    /// Report this process's effective cgroup memory controls.
    SelfReport(SelfReportArgs),
}

#[derive(Debug, Args)]
struct ExpectedArgs {
    #[arg(long, value_name = "BYTES")]
    expect_memory_max: Option<String>,
    #[arg(long, value_name = "BYTES")]
    expect_memory_swap_max: Option<String>,
    #[arg(long, value_name = "BYTES")]
    expect_memory_high: Option<String>,
    #[arg(long, value_name = "0|1")]
    expect_memory_oom_group: Option<u8>,
}

impl From<ExpectedArgs> for ExpectedLimits {
    fn from(value: ExpectedArgs) -> Self {
        Self {
            memory_max: value.expect_memory_max,
            memory_swap_max: value.expect_memory_swap_max,
            memory_high: value.expect_memory_high,
            memory_oom_group: value.expect_memory_oom_group,
        }
    }
}

#[derive(Debug, Args)]
struct SelfReportArgs {
    #[arg(long)]
    output: Option<PathBuf>,
    #[command(flatten)]
    expected: ExpectedArgs,
    #[arg(long, default_value_t = 0)]
    sleep_ms: u64,
}

#[derive(Debug, Args)]
struct HogArgs {
    #[arg(long)]
    report: Option<PathBuf>,
    #[command(flatten)]
    expected: ExpectedArgs,
    #[arg(long, default_value = "16MiB")]
    chunk_size: String,
    #[arg(long, default_value = "1GiB")]
    maximum: String,
    #[arg(long, default_value = "64MiB")]
    progress_interval: String,
    #[arg(long, default_value_t = 10)]
    delay_ms: u64,
}

#[derive(Debug, Args)]
struct WorkArgs {
    #[arg(long)]
    report: Option<PathBuf>,
    #[command(flatten)]
    expected: ExpectedArgs,
    #[arg(long, default_value = "4MiB")]
    size: String,
    #[arg(long, default_value_t = 10)]
    iterations: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Explanation<'a> {
    id: &'a str,
    profile: &'a str,
    resolution: ResolutionSource,
    config_source: String,
    memory_max_bytes: u64,
    memory_swap_max_bytes: u64,
    memory_high_bytes: Option<u64>,
    slice: &'a str,
    unit_name_pattern: &'static str,
    systemd_properties: Vec<String>,
    environment_inheritance: Vec<String>,
    oom_group_enforcement: &'static str,
    scope_statement: &'static str,
    /// Requested TOML values are not yet effective: no unit exists.
    effective: &'static str,
}

fn main() {
    let exit_code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("amc: {error:#}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Admission { command } => admission::execute(command),
        Command::Doctor { json } => {
            let report = systemd::doctor();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "AMC fixture backend: {}",
                    if report.available {
                        "available"
                    } else {
                        "unavailable"
                    }
                );
                for check in &report.checks {
                    let status = if check.ok {
                        "PASS"
                    } else if check.required {
                        "FAIL"
                    } else {
                        "INFO"
                    };
                    println!("[{status}] {}: {}", check.name, check.detail);
                }
                println!("Nix boundary: {}", report.nix_boundary.detail);
                println!("Scope: {}", report.scope_statement);
            }
            Ok(i32::from(!report.available))
        }
        Command::Explain { id, profile, json } => {
            let config = config::load(cli.config.as_deref())?;
            explain(&config, &id, profile.as_deref(), json)?;
            Ok(0)
        }
        Command::Run(arguments) => managed(config::load(cli.config.as_deref())?, arguments, true),
        Command::Launch(arguments) => {
            managed(config::load(cli.config.as_deref())?, arguments, false)
        }
        Command::Inspect { unit, system, json } => {
            let inspection = systemd::inspect(&unit, system)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&inspection)?);
            } else {
                println!("Unit: {}", inspection.unit);
                println!("--- manager-reported (systemctl show) ---");
                for (name, value) in &inspection.properties {
                    println!("{name}={value}");
                }
                println!("Requested: {}", inspection.requested);
                println!(
                    "Kernel observations (leaf through visible root):\n{}",
                    serde_json::to_string_pretty(&inspection.kernel)?
                );
                println!(
                    "Ancestor manager metadata:\n{}",
                    serde_json::to_string_pretty(&inspection.ancestors)?
                );
                if !inspection.evidence_notes.is_empty() {
                    println!("--- evidence notes ---");
                    for note in &inspection.evidence_notes {
                        println!("note: {note}");
                    }
                }
                println!("Application journal messages are omitted from diagnostics.");
            }
            Ok(0)
        }
        Command::Watch {
            unit,
            system,
            production,
            seconds,
            interval_ms,
            output,
        } => crate::watch::watch(&crate::watch::WatchArgs {
            unit,
            system,
            production,
            seconds: seconds.unwrap_or(if production {
                1800
            } else {
                crate::watch::DEFAULT_SECONDS
            }),
            interval_ms: interval_ms.unwrap_or(if production {
                1000
            } else {
                crate::watch::DEFAULT_INTERVAL_MS
            }),
            output,
        }),
        Command::Report { capture, json } => {
            let mut command = std::process::Command::new("python3");
            command
                .args([
                    "-I",
                    "-B",
                    "-c",
                    include_str!("../scripts/validate-capture.py"),
                ])
                .arg(capture);
            if !json {
                command.arg("--markdown");
            }
            let status = command
                .status()
                .map_err(|_| anyhow::anyhow!("amc report requires Python 3.11+ on PATH"))?;
            Ok(status.code().unwrap_or(1))
        }
        Command::Diff { before, after } => crate::watch::diff(&before, &after),
        Command::Test { command } => test_helper(command),
    }
}

fn explain(config: &Config, id: &str, explicit_profile: Option<&str>, json: bool) -> Result<()> {
    let resolved = config.resolve(id, explicit_profile)?;
    let environment = systemd::inherited_environment(&[])?;
    let dummy_argv = vec!["/absolute/executable".into()];
    let spec = StartSpec {
        id,
        profile: &resolved.profile,
        unit: UNIT_PATTERN,
        retain: false,
        runtime_max_sec: None,
        environment: &environment,
        argv: &dummy_argv,
    };
    let systemd_properties = systemd::systemd_arguments(&spec, StartMode::Launch)
        .into_iter()
        .filter_map(|argument| argument.strip_prefix("--property=").map(str::to_owned))
        .collect();
    let explanation = Explanation {
        id,
        profile: &resolved.name,
        resolution: resolved.resolution,
        config_source: config.source.display().to_string(),
        memory_max_bytes: resolved.profile.memory_max_bytes,
        memory_swap_max_bytes: resolved.profile.memory_swap_max_bytes,
        memory_high_bytes: resolved.profile.memory_high_bytes,
        slice: &resolved.profile.slice,
        unit_name_pattern: UNIT_PATTERN,
        systemd_properties,
        environment_inheritance: environment,
        oom_group_enforcement: "fixture only: OOMPolicy=kill sets memory.oom.group=1; doctor verifies kernel cgroupfs directly (no MemoryOOMGroup property exists)",
        scope_statement: SCOPE_STATEMENT,
        effective: "unverified: no unit exists yet; requested TOML values are not effective settings. Real applications use native units/drop-ins, not this TOML.",
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&explanation)?);
    } else {
        println!("Application: {}", explanation.id);
        println!(
            "Profile: {} ({:?})",
            explanation.profile, explanation.resolution
        );
        println!("Config: {}", explanation.config_source);
        println!("MemoryMax: {} bytes", explanation.memory_max_bytes);
        println!("MemorySwapMax: {} bytes", explanation.memory_swap_max_bytes);
        match explanation.memory_high_bytes {
            Some(high) => println!("MemoryHigh: {high} bytes"),
            None => println!("MemoryHigh: unset (not emitted; max-only fixture)"),
        }
        println!("Effective: {}", explanation.effective);
        println!("Slice: {}", explanation.slice);
        println!("Unit pattern: {}", explanation.unit_name_pattern);
        println!("Properties:");
        for property in &explanation.systemd_properties {
            println!("  {property}");
        }
        println!(
            "Inherited environment names: {}",
            explanation.environment_inheritance.join(", ")
        );
        println!("OOM group: {}", explanation.oom_group_enforcement);
        println!("Scope: {}", explanation.scope_statement);
    }
    Ok(())
}

fn managed(config: Config, arguments: ManagedCommand, synchronous: bool) -> Result<i32> {
    let resolved = config.resolve(&arguments.id, arguments.profile.as_deref())?;
    let environment = systemd::inherited_environment(&arguments.inherit_env)?;
    let argv = systemd::resolve_command(&arguments.command)
        .map_err(|_| anyhow::anyhow!("workload executable unavailable; arguments omitted"))?;
    let unit = systemd::unit_name(&arguments.id)?;
    if let Some(path) = &arguments.unit_file {
        helpers::atomic_write(path, format!("{unit}\n").as_bytes())?;
    }
    let spec = StartSpec {
        id: &arguments.id,
        profile: &resolved.profile,
        unit: &unit,
        retain: arguments.retain_unit,
        runtime_max_sec: arguments.runtime_max_sec,
        environment: &environment,
        argv: &argv,
    };
    let mode = if synchronous {
        StartMode::Run {
            pty: systemd::all_stdio_are_terminals(),
        }
    } else {
        StartMode::Launch
    };
    systemd::start(&spec, mode)
}

fn test_helper(command: TestCommand) -> Result<i32> {
    match command {
        TestCommand::SelfReport(arguments) => {
            helpers::self_report(arguments.output.as_deref(), &arguments.expected.into())?;
            if arguments.sleep_ms != 0 {
                thread::sleep(Duration::from_millis(arguments.sleep_ms));
            }
        }
        TestCommand::Hog(arguments) => helpers::hog(
            arguments.report.as_deref(),
            &arguments.expected.into(),
            &arguments.chunk_size,
            &arguments.maximum,
            &arguments.progress_interval,
            arguments.delay_ms,
        )?,
        TestCommand::Work(arguments) => {
            helpers::work(
                arguments.report.as_deref(),
                &arguments.expected.into(),
                &arguments.size,
                arguments.iterations,
            )?;
        }
        TestCommand::Heartbeat {
            samples,
            summary,
            interval_ms,
            duration_ms,
        } => {
            helpers::heartbeat(&samples, &summary, interval_ms, duration_ms)?;
        }
    }
    Ok(0)
}
