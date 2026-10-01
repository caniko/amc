//! Root broker observations of native, finite execution domains.
use crate::host::{Capacity, Domain, Identity};
use anyhow::{Context, Result, ensure};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

pub fn process_start(pid: i32) -> Result<u64> {
    ensure!(pid > 0, "invalid workload PID");
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .context("missing process identity")?
        .1
        .split_whitespace()
        .nth(19)
        .context("missing process start time")?
        .parse()?)
}

fn number(path: &Path, name: &str) -> Result<u64> {
    Ok(fs::read_to_string(path.join(name))?.trim().parse()?)
}

/// Only a helper already inside its native boundary may request capacity.
pub fn identify(pid: i32, uid: u32, domains: &[Domain]) -> Result<(String, Identity, u64, u64)> {
    let start_ticks = process_start(pid)?;
    let process = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let cgroup = process
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .context("peer has no cgroup v2 placement")?;
    let domain = domains
        .iter()
        .find(|d| {
            d.uid == uid
                && cgroup
                    .strip_prefix(&d.cgroup)
                    .is_some_and(|s| s.starts_with('/'))
        })
        .context("peer is outside enrolled host domains")?;
    // User service groups have no automatic restart and a server-generated name.
    let name = Path::new(cgroup)
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid workload group")?;
    ensure!(
        name.starts_with("app-amc-job-") && name.ends_with(".service"),
        "host admission requires a native AMC entry helper"
    );
    let directory = crate::native::cgroup_directory(cgroup)?;
    let memory = number(&directory, "memory.max")?;
    let swap = number(&directory, "memory.swap.max")?;
    ensure!(
        memory > 0 && memory <= domain.ceiling_bytes && swap <= domain.swap_bytes,
        "native ceiling exceeds enrolled host contract"
    );
    ensure!(
        process_start(pid)? == start_ticks,
        "peer process identity changed"
    );
    Ok((
        domain.name.clone(),
        Identity {
            cgroup: cgroup.into(),
            inode: fs::metadata(directory)?.ino(),
            uid,
            pid,
            start_ticks,
        },
        memory,
        swap,
    ))
}

pub fn empty(identity: &Identity) -> Option<bool> {
    let directory = crate::native::cgroup_directory(&identity.cgroup).ok()?;
    empty_directory(identity, &directory)
}

fn empty_directory(identity: &Identity, directory: &Path) -> Option<bool> {
    match fs::metadata(directory) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
        Ok(m) if m.ino() == identity.inode => {
            let events = fs::read_to_string(directory.join("cgroup.events")).ok()?;
            if events.lines().any(|l| l == "populated 0") {
                Some(true)
            } else if events.lines().any(|l| l == "populated 1") {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn empty_reservation(r: &crate::host::Reservation) -> Option<bool> {
    let observed = empty(&r.identity);
    if observed != Some(true) {
        return observed;
    }
    if r.identity.uid != 0 {
        return Some(true);
    }
    if r.owners.is_empty() {
        return None;
    }
    for owner in &r.owners {
        match fs::metadata(format!("/proc/{}", owner.pid)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
            Ok(_) => {
                if process_start(owner.pid).ok()? == owner.start_ticks {
                    return Some(false);
                }
            }
        }
    }
    Some(true)
}

/// Any changed or unreadable granted boundary inhibits every new grant.
pub fn enforcement(r: &crate::host::Reservation) -> Option<()> {
    let path = crate::native::cgroup_directory(&r.identity.cgroup).ok()?;
    if fs::metadata(&path).ok()?.ino() != r.identity.inode
        || number(&path, "memory.max").ok()? != r.memory_bytes
        || number(&path, "memory.swap.max").ok()? != r.swap_bytes
    {
        return None;
    }
    Some(())
}

pub fn owner_alive(owner: &crate::ledger::ClientIdentity) -> Option<bool> {
    match fs::metadata(format!("/proc/{}", owner.pid)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
        Ok(_) => Some(process_start(owner.pid).ok()? == owner.start_ticks),
    }
}

pub fn identify_pool(
    pid: i32,
    uid: u32,
    name: &str,
    domains: &[Domain],
) -> Result<(String, Identity, u64, u64)> {
    ensure!(uid == 0, "only a root execution owner may enroll a pool");
    let d = domains
        .iter()
        .find(|d| d.uid == 0 && d.name == name)
        .context("unknown execution-owner pool")?;
    let directory = crate::native::cgroup_directory(&d.cgroup)?;
    ensure!(
        number(&directory, "memory.max")? == d.ceiling_bytes
            && number(&directory, "memory.swap.max")? == d.swap_bytes,
        "pool enforcement differs from host contract"
    );
    Ok((
        d.name.clone(),
        Identity {
            cgroup: d.cgroup.clone(),
            inode: fs::metadata(directory)?.ino(),
            uid,
            pid,
            start_ticks: process_start(pid)?,
        },
        d.ceiling_bytes,
        d.swap_bytes,
    ))
}

pub fn ancestor_headroom(
    identity: &Identity,
    reservations: &[crate::host::Reservation],
) -> Option<u64> {
    let directory = crate::native::cgroup_directory(&identity.cgroup).ok()?;
    if fs::metadata(&directory).ok()?.ino() != identity.inode {
        return None;
    }
    let mut available = u64::MAX;
    for ancestor in directory
        .ancestors()
        .skip(1)
        .take_while(|p| *p != Path::new("/sys/fs/cgroup"))
    {
        let max = fs::read_to_string(ancestor.join("memory.max")).ok()?;
        if max.trim() == "max" {
            continue;
        }
        let max: u64 = max.trim().parse().ok()?;
        let prefix = format!(
            "/{}/",
            ancestor.strip_prefix("/sys/fs/cgroup").ok()?.display()
        );
        let committed = reservations
            .iter()
            .filter(|r| r.granted && r.identity.cgroup.starts_with(&prefix))
            .fold(0u64, |sum, r| sum.saturating_add(r.memory_bytes));
        available = available.min(
            max.saturating_sub(number(ancestor, "memory.current").ok()?)
                .saturating_sub(committed),
        );
    }
    Some(available)
}

pub fn capacity() -> Result<Capacity> {
    let memory = fs::read_to_string("/proc/meminfo")?;
    let available = memory
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .context("missing MemAvailable")?;
    let mut fields = available.split_whitespace();
    let kib: u64 = fields.next().context("missing RAM capacity")?.parse()?;
    ensure!(fields.next() == Some("kB"), "invalid RAM unit");
    let swap = memory
        .lines()
        .find_map(|l| l.strip_prefix("SwapFree:"))
        .context("missing swap accounting")?;
    let mut fields = swap.split_whitespace();
    let swap_kib: u64 = fields.next().context("missing free swap")?.parse()?;
    ensure!(fields.next() == Some("kB"), "invalid swap unit");
    Ok(Capacity {
        available_bytes: kib.checked_mul(1024).context("RAM capacity overflow")?,
        swap_free_bytes: swap_kib
            .checked_mul(1024)
            .context("swap accounting overflow")?,
        memory_full_psi: psi("memory")?,
        io_full_psi: psi("io")?,
    })
}

fn psi(kind: &str) -> Result<f64> {
    let text = fs::read_to_string(format!("/proc/pressure/{kind}"))?;
    let full = text
        .lines()
        .find(|l| l.starts_with("full "))
        .context("missing full PSI")?;
    let value: f64 = full
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))
        .context("missing PSI avg10")?
        .parse()?;
    ensure!(
        value.is_finite() && (0.0..=100.0).contains(&value),
        "invalid PSI percentage"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surviving_descendants_unknown_events_and_replaced_cgroups_hold_capacity() {
        let path = std::env::temp_dir().join(format!(
            "amc-native-host-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        let mut id = Identity {
            cgroup: "/test/job".into(),
            inode: fs::metadata(&path).unwrap().ino(),
            uid: 1000,
            pid: 1,
            start_ticks: 1,
        };
        fs::write(path.join("cgroup.events"), "populated 1\n").unwrap();
        assert_eq!(empty_directory(&id, &path), Some(false));
        fs::write(path.join("cgroup.events"), "unavailable\n").unwrap();
        assert_eq!(empty_directory(&id, &path), None);
        fs::write(path.join("cgroup.events"), "populated 0\n").unwrap();
        assert_eq!(empty_directory(&id, &path), Some(true));
        id.inode += 1;
        assert_eq!(empty_directory(&id, &path), None);
        fs::remove_dir_all(&path).unwrap();
        assert_eq!(empty_directory(&id, &path), Some(true));
    }

    #[test]
    fn an_empty_root_pool_is_retained_while_any_execution_owner_can_start_work() {
        let pid = std::process::id() as i32;
        let ticks = process_start(pid).unwrap();
        let mut r = crate::host::Reservation {
            id: "pool".into(),
            domain: "builders".into(),
            identity: Identity {
                cgroup: format!("/amc-test-{}", crate::store::fresh_id().unwrap()),
                inode: 1,
                uid: 0,
                pid,
                start_ticks: ticks,
            },
            memory_bytes: 100,
            swap_bytes: 0,
            requested_ms: 0,
            deadline_ms: 1,
            granted: true,
            owners: vec![crate::ledger::ClientIdentity {
                pid,
                start_ticks: ticks,
            }],
        };
        assert_eq!(empty_reservation(&r), Some(false));
        r.owners.insert(
            0,
            crate::ledger::ClientIdentity {
                pid,
                start_ticks: ticks + 1,
            },
        );
        assert_eq!(empty_reservation(&r), Some(false));
        r.owners.pop();
        assert_eq!(empty_reservation(&r), Some(true));
        r.owners.clear();
        assert_eq!(empty_reservation(&r), None);
    }
}
