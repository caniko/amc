//! Root recovery runs inside the configured finite native maintenance unit.
use amc_admission::{
    host_server::{Request, call},
    page_discovery::{self, ProcessCursor, Scan},
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
    /// Private durable discovery hints, retained between bounded campaigns.
    #[arg(long, default_value = "/var/lib/amc-page-return")]
    state: PathBuf,
    /// Restore configured swap devices without acquiring return capacity.
    #[arg(long)]
    restore: bool,
    /// Explicitly use whole-device swapoff instead of bounded page returns.
    #[arg(long, conflicts_with = "restore")]
    whole_device: bool,
    /// Broker-independent, root-owned JSON target list for emergency restoration.
    #[arg(long, requires = "restore")]
    restore_manifest: Option<PathBuf>,
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
        amc_admission::swap::device(target)?
            .is_some_and(|device| device.priority == target.priority),
        "swap {} is inactive or differs from declared priority {}",
        target.name,
        target.priority
    );
    Ok(())
}

pub fn execute(args: RecoveryArgs) -> Result<i32> {
    if let Some(manifest) = &args.restore_manifest {
        for target in restoration_manifest(manifest)? {
            restore(&target)?;
        }
        return Ok(0);
    }
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
            &args.state,
            reply
                .recovery_policy
                .context("missing page return policy")?,
        ) {
            Ok(code) => Ok(code),
            Err(error) => {
                eprintln!(
                    "page return incomplete: native backing or residency proof unavailable: {error:#}"
                );
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

fn restoration_manifest(path: &Path) -> Result<Vec<RecoveryTarget>> {
    use std::{
        fs::OpenOptions,
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    ensure!(path.is_absolute(), "restoration manifest must be absolute");
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "restoration manifest must be root-owned and not group/other writable"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "restoration manifest exceeds bound");
    let targets: Vec<RecoveryTarget> = serde_json::from_slice(&bytes)?;
    ensure!(!targets.is_empty(), "restoration manifest has no targets");
    amc_admission::recovery::validate_targets(&targets)?;
    Ok(targets)
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

fn return_pages(socket: &Path, state: &Path, policy: RecoveryPolicy) -> Result<i32> {
    if policy.page_cgroups.is_empty() {
        eprintln!("page return incomplete: no page-return subtrees are configured");
        return Ok(75);
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let (store, mut discovery) = amc_admission::store::Store::open_discovery(state, boot.trim())?;
    discovery.select(&policy);
    store.save_discovery(&discovery)?;
    let mut returned = 0u64;
    let mut unproven = false;
    let mut sweep_complete = false;
    // Durable cursors are progress hints, not a residency proof for the prefix
    // that a previous (possibly interrupted or failed) campaign visited.
    let mut sweep = page_discovery::Sweep::new(&discovery);
    for _ in 0..1024 {
        if Instant::now() >= deadline {
            break;
        }
        if discovery.active.is_none() {
            let candidates = page_discovery::candidates(discovery.after_pid)?;
            if candidates.is_empty() {
                let complete = sweep.wrap();
                discovery.wrap();
                store.save_discovery(&discovery)?;
                if complete && amc_admission::page_return::selected_return_bytes(&policy)? == 0 {
                    sweep_complete = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            for pid in candidates {
                if Instant::now() >= deadline {
                    break;
                }
                discovery.after_pid = pid;
                match page_discovery::selected(pid, &policy) {
                    Ok(true) => {
                        match (|| -> Result<_> {
                            let start = amc_admission::host_native::process_start(pid)?;
                            Ok(ProcessCursor {
                                target: amc_admission::page_return::identity(pid, start, &policy)?,
                                layout: page_discovery::layout(pid)?,
                                maps_offset: 0,
                                address: 0,
                            })
                        })() {
                            Ok(cursor) => {
                                discovery.active = Some(cursor);
                                break;
                            }
                            Err(_) => {
                                unproven = true;
                            }
                        }
                    }
                    Ok(false) => (),
                    Err(_) if !Path::new(&format!("/proc/{pid}")).exists() => (),
                    Err(_) => {
                        unproven = true;
                    }
                }
            }
            store.save_discovery(&discovery)?;
            if discovery.active.is_none() {
                continue;
            }
        }
        let cursor = discovery
            .active
            .as_mut()
            .context("missing process cursor")?;
        let pid = cursor.target.pid;
        let target = (|| -> Result<_> {
            let start = amc_admission::host_native::process_start(pid)?;
            let identity = amc_admission::page_return::identity(pid, start, &policy)?;
            let layout = page_discovery::layout(pid)?;
            cursor.revalidate(identity.clone(), layout);
            let memory = amc_admission::page_return::open_memory(&identity)?;
            let pagemap = amc_admission::page_return::open_pagemap(&identity)?;
            ensure!(
                amc_admission::page_return::identity(pid, start, &policy)? == identity,
                "page return placement changed while opening native descriptors"
            );
            Ok((memory, pagemap))
        })();
        let (memory, pagemap) = match target {
            Ok(target) => target,
            Err(_) => {
                eprintln!("page return: native target identity or mm is unproven");
                unproven = true;
                discovery.finish_process();
                store.save_discovery(&discovery)?;
                continue;
            }
        };
        let range = page_discovery::scan(cursor, &pagemap, policy.batch_bytes);
        let (address, bytes) = match range {
            Ok(Scan::Range { address, bytes }) => (address, bytes),
            Ok(Scan::More) => {
                store.save_discovery(&discovery)?;
                continue;
            }
            Ok(Scan::Complete) => {
                discovery.finish_process();
                store.save_discovery(&discovery)?;
                continue;
            }
            Err(_) => {
                eprintln!("page return: native mapping discovery is unproven");
                unproven = true;
                discovery.finish_process();
                store.save_discovery(&discovery)?;
                continue;
            }
        };
        let target = cursor.target.clone();
        // Persist the first unread page. A wait, interruption or lost reply
        // must revisit this range instead of dropping the return obligation.
        store.save_discovery(&discovery)?;
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
        ensure!(
            amc_admission::page_return::identity(target.pid, target.start_ticks, &policy)?
                == target
                && page_discovery::layout(target.pid)?
                    == discovery
                        .active
                        .as_ref()
                        .context("missing native cursor")?
                        .layout,
            "native target changed after page return admission"
        );
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
        discovery
            .active
            .as_mut()
            .context("missing settled cursor")?
            .address = address + bytes;
        store.save_discovery(&discovery)?;
    }
    let remaining = amc_admission::page_return::selected_return_bytes(&policy)?;
    eprintln!(
        "page return: proved {returned} bytes resident, {remaining} nonresident return bytes remain in selected subtrees"
    );
    // Partial progress, unreadable/unsupported pages and campaign cutoffs remain
    // stalled. The next timer can resume, but this invocation did not complete.
    Ok(page_status(remaining, unproven, sweep_complete))
}

fn page_status(remaining: u64, unproven: bool, sweep_complete: bool) -> i32 {
    if remaining == 0 && !unproven && sweep_complete {
        0
    } else {
        75
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_discovery_cutoff_cannot_prove_completion_with_zero_destination_counters() {
        assert_eq!(page_status(0, false, false), 75);
        assert_eq!(page_status(0, true, true), 75);
        assert_eq!(page_status(1, false, true), 75);
        assert_eq!(page_status(0, false, true), 0);
    }

    #[test]
    fn default_recovery_cannot_succeed_without_selected_page_return_subtrees() {
        let policy: RecoveryPolicy = serde_json::from_value(serde_json::json!({
            "cgroup":"/recovery", "helper_bytes":1048576, "minimum_bytes":1,
            "targets":[{"name":"device", "path":"/swap", "priority":10}],
            "page_cgroups":[], "batch_bytes":4096
        }))
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(
            return_pages(Path::new("/unused"), Path::new("/unused-state"), policy).unwrap(),
            75
        );
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
