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

/// Resident swap-cache pages already consume RAM (and memory.current). Only
/// the remaining occupied slots need additional RAM to return their pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapUsage {
    pub(crate) used_bytes: u64,
    pub(crate) resident_cache_bytes: u64,
}

impl SwapUsage {
    fn new(used_bytes: u64, resident_cache_bytes: u64) -> Result<Self> {
        ensure!(
            resident_cache_bytes <= used_bytes,
            "inconsistent native swap cache accounting"
        );
        Ok(Self {
            used_bytes,
            resident_cache_bytes,
        })
    }

    pub fn return_bytes(self) -> u64 {
        self.used_bytes - self.resident_cache_bytes
    }

    pub fn used_bytes(self) -> u64 {
        self.used_bytes
    }
}

fn stat_value(text: &str, key: &str) -> Result<u64> {
    let mut matches = text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(key)).then_some(fields)
    });
    let mut fields = matches.next().context("missing native swap accounting")?;
    ensure!(matches.next().is_none(), "duplicate native swap accounting");
    let value = fields.next().context("missing swap value")?.parse()?;
    ensure!(fields.next().is_none(), "invalid native swap value");
    Ok(value)
}

pub fn cgroup_usage(directory: &Path) -> Result<SwapUsage> {
    let current = || -> Result<u64> {
        Ok(fs::read_to_string(directory.join("memory.swap.current"))?
            .trim()
            .parse()?)
    };
    let cached = || -> Result<u64> {
        stat_value(
            &fs::read_to_string(directory.join("memory.stat"))?,
            "swapcached",
        )
    };
    // memory.stat conditionally flushes hierarchical rstat counters; small
    // updates may await the periodic kernel flush. Stable samples are required,
    // but do not force that flush. An inconsistent sample inhibits new work;
    // the recovery campaign waits for outstanding demand to converge to zero.
    for _ in 0..3 {
        let first = current()?;
        let cache = cached()?;
        let middle = current()?;
        let next_cache = cached()?;
        let last = current()?;
        if first == middle && middle == last && cache == next_cache && cache <= last {
            return SwapUsage::new(last, cache);
        }
    }
    anyhow::bail!("unstable native swap cache accounting")
}

pub fn return_bytes(ledger: &HostLedger) -> Result<u64> {
    let text = fs::read_to_string("/proc/meminfo")?;
    return_bytes_at(ledger, &text, Path::new("/sys/fs/cgroup"))
}

fn return_bytes_at(ledger: &HostLedger, text: &str, root: &Path) -> Result<u64> {
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
    let demand = SwapUsage::new(used, value("SwapCached:")?)?.return_bytes();
    let mut covered = 0u64;
    for r in ledger.reservations.iter().filter(|r| r.granted) {
        let directory = root.join(r.identity.cgroup.trim_start_matches('/'));
        ensure!(
            fs::metadata(&directory)?.ino() == r.identity.inode,
            "swap target identity changed"
        );
        // Use disjoint native boundaries: a parent's observation includes its
        // children and cannot provide a second credit for their same pages.
        if ledger.reservations.iter().any(|other| {
            other.granted
                && other.id != r.id
                && (other.identity.cgroup == r.identity.cgroup
                    || other
                        .identity
                        .cgroup
                        .starts_with(&format!("{}/", r.identity.cgroup)))
        }) {
            continue;
        }
        let swap = cgroup_usage(&directory)?.return_bytes();
        let resident: u64 = fs::read_to_string(directory.join("memory.current"))?
            .trim()
            .parse()?;
        covered = covered.saturating_add(swap.min(r.memory_bytes.saturating_sub(resident)));
    }
    Ok(demand.saturating_sub(covered))
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
        let swap = cgroup_usage(&directory)?.return_bytes();
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
    fn resident_swap_cache_is_not_an_additional_ram_obligation() {
        let ledger = HostLedger::new("boot".into());
        let info = "SwapTotal: 100 kB\nSwapFree: 20 kB\nSwapCached: 30 kB\n";
        assert_eq!(
            return_bytes_at(&ledger, info, Path::new("/unused")).unwrap(),
            50 * 1024
        );
        for invalid in [
            "SwapTotal: 100 kB\nSwapFree: 20 kB\n",
            "SwapTotal: 100 kB\nSwapFree: 20 kB\nSwapCached: 81 kB\n",
        ] {
            assert!(return_bytes_at(&ledger, invalid, Path::new("/unused")).is_err());
        }
    }

    #[test]
    fn a_native_grant_only_covers_return_within_its_unused_resident_ceiling() {
        let root = std::env::temp_dir().join(format!(
            "amc-return-credit-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(
            root.join("target/memory.swap.current"),
            (40 * 1024).to_string(),
        )
        .unwrap();
        fs::write(
            root.join("target/memory.stat"),
            format!("swapcached {}\n", 10 * 1024),
        )
        .unwrap();
        fs::write(root.join("target/memory.current"), (58 * 1024).to_string()).unwrap();
        let mut ledger = HostLedger::new("boot".into());
        ledger.reservations.push(crate::host::Reservation {
            id: "grant".into(),
            domain: "target".into(),
            identity: Identity {
                cgroup: "/target".into(),
                inode: fs::metadata(root.join("target")).unwrap().ino(),
                uid: 1000,
                pid: 42,
                start_ticks: 7,
            },
            memory_bytes: 60 * 1024,
            swap_bytes: 40 * 1024,
            requested_ms: 0,
            deadline_ms: 1000,
            granted: true,
            owners: vec![],
            owners_finished: false,
            burst: false,
            runtime_max_ms: None,
            continuation: None,
        });
        let info = "SwapTotal: 100 kB\nSwapFree: 20 kB\nSwapCached: 30 kB\n";
        assert_eq!(return_bytes_at(&ledger, info, &root).unwrap(), 48 * 1024);
        // Overlapping observations must not cover the same return twice.
        let mut duplicate = ledger.reservations[0].clone();
        duplicate.id = "duplicate".into();
        ledger.reservations.push(duplicate);
        assert_eq!(return_bytes_at(&ledger, info, &root).unwrap(), 50 * 1024);
        for invalid in [
            "",
            "swapcached nope\n",
            "swapcached 999999\n",
            "swapcached 0\nswapcached 0\n",
        ] {
            fs::write(root.join("target/memory.stat"), invalid).unwrap();
            assert!(cgroup_usage(&root.join("target")).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn return_feasibility_checks_the_pages_leaf_even_when_the_helper_and_host_fit() {
        let root =
            std::env::temp_dir().join(format!("amc-swap-{}", crate::store::fresh_id().unwrap()));
        fs::create_dir_all(root.join("parent/leaf")).unwrap();
        for (group, max, current, swap) in [("parent", 100, 20, 20), ("parent/leaf", 30, 20, 20)] {
            fs::write(root.join(group).join("memory.stat"), "swapcached 0\n").unwrap();
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
