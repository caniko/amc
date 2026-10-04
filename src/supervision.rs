use amc_supervision::{
    native::{Manager, bounded_json},
    policy::Policy,
    server,
};
use anyhow::Result;
use clap::Subcommand;
use std::{fs::File, io::BufReader, path::PathBuf};

#[derive(Debug, Subcommand)]
pub enum SupervisionCommand {
    /// Run the host-wide supervisor; policy defaults remain consumer-owned.
    Serve {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long, default_value = "/var/lib/amc-supervision")]
        state: PathBuf,
        #[arg(long, default_value = "/run/amc-supervision")]
        runtime: PathBuf,
        #[arg(long, default_value = "systemctl")]
        systemctl: PathBuf,
    },
    /// Read the last bounded native observation and recovery state as JSON.
    Status {
        #[arg(long, default_value = "/run/amc-supervision/status.json")]
        file: PathBuf,
    },
    /// Replay a chronological JSONL trace without signalling or starting services.
    Replay {
        #[arg(long)]
        file: PathBuf,
    },
    /// Clear a tripped recovery after proving its original domain has terminated.
    /// Stop the supervisor first; this keeps all recovery accounting and never starts work.
    Reconcile {
        #[arg(long, default_value = "/var/lib/amc-supervision")]
        state: PathBuf,
        #[arg(long, default_value = "systemctl")]
        systemctl: PathBuf,
    },
}

pub fn execute(command: SupervisionCommand) -> Result<i32> {
    match command {
        SupervisionCommand::Serve {
            policy,
            state,
            runtime,
            systemctl,
        } => {
            let policy: Policy = bounded_json(&policy, Some(0))?;
            let signals = crate::control::Signals::install()?;
            server::serve(policy, &state, &runtime, Manager { systemctl }, || {
                signals.cancelled().is_some()
            })?;
        }
        SupervisionCommand::Status { file } => {
            let status: serde_json::Value = bounded_json(&file, Some(0))?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        SupervisionCommand::Replay { file } => {
            let summary = amc_supervision::replay::run(BufReader::new(File::open(file)?))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        SupervisionCommand::Reconcile { state, systemctl } => {
            amc_supervision::native::require_root()?;
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
            let (store, mut snapshot) = amc_supervision::store::Store::open(&state, boot.trim())?;
            if let Some(active) = &snapshot.active {
                let manager = Manager { systemctl };
                let empty = match &active.identity {
                    amc_supervision::recovery::RecoveryIdentity::Process(identity) => {
                        amc_supervision::native::terminated(&manager, &active.domain, identity)?
                    }
                    amc_supervision::recovery::RecoveryIdentity::Failed(identity) => {
                        amc_supervision::native::failed_matches(
                            &manager.show(&active.domain)?,
                            &active.domain,
                            identity,
                        ) && amc_supervision::native::empty_failed_slot(identity)?
                    }
                };
                anyhow::ensure!(
                    empty,
                    "original native domain has not terminated; recovery retained"
                );
                snapshot.active = None;
                store.save(&snapshot)?;
            }
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }
    }
    Ok(0)
}
