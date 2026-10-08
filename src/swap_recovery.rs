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
    for target in &targets {
        restore(target)?;
    }
    if args.restore {
        return Ok(0);
    }
    if !args.whole_device {
        return match return_pages(
            &args.socket,
            reply
                .recovery_policy
                .context("missing page return policy")?,
        ) {
            Ok(code) => Ok(code),
            Err(_) => {
                eprintln!("page return incomplete: native backing or residency proof unavailable");
                Ok(75)
            }
        };
    }
    for target in &targets {
        if amc_admission::swap::device(target)?.is_some_and(|device| device.used_bytes == 0) {
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
        break;
    }
    whole_device_status(&targets, |target| {
        Ok(amc_admission::swap::device(target)?.map(|device| device.used_bytes))
    })
}

fn whole_device_status(
    targets: &[RecoveryTarget],
    mut observe: impl FnMut(&RecoveryTarget) -> Result<Option<u64>>,
) -> Result<i32> {
    let mut incomplete = targets.is_empty();
    // A single successful lease is not a receipt for the remaining devices.
    // Reobserve all configured devices, including earlier waits and later work.
    for target in targets {
        incomplete |= observe(target)?.is_none_or(|bytes| bytes > 0);
    }
    Ok(if incomplete { 75 } else { 0 })
}

fn return_pages(socket: &Path, policy: RecoveryPolicy) -> Result<i32> {
    if policy.page_cgroups.is_empty() {
        eprintln!("page return incomplete: no page-return subtrees are configured");
        return Ok(75);
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut returned = 0u64;
    let mut unproven = false;
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
                if amc_admission::page_return::target_usage(&identity)?.used_bytes() == 0 {
                    return Ok(None);
                }
                let memory = amc_admission::page_return::open_memory(&identity)?;
                let pagemap = amc_admission::page_return::open_pagemap(&identity)?;
                Ok(Some((identity, memory, pagemap)))
            })();
            let (target, memory, pagemap) = match target {
                Ok(Some(target)) => target,
                Ok(None) => continue,
                Err(_) => {
                    eprintln!("page return: native target identity or mm is unproven");
                    unproven = true;
                    continue;
                }
            };
            let range =
                amc_admission::page_return::first_swapped_range(&target, policy.batch_bytes);
            let (address, bytes) = match range {
                Ok(Some(range)) => range,
                Ok(None) => continue,
                Err(_) => {
                    eprintln!("page return: native mapping discovery is unproven");
                    unproven = true;
                    continue;
                }
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
            let resident = amc_admission::page_return::range_resident(&pagemap, address, bytes)?;
            drop(memory);
            thread::sleep(Duration::from_millis(10));
            let reply = call(socket, &Request::FinishPageReturn { version: 1 })?;
            if !resident || reply.resident_bytes != Some(bytes) {
                eprintln!("page return stalled: batch residency is unproven");
                return Ok(75);
            }
            returned = returned.saturating_add(bytes);
            progressed = true;
            break;
        }
        if !progressed {
            // Small memory.stat updates can remain buffered until the kernel's
            // periodic rstat flush. An empty PTE scan is not evidence that the
            // selected subtree's RAM-return obligation has settled. Reobserve
            // within this campaign's existing deadline, retaining every proof
            // failure and requiring the actual counter to reach zero.
            if unproven || amc_admission::page_return::selected_return_bytes(&policy)? == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    let remaining = amc_admission::page_return::selected_return_bytes(&policy)?;
    eprintln!(
        "page return: proved {returned} bytes resident, {remaining} nonresident return bytes remain in selected subtrees"
    );
    // Partial progress, unreadable/unsupported pages and campaign cutoffs remain
    // stalled. The next timer can resume, but this invocation did not complete.
    Ok(if remaining == 0 && !unproven { 0 } else { 75 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_recovery_cannot_succeed_without_selected_page_return_subtrees() {
        let policy: RecoveryPolicy = serde_json::from_value(serde_json::json!({
            "cgroup":"/recovery", "helper_bytes":1048576, "minimum_bytes":1,
            "targets":[{"name":"device", "path":"/swap", "priority":10}],
            "page_cgroups":[], "batch_bytes":4096
        }))
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(return_pages(Path::new("/unused"), policy).unwrap(), 75);
    }

    #[test]
    fn whole_device_return_without_device_targets_is_incomplete() {
        assert_eq!(
            whole_device_status(&[], |_| panic!("no target to observe")).unwrap(),
            75
        );
    }

    #[test]
    fn whole_device_success_requires_every_configured_device_to_be_observed_empty() {
        let targets: Vec<RecoveryTarget> = ["blocked", "returned", "later"]
            .into_iter()
            .map(|name| RecoveryTarget {
                name: name.into(),
                path: format!("/swap-{name}"),
                priority: 10,
            })
            .collect();
        let mut examined = Vec::new();
        let code = whole_device_status(&targets, |target| {
            examined.push(target.name.clone());
            Ok(Some(if target.name == "returned" { 0 } else { 4096 }))
        })
        .unwrap();
        assert_eq!(code, 75);
        assert_eq!(examined, ["blocked", "returned", "later"]);
        assert_eq!(whole_device_status(&targets, |_| Ok(Some(0))).unwrap(), 0);
        assert_eq!(whole_device_status(&targets, |_| Ok(None)).unwrap(), 75);
        assert!(whole_device_status(&targets, |_| anyhow::bail!("unavailable")).is_err());
    }
}
