//! Bounded reads through an mm-bound /proc/<pid>/mem descriptor fault actual
//! pages in. No advice-success assumption, process writes, signals, or payload
//! contents in diagnostics. Native swap counters decide whether progress occurred.
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

pub fn target_swap(target: &Identity) -> Result<u64> {
    let directory = crate::native::cgroup_directory(&target.cgroup)?;
    ensure!(
        fs::metadata(&directory)?.ino() == target.inode,
        "page return cgroup changed"
    );
    Ok(fs::read_to_string(directory.join("memory.swap.current"))?
        .trim()
        .parse()?)
}

pub fn native_headroom(target: &Identity, claims: &[Reservation], bytes: u64) -> Result<bool> {
    native_headroom_at(Path::new("/sys/fs/cgroup"), target, claims, bytes)
}

fn native_headroom_at(
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
            if target.cgroup == r.identity.cgroup
                || target
                    .cgroup
                    .starts_with(&format!("{}/", r.identity.cgroup))
            {
                covered_return = covered_return.max(r.memory_bytes.saturating_sub(resident));
            }
            covered_current = covered_current.saturating_add(resident);
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

pub fn candidates(policy: &RecoveryPolicy) -> Result<Vec<i32>> {
    let mut pids = std::collections::BTreeSet::new();
    let mut count = 0usize;
    let mut pending: Vec<_> = policy
        .page_cgroups
        .iter()
        .map(|p| crate::native::cgroup_directory(p))
        .collect::<Result<_>>()?;
    while let Some(directory) = pending.pop() {
        count += 1;
        ensure!(count <= 8192, "page return discovery exceeds bound");
        let groups = fs::read_dir(&directory);
        let groups = match groups {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            other => other?,
        };
        for child in groups {
            let child = child?;
            if child.file_type()?.is_dir() {
                pending.push(child.path());
            }
        }
        if let Ok(procs) = fs::read_to_string(directory.join("cgroup.procs")) {
            for pid in procs.lines() {
                pids.insert(pid.parse()?);
                if pids.len() >= 512 {
                    return Ok(pids.into_iter().collect());
                }
            }
        }
    }
    Ok(pids.into_iter().collect())
}

/// Count the selected subtrees even when no readable process remains. An
/// unreadable mm or a bounded discovery cutoff must never look like recovery.
pub fn selected_swap_bytes(policy: &RecoveryPolicy) -> Result<u64> {
    let mut bytes = 0u64;
    for group in &policy.page_cgroups {
        let directory = crate::native::cgroup_directory(group)?;
        match fs::metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            result => {
                result?;
            }
        }
        let used: u64 = fs::read_to_string(directory.join("memory.swap.current"))?
            .trim()
            .parse()?;
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

pub fn page_size() -> Result<u64> {
    let size = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)?
        .context("native page size unavailable")?;
    ensure!(size > 0, "native page size unavailable");
    Ok(size as u64)
}

pub fn first_swapped_range(target: &Identity, bound: u64) -> Result<Option<(u64, u64)>> {
    let page = page_size()?;
    ensure!(bound >= page, "return batch smaller than page");
    let maps = fs::read_to_string(format!("/proc/{}/maps", target.pid))?;
    let map = File::open(format!("/proc/{}/pagemap", target.pid))?;
    let mut scanned = 0u64;
    let mut entries = [0u8; 4096];
    for mapping in maps.lines().take(8192) {
        let mut fields = mapping.split_whitespace();
        let range = fields.next().context("missing page mapping")?;
        if !fields
            .next()
            .is_some_and(|p| p.starts_with('r') && p.ends_with('p'))
        {
            continue;
        }
        let (first, last) = range.split_once('-').context("invalid page mapping")?;
        let first = u64::from_str_radix(first, 16)?;
        let last = u64::from_str_radix(last, 16)?;
        let mut address = first;
        while address < last {
            let count = ((last - address) / page).min((entries.len() / 8) as u64) as usize;
            if count == 0 {
                break;
            }
            scanned += count as u64;
            if scanned > 8_388_608 {
                return Ok(None);
            }
            map.read_exact_at(
                &mut entries[..count * 8],
                (address / page)
                    .checked_mul(8)
                    .context("pagemap overflow")?,
            )?;
            let swapped = |i: usize| {
                u64::from_ne_bytes(
                    entries[i * 8..i * 8 + 8]
                        .try_into()
                        .expect("eight-byte pagemap entry"),
                ) & (1u64 << 62)
                    != 0
            };
            if let Some(first) = (0..count).find(|i| swapped(*i)) {
                let pages = (first..count)
                    .take((bound / page) as usize)
                    .take_while(|i| swapped(*i))
                    .count();
                return Ok(Some((address + first as u64 * page, pages as u64 * page)));
            }
            address += count as u64 * page;
        }
    }
    Ok(None)
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
}
