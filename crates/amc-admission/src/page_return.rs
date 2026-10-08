//! Bounded reads through an mm-bound /proc/<pid>/mem descriptor fault actual
//! pages in. No advice-success assumption, process writes, signals, or payload
//! contents in diagnostics. Native PTEs prove each batch's residency; swap cache
//! and occupied slots remain separate from outstanding RAM-return demand.
use crate::{
    host::{Identity, Reservation},
    recovery::RecoveryPolicy,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File},
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
};

pub fn identity(pid: i32, start_ticks: u64, policy: &RecoveryPolicy) -> Result<Identity> {
    ensure!(
        crate::host_native::process_start(pid)? == start_ticks,
        "page return process changed"
    );
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let text = fs::read_to_string(proc.join("cgroup"))?;
    let group = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("missing page return placement")?;
    ensure!(
        policy
            .page_cgroups
            .iter()
            .any(|prefix| group == prefix || group.starts_with(&format!("{prefix}/"))),
        "page return process is outside selected domains"
    );
    let directory = crate::native::cgroup_directory(group)?;
    let result = Identity {
        cgroup: group.into(),
        inode: fs::metadata(directory)?.ino(),
        uid: fs::metadata(&proc)?.uid(),
        pid,
        start_ticks,
    };
    ensure!(
        crate::host_native::process_start(pid)? == start_ticks,
        "page return process changed"
    );
    Ok(result)
}

pub fn target_usage(target: &Identity) -> Result<crate::swap::SwapUsage> {
    let directory = crate::native::cgroup_directory(&target.cgroup)?;
    ensure!(
        fs::metadata(&directory)?.ino() == target.inode,
        "page return cgroup changed"
    );
    let usage = crate::swap::cgroup_usage(&directory)?;
    ensure!(
        fs::metadata(&directory)?.ino() == target.inode,
        "page return cgroup changed"
    );
    Ok(usage)
}

pub fn native_headroom(target: &Identity, claims: &[Reservation], bytes: u64) -> Result<bool> {
    native_headroom_at(Path::new("/sys/fs/cgroup"), target, claims, bytes)
}

pub fn recovery_headroom(
    target: &Identity,
    helper: &Identity,
    claims: &[Reservation],
    bytes: u64,
    helper_bytes: u64,
) -> Result<bool> {
    recovery_headroom_at(
        Path::new("/sys/fs/cgroup"),
        target,
        helper,
        claims,
        bytes,
        helper_bytes,
    )
}

fn recovery_headroom_at(
    root: &Path,
    target: &Identity,
    helper: &Identity,
    claims: &[Reservation],
    bytes: u64,
    helper_bytes: u64,
) -> Result<bool> {
    let mut claims = claims.to_vec();
    claims.push(crate::recovery::helper_claim(helper, helper_bytes));
    Ok(native_headroom_at(root, helper, &claims, bytes)?
        && native_headroom_at(root, target, &claims, bytes)?
        && charge_owner_headroom_at(root, &claims, bytes)?)
}

/// A swapped PTE does not expose its recorded memcg to userspace. Moving a
/// process does not move its swap charge. For an offlined owner, the direct
/// fault path can use the target mm, while swap-cache reads pass NULL and charge
/// the current helper mm. Back BOTH fallbacks AND every online possible
/// original owner, including shared/mixed-origin pages. Never credit a target's
/// grant as proof that another memcg owns these particular swap entries.
fn charge_owner_headroom_at(root: &Path, claims: &[Reservation], bytes: u64) -> Result<bool> {
    fn visit(
        directory: &Path,
        root: &Path,
        claims: &[Reservation],
        bytes: u64,
        depth: usize,
    ) -> Result<bool> {
        ensure!(
            depth <= 256,
            "native charge-owner hierarchy exceeds depth bound"
        );
        if directory != root && directory.join("memory.swap.current").exists() {
            let swap: u64 = fs::read_to_string(directory.join("memory.swap.current"))?
                .trim()
                .parse()?;
            if swap > 0 {
                let owner = Identity {
                    uid: 0,
                    pid: 0,
                    start_ticks: 0,
                    inode: fs::metadata(directory)?.ino(),
                    cgroup: format!("/{}", directory.strip_prefix(root)?.display()),
                };
                // Hierarchical charge observations may overlap, but every one
                // gets an independent conservative feasibility check.
                if !native_headroom_at(root, &owner, claims, bytes)? {
                    return Ok(false);
                }
            }
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && !visit(&entry.path(), root, claims, bytes, depth + 1)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
    visit(root, root, claims, bytes, 0)
}

pub(crate) fn native_headroom_at(
    root: &Path,
    target: &Identity,
    claims: &[Reservation],
    bytes: u64,
) -> Result<bool> {
    let leaf = root.join(target.cgroup.trim_start_matches('/'));
    ensure!(
        fs::metadata(&leaf)?.ino() == target.inode,
        "page return identity changed"
    );
    for directory in leaf.ancestors().take_while(|p| *p != root) {
        let max = fs::read_to_string(directory.join("memory.max"))?;
        if max.trim() == "max" {
            continue;
        }
        let max: u64 = max.trim().parse()?;
        let current: u64 = fs::read_to_string(directory.join("memory.current"))?
            .trim()
            .parse()?;
        let group = format!("/{}", directory.strip_prefix(root)?.display());
        let mut committed = 0u64;
        let mut covered_current = 0u64;
        let mut covered_return = 0u64;
        for r in claims.iter().filter(|r| {
            r.granted
                && (r.identity.cgroup == group
                    || r.identity.cgroup.starts_with(&format!("{group}/")))
        }) {
            if r.identity.inode == 0 && r.identity.pid == 0 {
                committed = committed.saturating_add(r.memory_bytes);
                continue;
            }
            let boundary = root.join(r.identity.cgroup.trim_start_matches('/'));
            ensure!(
                fs::metadata(&boundary)?.ino() == r.identity.inode,
                "return entitlement identity changed"
            );
            let resident: u64 = fs::read_to_string(boundary.join("memory.current"))?
                .trim()
                .parse()?;
            // Credit only the charged entitlement inside this exact target path.
            // Other grants cannot back these pages merely by sharing an ancestor.
            let overlaps = claims.iter().any(|other| {
                other.granted
                    && other.identity.inode != 0
                    && !std::ptr::eq(other, r)
                    && (other.identity.cgroup == r.identity.cgroup
                        || other
                            .identity
                            .cgroup
                            .starts_with(&format!("{}/", r.identity.cgroup))
                        || r.identity
                            .cgroup
                            .starts_with(&format!("{}/", other.identity.cgroup)))
            });
            if !overlaps
                && (target.cgroup == r.identity.cgroup
                    || target
                        .cgroup
                        .starts_with(&format!("{}/", r.identity.cgroup)))
            {
                covered_return = covered_return.max(r.memory_bytes.saturating_sub(resident));
            }
            if !overlaps {
                covered_current = covered_current.saturating_add(resident);
            }
            committed = committed.saturating_add(r.memory_bytes.max(resident));
        }
        let obligation = current
            .saturating_sub(covered_current)
            .saturating_add(committed)
            .saturating_add(bytes.saturating_sub(covered_return));
        if obligation > max {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Count the selected subtrees even when no readable process remains. An
/// unreadable mm or a bounded discovery cutoff must never look like recovery.
pub fn selected_return_bytes(policy: &RecoveryPolicy) -> Result<u64> {
    let mut bytes = 0u64;
    for group in &policy.page_cgroups {
        let directory = crate::native::cgroup_directory(group)?;
        match fs::metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            result => {
                result?;
            }
        }
        let used = crate::swap::cgroup_usage(&directory)?.return_bytes();
        bytes = bytes
            .checked_add(used)
            .context("selected swap occupancy overflow")?;
    }
    Ok(bytes)
}

/// Open first, then verify the process. proc mem pins the original mm across
/// PID reuse and exec; an obsolete descriptor cannot access a replacement mm.
pub fn open_memory(target: &Identity) -> Result<File> {
    let file = File::open(format!("/proc/{}/mem", target.pid))?;
    ensure!(
        crate::host_native::process_start(target.pid)? == target.start_ticks,
        "page return process changed"
    );
    Ok(file)
}

/// Like proc mem, an opened pagemap pins its original mm across exec/PID reuse.
pub fn open_pagemap(target: &Identity) -> Result<File> {
    let file = File::open(format!("/proc/{}/pagemap", target.pid))?;
    ensure!(
        crate::host_native::process_start(target.pid)? == target.start_ticks,
        "page return process changed"
    );
    Ok(file)
}

pub fn range_resident(pagemap: &File, address: u64, bytes: u64) -> Result<bool> {
    range_matches(pagemap, address, bytes, 1 << 63)
}

pub fn range_swapped(pagemap: &File, address: u64, bytes: u64) -> Result<bool> {
    range_matches(pagemap, address, bytes, 1 << 62)
}

fn range_matches(pagemap: &File, address: u64, bytes: u64, expected: u64) -> Result<bool> {
    let page = page_size()?;
    ensure!(
        bytes > 0
            && bytes <= 16 * 1024 * 1024
            && bytes.is_multiple_of(page)
            && address.is_multiple_of(page)
            && address.checked_add(bytes).is_some(),
        "invalid pagemap proof bound"
    );
    let mut entries = [0u8; 4096];
    let mut pages = bytes / page;
    let mut offset = (address / page)
        .checked_mul(8)
        .context("pagemap overflow")?;
    while pages > 0 {
        let count = pages.min((entries.len() / 8) as u64) as usize;
        pagemap.read_exact_at(&mut entries[..count * 8], offset)?;
        if entries[..count * 8].chunks_exact(8).any(|entry| {
            let entry = u64::from_ne_bytes(entry.try_into().expect("eight-byte pagemap entry"));
            entry & ((1 << 63) | (1 << 62)) != expected
        }) {
            return Ok(false);
        }
        pages -= count as u64;
        offset = offset
            .checked_add(count as u64 * 8)
            .context("pagemap overflow")?;
    }
    Ok(true)
}

pub fn page_size() -> Result<u64> {
    let size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)?
        .context("native page size unavailable")?;
    ensure!(size > 0, "native page size unavailable");
    Ok(size as u64)
}

pub fn read_batch(memory: &File, address: u64, bytes: u64) -> Result<u64> {
    ensure!(
        bytes > 0 && bytes <= 16 * 1024 * 1024 && address.checked_add(bytes).is_some(),
        "invalid native read bound"
    );
    // A small reusable buffer bounds helper RSS independently of the target batch.
    let mut buffer = [0u8; 4096];
    let mut read = 0u64;
    while read < bytes {
        let take = usize::try_from((bytes - read).min(buffer.len() as u64))?;
        let n = memory.read_at(&mut buffer[..take], address + read)?;
        buffer.fill(0);
        ensure!(n > 0, "page return mm is no longer readable");
        read += n as u64;
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostLedger;
    #[test]
    fn charge_owner_scan_does_not_truncate_a_wide_native_hierarchy() {
        let root = std::env::temp_dir().join(format!(
            "amc-wide-owner-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        for i in 0..8193 {
            fs::create_dir(root.join(format!("empty-{i}"))).unwrap();
        }
        let owner = root.join("charged");
        fs::create_dir(&owner).unwrap();
        for (name, value) in [
            ("memory.max", "10"),
            ("memory.current", "10"),
            ("memory.swap.current", "1"),
        ] {
            fs::write(owner.join(name), value).unwrap();
        }
        assert!(!charge_owner_headroom_at(&root, &[], 1).unwrap());
        fs::write(owner.join("memory.max"), "11").unwrap();
        assert!(charge_owner_headroom_at(&root, &[], 1).unwrap());
        fs::write(owner.join("memory.swap.current"), "unproven").unwrap();
        assert!(charge_owner_headroom_at(&root, &[], 1).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overlapping_native_observations_cannot_credit_the_same_resident_bytes_twice() {
        let root = std::env::temp_dir().join(format!(
            "amc-overlap-owner-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(root.join("parent/target")).unwrap();
        for (group, max, current) in [("parent", "100", 80), ("parent/target", "max", 20)] {
            fs::write(root.join(group).join("memory.max"), max).unwrap();
            fs::write(root.join(group).join("memory.current"), current.to_string()).unwrap();
        }
        let target = Identity {
            cgroup: "/parent/target".into(),
            inode: fs::metadata(root.join("parent/target")).unwrap().ino(),
            uid: 1000,
            pid: 42,
            start_ticks: 7,
        };
        let first = Reservation {
            id: "first".into(),
            domain: "target".into(),
            identity: target.clone(),
            memory_bytes: 20,
            swap_bytes: 0,
            requested_ms: 0,
            deadline_ms: 10000,
            granted: true,
            owners: vec![],
            owners_finished: false,
            burst: false,
            runtime_max_ms: None,
            continuation: None,
        };
        let second = Reservation {
            id: "second".into(),
            ..first.clone()
        };
        assert!(!native_headroom_at(&root, &target, &[first, second], 10).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn moved_and_mixed_origin_pages_need_original_owner_and_offline_fallback_backing() {
        let root = std::env::temp_dir().join(format!(
            "amc-charge-owner-{}",
            crate::store::fresh_id().unwrap()
        ));
        for (group, max, current, swap) in [
            ("original", 20, 19, 2),
            ("other-origin", 20, 17, 1),
            ("destination", 40, 10, 0),
            ("helper", 10, 1, 0),
        ] {
            fs::create_dir_all(root.join(group)).unwrap();
            for (name, value) in [
                ("memory.max", max),
                ("memory.current", current),
                ("memory.swap.current", swap),
            ] {
                fs::write(root.join(group).join(name), value.to_string()).unwrap();
            }
        }
        let id = |name: &str, pid| Identity {
            cgroup: format!("/{name}"),
            inode: fs::metadata(root.join(name)).unwrap().ino(),
            uid: 0,
            pid,
            start_ticks: 7,
        };
        let target = id("destination", 42);
        let helper = id("helper", 43);
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 2, 10).unwrap());
        fs::write(root.join("original/memory.max"), "21").unwrap();
        assert!(recovery_headroom_at(&root, &target, &helper, &[], 2, 10).unwrap());
        fs::write(root.join("other-origin/memory.max"), "18").unwrap();
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 2, 10).unwrap());
        // Offlined memcg paths disappear; Linux's fallback is now the target.
        fs::remove_dir_all(root.join("original")).unwrap();
        fs::remove_dir_all(root.join("other-origin")).unwrap();
        assert!(recovery_headroom_at(&root, &target, &helper, &[], 2, 10).unwrap());
        fs::write(root.join("destination/memory.max"), "11").unwrap();
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 2, 10).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn recovery_backs_helper_growth_under_independent_and_shared_ancestors() {
        let root = std::env::temp_dir().join(format!(
            "amc-helper-headroom-{}",
            crate::store::fresh_id().unwrap()
        ));
        for (group, max, current) in [
            ("target", 40, 20),
            ("helper-parent", 24, 5),
            ("helper-parent/helper", 25, 5),
            ("shared", 49, 25),
            ("shared/target", 40, 20),
            ("shared/helper", 25, 5),
        ] {
            fs::create_dir_all(root.join(group)).unwrap();
            fs::write(root.join(group).join("memory.max"), max.to_string()).unwrap();
            fs::write(root.join(group).join("memory.current"), current.to_string()).unwrap();
        }
        let identity = |group: &str, pid| Identity {
            cgroup: format!("/{group}"),
            inode: fs::metadata(root.join(group)).unwrap().ino(),
            uid: 0,
            pid,
            start_ticks: 7,
        };
        let target = identity("target", 42);
        let helper = identity("helper-parent/helper", 43);
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 20, 10).unwrap());
        fs::write(root.join("helper-parent/memory.max"), "25").unwrap();
        assert!(recovery_headroom_at(&root, &target, &helper, &[], 20, 10).unwrap());
        fs::write(root.join("helper-parent/helper/memory.max"), "24").unwrap();
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 20, 10).unwrap());
        let target = identity("shared/target", 42);
        let helper = identity("shared/helper", 43);
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 20, 10).unwrap());
        fs::write(root.join("shared/memory.max"), "50").unwrap();
        assert!(recovery_headroom_at(&root, &target, &helper, &[], 20, 10).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remote_swapcache_fallback_must_fit_unused_helper_resident_capacity() {
        let root = std::env::temp_dir().join(format!(
            "amc-helper-fallback-{}",
            crate::store::fresh_id().unwrap()
        ));
        for (group, current) in [("target", 0), ("helper", 90)] {
            fs::create_dir_all(root.join(group)).unwrap();
            fs::write(root.join(group).join("memory.max"), "100").unwrap();
            fs::write(root.join(group).join("memory.current"), current.to_string()).unwrap();
        }
        let identity = |group: &str, pid| Identity {
            cgroup: format!("/{group}"),
            inode: fs::metadata(root.join(group)).unwrap().ino(),
            uid: 0,
            pid,
            start_ticks: 7,
        };
        let target = identity("target", 42);
        let helper = identity("helper", 43);
        assert!(recovery_headroom_at(&root, &target, &helper, &[], 10, 100).unwrap());
        assert!(!recovery_headroom_at(&root, &target, &helper, &[], 11, 100).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn charged_target_entitlement_backs_its_own_return_without_crediting_a_peer() {
        let root = std::env::temp_dir().join(format!(
            "amc-page-return-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(root.join("parent/target")).unwrap();
        fs::create_dir_all(root.join("parent/peer")).unwrap();
        for (dir, max, current) in [
            ("parent", 100, 40),
            ("parent/target", 40, 20),
            ("parent/peer", 40, 20),
        ] {
            fs::write(root.join(dir).join("memory.max"), max.to_string()).unwrap();
            fs::write(root.join(dir).join("memory.current"), current.to_string()).unwrap();
        }
        let mut target = Identity {
            cgroup: "/parent/target".into(),
            inode: fs::metadata(root.join("parent/target")).unwrap().ino(),
            uid: 1000,
            pid: 42,
            start_ticks: 7,
        };
        let mut ledger = HostLedger::new("boot".into());
        ledger.reservations.push(crate::host::Reservation {
            id: "grant".into(),
            domain: "target".into(),
            identity: target.clone(),
            memory_bytes: 40,
            swap_bytes: 20,
            requested_ms: 0,
            deadline_ms: 1000,
            granted: true,
            owners: vec![],
            owners_finished: false,
            burst: false,
            runtime_max_ms: None,
            continuation: None,
        });
        assert!(native_headroom_at(&root, &target, &ledger.reservations, 20).unwrap());
        assert!(!native_headroom_at(&root, &target, &ledger.reservations, 21).unwrap());
        target.cgroup = "/parent/peer".into();
        target.inode = fs::metadata(root.join("parent/peer")).unwrap().ino();
        assert!(!native_headroom_at(&root, &target, &ledger.reservations, 21).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pinned_read_faults_real_bytes_and_rejects_unbounded_requests() {
        let data = [9u8; 4096];
        let memory = File::open("/proc/self/mem").unwrap();
        assert_eq!(
            read_batch(&memory, data.as_ptr() as u64, 4096).unwrap(),
            4096
        );
        assert!(read_batch(&memory, 0, u64::MAX).is_err());
        assert_eq!(data[4095], 9);
    }

    #[test]
    fn native_residency_requires_every_page_and_rejects_missing_or_conflicting_ptes() {
        let root =
            std::env::temp_dir().join(format!("amc-pagemap-{}", crate::store::fresh_id().unwrap()));
        let present = (1u64 << 63).to_ne_bytes();
        let swapped = (1u64 << 62).to_ne_bytes();
        fs::write(&root, [present, present].concat()).unwrap();
        let pagemap = File::open(&root).unwrap();
        let page = page_size().unwrap();
        assert!(range_resident(&pagemap, 0, page * 2).unwrap());
        assert!(!range_swapped(&pagemap, 0, page * 2).unwrap());
        for second in [swapped, [0; 8], ((1u64 << 63) | (1u64 << 62)).to_ne_bytes()] {
            fs::write(&root, [present, second].concat()).unwrap();
            assert!(!range_resident(&pagemap, 0, page * 2).unwrap());
        }
        fs::write(&root, [swapped, swapped].concat()).unwrap();
        assert!(range_swapped(&pagemap, 0, page * 2).unwrap());
        fs::write(&root, swapped).unwrap();
        assert!(range_swapped(&pagemap, 0, page * 2).is_err());
        assert!(range_resident(&pagemap, 1, page).is_err());
        assert!(range_resident(&pagemap, 0, 0).is_err());
        assert!(range_resident(&pagemap, 0, 32 * 1024 * 1024).is_err());
        fs::remove_file(root).unwrap();
    }
}
