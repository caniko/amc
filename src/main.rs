mod config;
mod helpers;
mod systemd;

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
    about = "Apply hard memory contracts to transient user services"
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
    /// Verify that the local user manager can enforce the contract.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Resolve and display an application's effective contract.
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
    /// Inspect a retained or running AMC service.
    Inspect {
        unit: String,
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
    /// Copy one additional named variable from the caller without logging its value.
    #[arg(long, value_name = "NAME")]
    inherit_env: Vec<String>,
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum TestCommand {
    /// Allocate and touch memory in bounded chunks.
    Hog(HogArgs),
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
    #[arg(long, value_name = "0|1")]
    expect_memory_oom_group: Option<u8>,
}

impl From<ExpectedArgs> for ExpectedLimits {
    fn from(value: ExpectedArgs) -> Self {
        Self {
            memory_max: value.expect_memory_max,
            memory_swap_max: value.expect_memory_swap_max,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Explanation<'a> {
    id: &'a str,
    profile: &'a str,
    resolution: ResolutionSource,
    config_source: String,
    memory_max_bytes: u64,
    memory_swap_max_bytes: u64,
    slice: &'a str,
    unit_name_pattern: &'static str,
    systemd_properties: Vec<String>,
    environment_inheritance: Vec<String>,
    oom_group_enforcement: &'static str,
    scope_statement: &'static str,
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
        Command::Doctor { json } => {
            let report = systemd::doctor();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "AMC hard-contained backend: {}",
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
        Command::Inspect { unit, json } => {
            let inspection = systemd::inspect(&unit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&inspection)?);
            } else {
                println!("Unit: {}", inspection.unit);
                for (name, value) in &inspection.properties {
                    println!("{name}={value}");
                }
                if let Some(path) = &inspection.cgroup_path {
                    println!("Cgroup: {path}");
                }
                for (name, value) in &inspection.cgroup {
                    println!("{name}={value}");
                }
                for (name, value) in &inspection.memory_events {
                    println!("memory.events {name}={value}");
                }
                println!("Journal:\n{}", inspection.journal);
            }
            Ok(0)
        }
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
        slice: &resolved.profile.slice,
        unit_name_pattern: UNIT_PATTERN,
        systemd_properties,
        environment_inheritance: environment,
        oom_group_enforcement: "OOMPolicy=kill sets memory.oom.group=1; doctor verifies cgroupfs directly",
        scope_statement: SCOPE_STATEMENT,
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
    let argv = systemd::resolve_command(&arguments.command)?;
    let unit = systemd::unit_name(&arguments.id)?;
    let spec = StartSpec {
        id: &arguments.id,
        profile: &resolved.profile,
        unit: &unit,
        retain: arguments.retain_unit,
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
