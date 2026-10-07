//! Source-backed swap return demand and conservative whole-device feasibility.
use crate::{
    host::{HostLedger, Identity},
    recovery::{RecoveryPolicy, RecoveryTarget},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub fn return_bytes(ledger: &HostLedger) -> Result<u64> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let value = |key: &str| -> Result<u64> {
        let mut fields = text
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .context("missing host swap accounting")?
            .split_whitespace();
        let kib: u64 = fields.next().context("missing swap value")?.parse()?;
        ensure!(fields.next() == Some("kB"), "invalid swap unit");
        kib.checked_mul(1024).context("swap accounting overflow")
    };
    let used = value("SwapTotal:")?
        .checked_sub(value("SwapFree:")?)
        .context("invalid host swap occupancy")?;
    let mut covered = 0u64;
    for r in ledger.reservations.iter().filter(|r| r.granted) {
        let directory = Path::new("/sys/fs/cgroup").join(r.identity.cgroup.trim_start_matches('/'));
        ensure!(
            fs::metadata(&directory)?.ino() == r.identity.inode,
            "swap target identity changed"
        );
        let swap: u64 = fs::read_to_string(directory.join("memory.swap.current"))?
            .trim()
            .parse()?;
        covered = covered.saturating_add(swap.min(r.memory_bytes));
    }
    Ok(used.saturating_sub(covered))
}

#[derive(Debug)]
pub struct Device {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub used_bytes: u64,
}

pub fn device(target: &RecoveryTarget) -> Result<Option<Device>> {
    let path = fs::canonicalize(&target.path)?;
    parse_device(&fs::read_to_string("/proc/swaps")?, &path)
}

fn parse_device(text: &str, path: &Path) -> Result<Option<Device>> {
    for line in text.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() == 5, "invalid native swap table");
        if fs::canonicalize(fields[0])? != path {
            continue;
        }
        let size_bytes = fields[2]
            .parse::<u64>()?
            .checked_mul(1024)
            .context("swap size overflow")?;
        let used_bytes = fields[3]
            .parse::<u64>()?
            .checked_mul(1024)
            .context("swap occupancy overflow")?;
        ensure!(
            size_bytes > 0 && used_bytes <= size_bytes,
            "invalid native swap size"
        );
        return Ok(Some(Device {
            path: path.to_owned(),
            size_bytes,
            used_bytes,
        }));
    }
    Ok(None)
}

pub fn helper(pid: i32, policy: &RecoveryPolicy) -> Result<Identity> {
    let start_ticks = crate::host_native::process_start(pid)?;
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let cgroup = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("missing recovery placement")?;
    ensure!(
        cgroup == policy.cgroup,
        "recovery peer is outside configured native boundary"
    );
    let directory = Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/'));
    let max: u64 = fs::read_to_string(directory.join("memory.max"))?
        .trim()
        .parse()?;
    let swap: u64 = fs::read_to_string(directory.join("memory.swap.max"))?
        .trim()
        .parse()?;
    ensure!(
        max > 0 && max <= policy.helper_bytes && swap == 0,
        "unbounded recovery helper"
    );
    Ok(Identity {
        cgroup: cgroup.into(),
        inode: fs::metadata(directory)?.ino(),
        uid: 0,
        pid,
        start_ticks,
    })
}

/// swapoff faults pages into their original memory domains. Host headroom alone
/// is insufficient: every finite leaf and ancestor must fit its own swap return.
pub fn native_return_safe(ledger: &HostLedger) -> Result<bool> {
    native_return_safe_at(Path::new("/sys/fs/cgroup"), ledger)
}

fn native_return_safe_at(root: &Path, ledger: &HostLedger) -> Result<bool> {
    let mut pending = vec![root.to_owned()];
    let mut seen = 0usize;
    while let Some(directory) = pending.pop() {
        seen += 1;
        ensure!(seen <= 8192, "swap feasibility scan exceeds bound");
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
        if directory == root || !directory.join("memory.swap.current").exists() {
            continue;
        }
        let swap: u64 = fs::read_to_string(directory.join("memory.swap.current"))?
            .trim()
            .parse()?;
        if swap == 0 {
            continue;
        }
        let max = fs::read_to_string(directory.join("memory.max"))?;
        if max.trim() == "max" {
            continue;
        }
        let max: u64 = max.trim().parse()?;
        let current: u64 = fs::read_to_string(directory.join("memory.current"))?
            .trim()
            .parse()?;
        let group = format!("/{}", directory.strip_prefix(root)?.display());
        let committed = ledger
            .reservations
            .iter()
            .filter(|r| {
                r.granted
                    && (r.identity.cgroup == group
                        || r.identity.cgroup.starts_with(&format!("{group}/")))
            })
            .fold(0u64, |sum, r| sum.saturating_add(r.memory_bytes));
        if swap > max.saturating_sub(current).saturating_sub(committed) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn return_feasibility_checks_the_pages_leaf_even_when_the_helper_and_host_fit() {
        let root =
            std::env::temp_dir().join(format!("amc-swap-{}", crate::store::fresh_id().unwrap()));
        fs::create_dir_all(root.join("parent/leaf")).unwrap();
        for (group, max, current, swap) in [("parent", 100, 20, 20), ("parent/leaf", 30, 20, 20)] {
            for (name, value) in [
                ("memory.max", max),
                ("memory.current", current),
                ("memory.swap.current", swap),
            ] {
                fs::write(root.join(group).join(name), value.to_string()).unwrap();
            }
        }
        let ledger = HostLedger::new("boot".into());
        assert!(!native_return_safe_at(&root, &ledger).unwrap());
        fs::write(root.join("parent/leaf/memory.max"), "40").unwrap();
        assert!(native_return_safe_at(&root, &ledger).unwrap());
        fs::write(root.join("parent/memory.max"), "39").unwrap();
        assert!(!native_return_safe_at(&root, &ledger).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
