//! Root recovery runs inside the configured finite native maintenance unit.
use amc_admission::{
    host_server::{Request, call},
    page_discovery::{self, ProcessCursor, Scan},
    recovery::{RecoveryAction, RecoveryLease, RecoveryPolicy, RecoveryTarget, TargetIdentity},
};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
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
    restore_with(target, amc_admission::swap::device_from_file, activate_swap)
}

fn activate_swap(path: &Path, priority: i32) -> Result<()> {
    // Linux include/linux/swap.h: explicit priority occupies the low 15 bits.
    const SWAP_FLAG_PREFER: i32 = 0x8000;
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: the CString is NUL terminated and remains alive through the
    // syscall. The descriptor path resolves to the still-open pinned object.
    let result = unsafe { nix::libc::swapon(path.as_ptr(), SWAP_FLAG_PREFER | priority) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn restore_with(
    target: &RecoveryTarget,
    observe: impl FnMut(&std::fs::File) -> Result<Option<amc_admission::swap::Device>>,
    activate: impl FnOnce(&Path, i32) -> Result<()>,
) -> Result<()> {
    let file = amc_admission::swap::open_target(target)?;
    restore_pinned_with(target, &file, observe, activate)
}

fn restore_pinned_with(
    target: &RecoveryTarget,
    file: &std::fs::File,
    mut observe: impl FnMut(&std::fs::File) -> Result<Option<amc_admission::swap::Device>>,
    activate: impl FnOnce(&Path, i32) -> Result<()>,
) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    ensure!(
        (0..=32767).contains(&target.priority),
        "invalid swap priority"
    );
    // The same descriptor pins both deactivation and restoration.
    // Do not use the swapon frontend: its canonicalization can turn this fd path
    // back into the mutable original pathname before the kernel sees it.
    TargetIdentity::from_file(file)?;
    if observe(file)?.is_none() {
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        activate(&path, target.priority)
            .with_context(|| format!("failed to restore swap {}", target.name))?;
    }
    ensure!(
        observe(file)?.is_some_and(|device| device.priority == target.priority),
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
    amc_admission::recovery::validate_targets(&targets)?;
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
        let file = amc_admission::swap::open_target(target)?;
        if amc_admission::swap::device_from_file(&file)?
            .is_some_and(|device| device.used_bytes == 0)
        {
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
        let (target, before_used_bytes) = validated_device_grant(target, &file, &lease)?;
        ensure!(
            amc_admission::swap::device_from_file(&file)?
                .is_some_and(|device| device.size_bytes == lease.return_bytes),
            "approved swap object size or activity changed"
        );
        // One device per unit lifetime: retain backing through ExecStopPost.
        let after = recover_device_with(
            &target,
            &file,
            before_used_bytes,
            amc_admission::swap::device_from_file,
            |path| {
                use std::os::unix::ffi::OsStrExt;
                let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
                // SAFETY: the NUL-terminated path and pinned descriptor remain
                // alive throughout the syscall; no frontend re-resolves aliases.
                if unsafe { nix::libc::swapoff(path.as_ptr()) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(())
            },
            activate_swap,
        )?;
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

fn validated_device_grant(
    target: &RecoveryTarget,
    file: &std::fs::File,
    lease: &RecoveryLease,
) -> Result<(RecoveryTarget, u64)> {
    lease.validate()?;
    let RecoveryAction::Device {
        target: approved,
        before_used_bytes,
        native_target,
    } = &lease.action
    else {
        anyhow::bail!("unexpected page return lease");
    };
    ensure!(
        native_target.as_ref() == Some(&TargetIdentity::from_file(file)?)
            && approved.name == target.name
            && approved.priority == target.priority,
        "recovery grant does not bind the pinned swap object"
    );
    Ok((approved.clone(), *before_used_bytes))
}

fn recover_device_with(
    target: &RecoveryTarget,
    file: &std::fs::File,
    before_used_bytes: u64,
    mut observe: impl FnMut(&std::fs::File) -> Result<Option<amc_admission::swap::Device>>,
    deactivate: impl FnOnce(&Path) -> Result<()>,
    activate: impl FnOnce(&Path, i32) -> Result<()>,
) -> Result<amc_admission::swap::Device> {
    use std::os::unix::io::AsRawFd;
    TargetIdentity::from_file(file)?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    let result = deactivate(&path);
    restore_pinned_with(target, file, &mut observe, activate)?;
    result.with_context(|| format!("swapoff {} failed; declared swap restored", target.name))?;
    let after = observe(file)?.context("restored swap disappeared")?;
    ensure!(
        after.used_bytes < before_used_bytes,
        "swap recovery made no observed occupancy progress"
    );
    Ok(after)
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
    let helper = amc_admission::swap::helper(std::process::id().try_into()?, &policy)?;
    let _helper_guard = amc_admission::page_return::open_memory(&helper)?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(45);
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
    let mut frontier = page_discovery::frontier(&policy)?;
    let mut iterations = 0;
    for _ in 0..1024 {
        if Instant::now() >= deadline {
            break;
        }
        iterations += 1;
        if discovery.active.is_none() {
            let candidates = page_discovery::candidates(discovery.after_pid)?;
            if candidates.is_empty() {
                let current = page_discovery::frontier(&policy)?;
                let complete = sweep.wrap(frontier, current);
                frontier = current;
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
                                maps_fingerprint: None,
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
        thread::sleep(Duration::from_millis(10));
        let reply = call(socket, &Request::FinishPageReturn { version: 1 })?;
        if !resident || reply.resident_bytes != Some(bytes) {
            eprintln!("page return stalled: batch residency is unproven");
            return Ok(75);
        }
        drop(memory);
        returned = returned.saturating_add(bytes);
        discovery
            .active
            .as_mut()
            .context("missing settled cursor")?
            .address = address + bytes;
        store.save_discovery(&discovery)?;
    }
    let remaining = amc_admission::page_return::selected_return_bytes(&policy)?;
    sweep_complete &= page_discovery::frontier(&policy)? == frontier;
    eprintln!(
        "page return: proved {returned} bytes resident, {remaining} nonresident return bytes remain in selected subtrees"
    );
    eprintln!(
        "page return campaign: elapsed_ms={} iterations={iterations} deadline_exhausted={} sweep_complete={sweep_complete} unproven={unproven} after_pid={} active_pid={:?} next_address={:?}",
        started.elapsed().as_millis(),
        Instant::now() >= deadline,
        discovery.after_pid,
        discovery.active.as_ref().map(|cursor| cursor.target.pid),
        discovery.active.as_ref().map(|cursor| cursor.address),
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
    fn whole_device_grants_require_the_native_identity_of_the_retained_object() {
        use std::{fs, os::unix::fs::symlink};
        let root = std::env::temp_dir().join(format!(
            "amc-swap-grant-{}",
            amc_admission::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let original = root.join("original");
        let replacement = root.join("replacement");
        let alias = root.join("alias");
        fs::write(&original, b"approved").unwrap();
        fs::write(&replacement, b"replacement").unwrap();
        symlink(&original, &alias).unwrap();
        let target = RecoveryTarget {
            name: "swap".into(),
            path: alias.to_str().unwrap().into(),
            priority: 17,
        };
        let file = amc_admission::swap::open_target(&target).unwrap();
        let native = TargetIdentity::from_file(&file).unwrap();
        let mut lease = RecoveryLease {
            identity: amc_admission::host::Identity {
                cgroup: "/recovery.service".into(),
                inode: 1,
                uid: 0,
                pid: 1,
                start_ticks: 1,
            },
            invocation_id: Some("1".repeat(32)),
            action: RecoveryAction::Device {
                target: target.clone(),
                before_used_bytes: 1024,
                native_target: Some(native),
            },
            return_bytes: 4096,
            helper_bytes: 1024,
        };
        fs::remove_file(&alias).unwrap();
        symlink(&replacement, &alias).unwrap();
        let encoded = serde_json::to_vec(&lease).unwrap();
        let decoded: RecoveryLease = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            validated_device_grant(&target, &file, &decoded).unwrap().1,
            1024
        );
        let replacement = amc_admission::swap::open_target(&target).unwrap();
        assert!(validated_device_grant(&target, &replacement, &decoded).is_err());
        if let RecoveryAction::Device { native_target, .. } = &mut lease.action {
            *native_target = None;
        }
        // Historical records remain readable, but grant no mutable-path authority.
        lease.validate().unwrap();
        assert!(
            !serde_json::to_string(&lease)
                .unwrap()
                .contains("native_target")
        );
        assert!(validated_device_grant(&target, &file, &lease).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn whole_device_failure_restores_only_the_pinned_object_across_path_replacement() {
        use std::{cell::RefCell, fs, os::unix::fs::symlink};
        for replacement_before_deactivate in [true, false] {
            let root = std::env::temp_dir().join(format!(
                "amc-pinned-handoff-{}",
                amc_admission::store::fresh_id().unwrap()
            ));
            fs::create_dir_all(&root).unwrap();
            let original = root.join("original");
            let replacement = root.join("replacement");
            let alias = root.join("alias");
            fs::write(&original, b"approved swap object").unwrap();
            fs::write(&replacement, b"unapproved replacement").unwrap();
            symlink(&original, &alias).unwrap();
            let file = fs::File::open(&alias).unwrap();
            let target = RecoveryTarget {
                name: "swap".into(),
                path: alias.to_str().unwrap().into(),
                priority: 17,
            };
            let replace = || {
                fs::remove_file(&alias).unwrap();
                symlink(&replacement, &alias).unwrap();
            };
            if replacement_before_deactivate {
                replace();
            }
            let deactivated = RefCell::new(Vec::new());
            let activated = RefCell::new(Vec::new());
            let mut active = false;
            let result = recover_device_with(
                &target,
                &file,
                4096,
                |_| {
                    if active {
                        Ok(Some(amc_admission::swap::Device {
                            path: original.clone(),
                            size_bytes: 4096,
                            used_bytes: 0,
                            priority: 17,
                        }))
                    } else {
                        active = true;
                        Ok(None)
                    }
                },
                |path| {
                    *deactivated.borrow_mut() = fs::read(path)?;
                    if !replacement_before_deactivate {
                        replace();
                    }
                    anyhow::bail!("injected deactivation failure")
                },
                |path, priority| {
                    assert_eq!(priority, 17);
                    *activated.borrow_mut() = fs::read(path)?;
                    Ok(())
                },
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("declared swap restored")
            );
            assert_eq!(
                *deactivated.borrow(),
                b"approved swap object",
                "deactivation followed a replacement pathname"
            );
            assert_eq!(
                *activated.borrow(),
                b"approved swap object",
                "failure restoration activated the unapproved replacement"
            );
            assert_eq!(fs::read(&alias).unwrap(), b"unapproved replacement");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn restoration_activates_the_validated_inode_after_target_path_replacement() {
        use std::{
            fs,
            os::unix::{
                fs::{MetadataExt, symlink},
                io::AsRawFd,
            },
        };
        let root = std::env::temp_dir().join(format!(
            "amc-pinned-restore-{}",
            amc_admission::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        let alias = root.join("alias");
        fs::write(&original, b"validated").unwrap();
        symlink(&original, &alias).unwrap();
        let inode = fs::metadata(&original).unwrap().ino();
        let target = RecoveryTarget {
            name: "swap".into(),
            path: alias.to_str().unwrap().into(),
            priority: 17,
        };
        let mut first = true;
        restore_with(
            &target,
            |file| {
                assert_eq!(file.metadata()?.ino(), inode);
                assert_eq!(
                    fs::read(format!("/proc/self/fd/{}", file.as_raw_fd()))?,
                    b"validated"
                );
                if first {
                    first = false;
                    fs::rename(&original, &moved)?;
                    fs::write(&original, b"replacement")?;
                    Ok(None)
                } else {
                    Ok(Some(amc_admission::swap::Device {
                        path: moved.clone(),
                        size_bytes: 4096,
                        used_bytes: 0,
                        priority: 17,
                    }))
                }
            },
            |path, priority| {
                assert_eq!(priority, 17);
                assert_eq!(
                    fs::read(path)?,
                    b"validated",
                    "activation must resolve the pinned object, not the replaced pathname"
                );
                assert_eq!(fs::read(&alias)?, b"replacement");
                Ok(())
            },
        )
        .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

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
            "cgroup":"/recovery.service", "helper_bytes":1048576, "minimum_bytes":1,
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
