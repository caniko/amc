//! Root recovery runs inside the configured finite native maintenance unit.
use amc_admission::{
    host_server::{Request, call},
    recovery::{RecoveryAction, RecoveryPolicy, RecoveryTarget},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, clap::Args)]
pub struct RecoveryArgs {
    #[arg(long, default_value = "/run/amc-host/admission.sock")]
    socket: PathBuf,
    /// Restore configured swap devices without acquiring return capacity.
    #[arg(long)]
    restore: bool,
    /// Explicitly use whole-device swapoff instead of bounded page returns.
    #[arg(long, conflicts_with = "restore")]
    whole_device: bool,
}

fn restore(target: &RecoveryTarget) -> Result<()> {
    if amc_admission::swap::device(target)?.is_none() {
        ensure!(
            Command::new("swapon")
                .args(["--priority", &target.priority.to_string(), &target.path])
                .status()?
                .success(),
            "failed to restore swap {}",
            target.name
        );
    }
    ensure!(
        amc_admission::swap::device(target)?.is_some(),
        "swap {} remains inactive",
        target.name
    );
    Ok(())
}

pub fn execute(args: RecoveryArgs) -> Result<i32> {
    let reply = call(&args.socket, &Request::RecoveryTargets { version: 1 })?;
    let targets = reply.recovery_targets.context("missing recovery targets")?;
    let mut waiting = false;
    for target in targets {
        restore(&target)?;
        if args.restore || !args.whole_device {
            continue;
        }
        let reply = call(
            &args.socket,
            &Request::AcquireRecovery {
                version: 1,
                target: target.name.clone(),
            },
        )?;
        if !reply.granted {
            eprintln!("swap recovery {} waiting: {:?}", target.name, reply.waiting);
            waiting = true;
            continue;
        }
        let lease = reply.recovery.context("missing native recovery lease")?;
        let RecoveryAction::Device {
            target,
            before_used_bytes,
        } = lease.action
        else {
            anyhow::bail!("unexpected page return lease");
        };
        // One device per unit lifetime: retain backing through ExecStopPost.
        let result = Command::new("swapoff").arg(&target.path).status();
        restore(&target)?;
        ensure!(
            result?.success(),
            "swapoff {} failed; declared swap restored",
            target.name
        );
        let after = amc_admission::swap::device(&target)?.context("restored swap disappeared")?;
        ensure!(
            after.used_bytes < before_used_bytes,
            "swap recovery made no observed occupancy progress"
        );
        eprintln!(
            "swap recovery {}: used {} -> {} bytes",
            target.name, before_used_bytes, after.used_bytes
        );
        return Ok(0);
    }
    if !args.restore && !args.whole_device {
        return return_pages(
            &args.socket,
            reply
                .recovery_policy
                .context("missing page return policy")?,
        );
    }
    Ok(if waiting { 75 } else { 0 })
}

fn return_pages(socket: &Path, policy: RecoveryPolicy) -> Result<i32> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut returned = 0u64;
    for _ in 0..1024 {
        if Instant::now() >= deadline {
            break;
        }
        let mut progressed = false;
        for pid in amc_admission::page_return::candidates(&policy)? {
            if Instant::now() >= deadline {
                break;
            }
            let target = (|| -> Result<_> {
                let start = amc_admission::host_native::process_start(pid)?;
                let identity = amc_admission::page_return::identity(pid, start, &policy)?;
                if amc_admission::page_return::target_swap(&identity)? == 0 {
                    return Ok(None);
                }
                let memory = amc_admission::page_return::open_memory(&identity)?;
                Ok(Some((identity, memory)))
            })();
            let Ok(Some((target, memory))) = target else {
                continue;
            };
            let range =
                amc_admission::page_return::first_swapped_range(&target, policy.batch_bytes);
            let Ok(Some((address, bytes))) = range else {
                continue;
            };
            let reply = call(
                socket,
                &Request::AcquirePageReturn {
                    version: 1,
                    pid: target.pid,
                    start_ticks: target.start_ticks,
                    address,
                    bytes,
                },
            )?;
            if !reply.granted {
                eprintln!("page return waiting: {:?}", reply.waiting);
                return Ok(75);
            }
            amc_admission::page_return::read_batch(&memory, address, bytes)?;
            drop(memory);
            thread::sleep(Duration::from_millis(10));
            let progress = call(socket, &Request::FinishPageReturn { version: 1 })?
                .returned_bytes
                .unwrap_or(0);
            if progress == 0 {
                eprintln!("page return stalled: no observed target swap reduction");
                return Ok(75);
            }
            returned = returned.saturating_add(progress);
            progressed = true;
            break;
        }
        if !progressed {
            break;
        }
    }
    let remaining = amc_admission::page_return::selected_swap_bytes(&policy)?;
    eprintln!(
        "page return: observed {returned} bytes of target swap reduction, {remaining} bytes remain in selected subtrees"
    );
    // Partial progress, unreadable/unsupported pages and campaign cutoffs remain
    // stalled. The next timer can resume, but this invocation did not complete.
    Ok(if remaining == 0 { 0 } else { 75 })
}
