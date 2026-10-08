//! Root broker observations of native, finite execution domains.
use crate::host::{Capacity, Domain, Identity};
use anyhow::{Context, Result, ensure};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

pub fn process_start(pid: i32) -> Result<u64> {
    process_start_at(Path::new("/proc"), pid)
}

fn process_start_at(proc: &Path, pid: i32) -> Result<u64> {
    ensure!(pid > 0, "invalid workload PID");
    let stat = fs::read_to_string(proc.join(pid.to_string()).join("stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .context("missing process identity")?
        .1
        .split_whitespace()
        .nth(19)
        .context("missing process start time")?
        .parse()?)
}

/// A host-native user helper can transfer its own preparation to a consenting
/// peer. PID/start-time and the real host UID are rechecked by the root broker;
/// a namespace-local PID or a different user's process has no authority here.
pub fn prepared_origin(origin: &crate::ledger::ClientIdentity, uid: u32) -> Result<i32> {
    ensure!(
        fs::metadata(format!("/proc/{}", origin.pid))?.uid() == uid
            && process_start(origin.pid)? == origin.start_ticks,
        "prepared origin identity or UID changed"
    );
    Ok(origin.pid)
}

fn number(path: &Path, name: &str) -> Result<u64> {
    Ok(fs::read_to_string(path.join(name))?.trim().parse()?)
}

/// Only a helper already inside its native boundary may request capacity.
pub fn identify(pid: i32, uid: u32, domains: &[Domain]) -> Result<(String, Identity, u64, u64)> {
    identify_entry(pid, uid, domains, false)
}

/// Prepared scopes retain the submitting application's filesystem namespace.
/// Only authenticated Consume, never ordinary Acquire, accepts this entry kind.
pub fn identify_prepared(
    pid: i32,
    uid: u32,
    domains: &[Domain],
) -> Result<(String, Identity, u64, u64)> {
    identify_entry(pid, uid, domains, true)
}

/// A committed Consume replay authenticates the original native grant rather
/// than re-admitting its envelope against a possibly replaced host policy.
pub fn identify_prepared_replay(
    pid: i32,
    uid: u32,
    granted: &crate::host::Reservation,
) -> Result<(String, Identity, u64, u64)> {
    identify_prepared_replay_at(
        Path::new("/proc"),
        Path::new("/sys/fs/cgroup"),
        pid,
        uid,
        granted,
    )
}

fn identify_prepared_replay_at(
    proc: &Path,
    root: &Path,
    pid: i32,
    uid: u32,
    granted: &crate::host::Reservation,
) -> Result<(String, Identity, u64, u64)> {
    let identity = &granted.identity;
    ensure!(
        granted.granted
            && pid == identity.pid
            && uid == identity.uid
            && process_start_at(proc, pid)? == identity.start_ticks,
        "consumed preparation peer changed"
    );
    let placement = fs::read_to_string(proc.join(pid.to_string()).join("cgroup"))?;
    ensure!(
        placement.lines().find_map(|line| line.strip_prefix("0::"))
            == Some(identity.cgroup.as_str())
            && Path::new(&identity.cgroup)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(
                    |name| name.starts_with("app-amc-prepared-") && name.ends_with(".scope")
                ),
        "consumed preparation placement changed"
    );
    crate::native::cgroup_directory(&identity.cgroup)?;
    let directory = root.join(identity.cgroup.trim_start_matches('/'));
    ensure!(
        fs::metadata(&directory)?.ino() == identity.inode
            && number(&directory, "memory.max")? == granted.memory_bytes
            && number(&directory, "memory.swap.max")? == granted.swap_bytes
            && process_start_at(proc, pid)? == identity.start_ticks,
        "consumed preparation enforcement changed"
    );
    Ok((
        granted.domain.clone(),
        identity.clone(),
        granted.memory_bytes,
        granted.swap_bytes,
    ))
}

fn identify_entry(
    pid: i32,
    uid: u32,
    domains: &[Domain],
    prepared: bool,
) -> Result<(String, Identity, u64, u64)> {
    identify_entry_at(
        Path::new("/proc"),
        Path::new("/sys/fs/cgroup"),
        pid,
        uid,
        domains,
        prepared,
    )
}

fn identify_entry_at(
    proc: &Path,
    root: &Path,
    pid: i32,
    uid: u32,
    domains: &[Domain],
    prepared: bool,
) -> Result<(String, Identity, u64, u64)> {
    let start_ticks = process_start_at(proc, pid)?;
    let process = fs::read_to_string(proc.join(pid.to_string()).join("cgroup"))?;
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
        if prepared {
            name.starts_with("app-amc-prepared-") && name.ends_with(".scope")
        } else {
            name.starts_with("app-amc-job-") && name.ends_with(".service")
        },
        "host admission requires a native AMC entry helper"
    );
    crate::native::cgroup_directory(cgroup)?;
    let directory = root.join(cgroup.trim_start_matches('/'));
    let memory = number(&directory, "memory.max")?;
    let swap = number(&directory, "memory.swap.max")?;
    ensure!(
        memory > 0 && memory <= domain.ceiling_bytes && swap <= domain.swap_bytes,
        "native ceiling exceeds enrolled host contract"
    );
    ensure!(
        process_start_at(proc, pid)? == start_ticks,
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
        return r.owners_finished.then_some(true);
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
    if r.burst && burst_runtime(&r.identity).ok()? != r.runtime_max_ms? {
        return None;
    }
    Some(())
}

/// Query the execution owner's actual manager, rather than trusting an estimate
/// or the submitting helper's argv. The root broker can address each user bus.
pub fn burst_runtime(identity: &Identity) -> Result<u64> {
    let text = amc_runner::systemd::capture(std::process::Command::new("systemctl").args([
        "--user", &format!("--machine={}@.host", identity.uid), "show", "--no-pager",
        "--property=ControlGroup,MainPID,RuntimeMaxUSec,RuntimeRandomizedExtraUSec,TimeoutStopUSec,SendSIGKILL,FinalKillSignal,Restart,KillMode,OOMPolicy",
        "--", Path::new(&identity.cgroup).file_name().and_then(|s| s.to_str()).context("missing burst unit")?,
    ]))?;
    let runtime = verified_burst_runtime(&text, identity)?;
    ensure!(
        process_start(identity.pid)? == identity.start_ticks,
        "burst process identity changed"
    );
    Ok(runtime)
}

fn verified_burst_runtime(text: &str, identity: &Identity) -> Result<u64> {
    let fields: std::collections::BTreeMap<_, _> =
        text.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |name| fields.get(name).copied().unwrap_or("");
    ensure!(
        get("ControlGroup") == identity.cgroup
            && get("MainPID").parse::<i32>()? == identity.pid
            && get("Restart") == "no"
            && get("KillMode") == "control-group"
            && get("OOMPolicy") == "kill"
            && get("SendSIGKILL") == "yes"
            && get("FinalKillSignal") == "9"
            && crate::native::duration_us(get("RuntimeRandomizedExtraUSec")) == Some(0)
            && crate::native::duration_us(get("TimeoutStopUSec"))
                .is_some_and(|timeout| timeout > 0 && timeout <= 1_000_000),
        "burst native identity or cleanup enforcement mismatch"
    );
    let micros = crate::native::duration_us(get("RuntimeMaxUSec"))
        .context("missing finite native runtime")?;
    ensure!(
        micros > 0 && micros <= 30_000_000 && micros.is_multiple_of(1000),
        "burst requires a finite short native deadline"
    );
    Ok(micros / 1000)
}

pub fn owner_alive(owner: &crate::ledger::ClientIdentity) -> Option<bool> {
    match fs::metadata(format!("/proc/{}", owner.pid)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
        Ok(_) => Some(process_start(owner.pid).ok()? == owner.start_ticks),
    }
}

fn recovery_unit(identity: &Identity) -> Result<String> {
    let unit = Path::new(&identity.cgroup)
        .file_name()
        .and_then(|name| name.to_str())
        .context("missing recovery unit")?;
    ensure!(
        unit.ends_with(".service"),
        "recovery requires a native service boundary"
    );
    amc_runner::systemd::capture(std::process::Command::new("systemctl").args([
        "show",
        "--property=ControlGroup,MainPID,Restart,KillMode,InvocationID",
        "--",
        unit,
    ]))
}

fn verified_recovery_unit(text: &str, cgroup: &str) -> Result<(i32, String)> {
    let fields: std::collections::BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let get = |key| fields.get(key).copied().unwrap_or("");
    let invocation = get("InvocationID");
    ensure!(
        get("ControlGroup") == cgroup
            && get("Restart") == "no"
            && get("KillMode") == "control-group"
            && invocation.len() == 32
            && invocation.bytes().all(|b| b.is_ascii_hexdigit())
            && invocation.bytes().any(|b| b != b'0'),
        "recovery requires a non-restarting, invocation-bound control-group service"
    );
    let pid: i32 = get("MainPID").parse()?;
    ensure!(pid > 0, "recovery service has no main process");
    Ok((pid, invocation.into()))
}

pub fn recovery_invocation(identity: &Identity) -> Result<String> {
    let (pid, invocation) = verified_recovery_unit(&recovery_unit(identity)?, &identity.cgroup)?;
    ensure!(
        pid == identity.pid && process_start(pid)? == identity.start_ticks,
        "recovery peer is not the current native main process"
    );
    Ok(invocation)
}

/// A new systemd invocation of a KillMode=control-group service starts only
/// after the previous invocation's processes have been stopped. Same-invocation
/// descendants, legacy leases and unavailable observations remain charged.
pub fn recovery_replaced(lease: &crate::recovery::RecoveryLease) -> Option<bool> {
    let alive = owner_alive(&crate::ledger::ClientIdentity {
        pid: lease.identity.pid,
        start_ticks: lease.identity.start_ticks,
    });
    // Normal live-helper ticks need no manager subprocess. Unknown liveness
    // and legacy leases also retain capacity without querying a replacement.
    if alive != Some(false) || lease.invocation_id.is_none() {
        return Some(false);
    }
    recovery_replaced_at(lease, &recovery_unit(&lease.identity).ok()?, alive)
}

fn recovery_replaced_at(
    lease: &crate::recovery::RecoveryLease,
    text: &str,
    alive: Option<bool>,
) -> Option<bool> {
    let previous = lease.invocation_id.as_ref()?;
    if alive != Some(false) {
        return Some(false);
    }
    let (_, current) = verified_recovery_unit(text, &lease.identity.cgroup).ok()?;
    Some(&current != previous)
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

/// RAM available to this reservation after accounting both native resources.
/// A finite ancestor swap ceiling that cannot back the requested swap makes
/// the reservation ineligible even when ancestor and host RAM are plentiful.
pub fn ancestor_headroom(
    reservation: &crate::host::Reservation,
    reservations: &[crate::host::Reservation],
) -> Option<u64> {
    ancestor_headroom_at(Path::new("/sys/fs/cgroup"), reservation, reservations)
}

fn ancestor_headroom_at(
    root: &Path,
    reservation: &crate::host::Reservation,
    reservations: &[crate::host::Reservation],
) -> Option<u64> {
    let identity = &reservation.identity;
    crate::native::cgroup_directory(&identity.cgroup).ok()?;
    let directory = root.join(identity.cgroup.trim_start_matches('/'));
    let envelope = identity.inode == 0 && identity.pid == 0;
    if !envelope && fs::metadata(&directory).ok()?.ino() != identity.inode {
        return None;
    }
    let mut available = u64::MAX;
    for ancestor in directory
        .ancestors()
        .skip(usize::from(!envelope))
        .take_while(|p| *p != root)
    {
        let prefix = format!("/{}/", ancestor.strip_prefix(root).ok()?.display());
        let (memory_committed, swap_committed) = reservations
            .iter()
            .filter(|r| {
                r.granted
                    && (r.identity.cgroup == prefix.trim_end_matches('/')
                        || r.identity.cgroup.starts_with(&prefix))
            })
            .fold((0u64, 0u64), |(memory, swap), r| {
                (
                    memory.saturating_add(r.memory_bytes),
                    swap.saturating_add(r.swap_bytes),
                )
            });
        for (max_file, current_file, committed, swap) in [
            ("memory.max", "memory.current", memory_committed, false),
            (
                "memory.swap.max",
                "memory.swap.current",
                swap_committed,
                true,
            ),
        ] {
            let max = fs::read_to_string(ancestor.join(max_file)).ok()?;
            if max.trim() == "max" {
                continue;
            }
            let max: u64 = max.trim().parse().ok()?;
            // A synthetic lane spanning the entire enforced native pool also
            // covers that pool's residual cache/kernel charges. Its ceiling is
            // a total envelope, not additional allocation on top of those bytes.
            // Partial lanes and sibling usage receive no such credit.
            let ceiling = if swap {
                reservation.swap_bytes
            } else {
                reservation.memory_bytes
            };
            let covered = if envelope
                && fs::read_to_string(directory.join(max_file))
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()
                    == Some(ceiling)
            {
                number(&directory, current_file).ok()?.min(ceiling)
            } else {
                0
            };
            let remaining = max
                .saturating_sub(number(ancestor, current_file).ok()?.saturating_sub(covered))
                .saturating_sub(committed);
            if swap {
                if reservation.swap_bytes > remaining {
                    return Some(0);
                }
            } else {
                available = available.min(remaining);
            }
        }
    }
    Some(available)
}

pub fn capacity() -> Result<Capacity> {
    capacity_at(Path::new("/proc"))
}

fn capacity_at(proc: &Path) -> Result<Capacity> {
    let memory = fs::read_to_string(proc.join("meminfo"))?;
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
        memory_full_psi: psi(&proc.join("pressure/memory"))?,
        // Keep memory observations fail-closed. Missing or malformed I/O is
        // represented as unknown so only I/O-enforcing domains stop admission.
        io_full_psi: psi(&proc.join("pressure/io")).unwrap_or(f64::NAN),
    })
}

fn psi(path: &Path) -> Result<f64> {
    let text = fs::read_to_string(path)?;
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
    fn replacement_recovery_invocation_cannot_hide_a_dead_helper_or_release_descendants() {
        let lease: crate::recovery::RecoveryLease = serde_json::from_value(serde_json::json!({
            "identity":{"cgroup":"/system.slice/return.service","inode":1,"uid":0,"pid":42,"start_ticks":7},
            "invocation_id":"11111111111111111111111111111111",
            "action":{"kind":"device","target":{"name":"swap","path":"/dev/swap","priority":10},"before_used_bytes":1},
            "helper_bytes":10,"return_bytes":100
        })).unwrap();
        let same = "ControlGroup=/system.slice/return.service\nMainPID=43\nRestart=no\nKillMode=control-group\nInvocationID=11111111111111111111111111111111\n";
        let replacement = same.replace(
            "InvocationID=11111111111111111111111111111111",
            "InvocationID=22222222222222222222222222222222",
        );
        assert_eq!(recovery_replaced_at(&lease, same, Some(false)), Some(false));
        assert_eq!(
            recovery_replaced_at(&lease, &replacement, Some(false)),
            Some(true)
        );
        assert_eq!(
            recovery_replaced_at(&lease, &replacement, Some(true)),
            Some(false)
        );
        assert_eq!(
            recovery_replaced_at(&lease, &replacement, None),
            Some(false)
        );
        for invalid in [
            replacement.replace("Restart=no", "Restart=always"),
            replacement.replace("KillMode=control-group", "KillMode=process"),
            replacement.replace("MainPID=43", "MainPID=0"),
            replacement.replace("/system.slice/return.service", "/foreign.service"),
        ] {
            assert_eq!(recovery_replaced_at(&lease, &invalid, Some(false)), None);
        }
        let mut legacy = lease.clone();
        legacy.invocation_id = None;
        assert_eq!(
            recovery_replaced_at(&legacy, &replacement, Some(false)),
            None
        );
    }

    #[test]
    fn full_native_completion_envelope_covers_its_residual_charge_but_not_siblings() {
        use crate::host::Reservation;
        let root = std::env::temp_dir().join(format!(
            "amc-envelope-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(root.join("parent/builders")).unwrap();
        for (group, max, current) in [("parent", 100, 15), ("parent/builders", 96, 12)] {
            for (file, value) in [
                ("memory.max", max),
                ("memory.current", current),
                ("memory.swap.max", 0),
                ("memory.swap.current", 0),
            ] {
                fs::write(root.join(group).join(file), value.to_string()).unwrap();
            }
        }
        let lane: Reservation = serde_json::from_value(serde_json::json!({
            "id":"escrow", "domain":"builders", "memory_bytes":96,"swap_bytes":0,
            "requested_ms":0,"deadline_ms":10000,"granted":true,"owners":[],
            "identity":{"cgroup":"/parent/builders","inode":0,"uid":0,"pid":0,"start_ticks":0}
        }))
        .unwrap();
        assert_eq!(ancestor_headroom_at(&root, &lane, &[]), Some(96));
        fs::write(root.join("parent/memory.current"), "17").unwrap();
        assert_eq!(ancestor_headroom_at(&root, &lane, &[]), Some(95));
        // A partial lane cannot cover unrelated usage of an entire native pool.
        let mut partial = lane.clone();
        partial.memory_bytes = 90;
        assert_eq!(ancestor_headroom_at(&root, &partial, &[]), Some(83));
        fs::write(root.join("parent/memory.current"), "15").unwrap();
        let mut peer = lane.clone();
        peer.id = "other-lane".into();
        assert_eq!(ancestor_headroom_at(&root, &lane, &[peer]), Some(0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn consumed_replay_uses_the_persisted_grant_and_rejects_changed_native_peers() {
        let root = std::env::temp_dir().join(format!(
            "amc-prepared-replay-{}",
            crate::store::fresh_id().unwrap()
        ));
        let proc = root.join("proc");
        let groups = root.join("cgroups");
        let group = "/retired.slice/app-amc-prepared-intent.scope";
        let directory = groups.join(group.trim_start_matches('/'));
        fs::create_dir_all(proc.join("42")).unwrap();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            proc.join("42/stat"),
            format!("42 (prepared) S {}7\n", "0 ".repeat(18)),
        )
        .unwrap();
        fs::write(proc.join("42/cgroup"), format!("0::{group}\n")).unwrap();
        fs::write(directory.join("memory.max"), "40").unwrap();
        fs::write(directory.join("memory.swap.max"), "10").unwrap();
        let grant: crate::host::Reservation = serde_json::from_value(serde_json::json!({
            "id":"intent", "domain":"removed", "memory_bytes":40, "swap_bytes":10,
            "requested_ms":0, "deadline_ms":10000, "granted":true, "owners":[],
            "identity":{"cgroup":group, "inode":fs::metadata(&directory).unwrap().ino(),
                "uid":1000, "pid":42, "start_ticks":7}
        }))
        .unwrap();
        // The old policy-based lookup fails after the domain disappears.
        assert!(identify_entry_at(&proc, &groups, 42, 1000, &[], true).is_err());
        let replay = identify_prepared_replay_at(&proc, &groups, 42, 1000, &grant).unwrap();
        assert_eq!(
            replay,
            (grant.domain.clone(), grant.identity.clone(), 40, 10)
        );
        assert!(identify_prepared_replay_at(&proc, &groups, 42, 1001, &grant).is_err());
        let mut pending = grant.clone();
        pending.granted = false;
        assert!(identify_prepared_replay_at(&proc, &groups, 42, 1000, &pending).is_err());
        let mut replaced = grant.clone();
        replaced.identity.inode += 1;
        assert!(identify_prepared_replay_at(&proc, &groups, 42, 1000, &replaced).is_err());
        for (name, changed, original) in
            [("memory.max", "39", "40"), ("memory.swap.max", "9", "10")]
        {
            fs::write(directory.join(name), changed).unwrap();
            assert!(identify_prepared_replay_at(&proc, &groups, 42, 1000, &grant).is_err());
            fs::write(directory.join(name), original).unwrap();
        }
        fs::write(proc.join("42/cgroup"), "0::/elsewhere.scope\n").unwrap();
        assert!(identify_prepared_replay_at(&proc, &groups, 42, 1000, &grant).is_err());
        fs::write(proc.join("42/cgroup"), format!("0::{group}\n")).unwrap();
        fs::write(
            proc.join("42/stat"),
            format!("42 (prepared) S {}8\n", "0 ".repeat(18)),
        )
        .unwrap();
        assert!(identify_prepared_replay_at(&proc, &groups, 42, 1000, &grant).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn burst_deadline_requires_native_identity_and_bounded_cleanup() {
        let identity = Identity {
            cgroup: "/burst/job.service".into(),
            inode: 1,
            uid: 1000,
            pid: 42,
            start_ticks: 1,
        };
        let valid = "ControlGroup=/burst/job.service\nMainPID=42\nRestart=no\nKillMode=control-group\nOOMPolicy=kill\nSendSIGKILL=yes\nFinalKillSignal=9\nRuntimeMaxUSec=5s\nRuntimeRandomizedExtraUSec=0\nTimeoutStopUSec=1s\n";
        assert_eq!(verified_burst_runtime(valid, &identity).unwrap(), 5000);
        for (before, after) in [
            ("MainPID=42", "MainPID=43"),
            ("Restart=no", "Restart=always"),
            ("RuntimeMaxUSec=5s", "RuntimeMaxUSec=infinity"),
            ("RuntimeMaxUSec=5s", "RuntimeMaxUSec=30s 1us"),
            (
                "RuntimeRandomizedExtraUSec=0",
                "RuntimeRandomizedExtraUSec=1s",
            ),
            ("TimeoutStopUSec=1s", "TimeoutStopUSec=15s"),
            ("TimeoutStopUSec=1s", "TimeoutStopUSec=0"),
            ("SendSIGKILL=yes", "SendSIGKILL=no"),
            ("FinalKillSignal=9", "FinalKillSignal=19"),
        ] {
            assert!(
                verified_burst_runtime(&valid.replace(before, after), &identity).is_err(),
                "{before}"
            );
        }
    }

    #[test]
    fn shared_ancestor_swap_cannot_be_spent_twice_with_ample_host_swap() {
        use crate::host::{HostLedger, HostPolicy, Reservation, WaitReason};
        let root = std::env::temp_dir().join(format!(
            "amc-ancestor-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(root.join("pool/first")).unwrap();
        fs::create_dir_all(root.join("pool/second")).unwrap();
        for (name, value) in [
            ("memory.max", "1000"),
            ("memory.current", "0"),
            ("memory.swap.max", "50"),
            ("memory.swap.current", "0"),
        ] {
            fs::write(root.join("pool").join(name), value).unwrap();
        }
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version": 1, "budget_bytes": 1000, "reserve_bytes": 20, "swap_reserve_bytes": 20,
            "max_memory_full_psi": 1.0, "max_io_full_psi": 20.0,
            "resume_ms": 250, "aging_ms": 1000, "queue_limit": 32,
            "domains": [{"name": "pool", "uid": 1000, "cgroup": "/pool", "ceiling_bytes": 1000,
                         "swap_bytes": 100, "fair_share_bytes": 1000}]
        }))
        .unwrap();
        policy.validate().unwrap();
        let mut ledger = HostLedger::new("boot".into());
        for name in ["first", "second"] {
            ledger
                .request(
                    Reservation {
                        id: name.into(),
                        domain: "pool".into(),
                        identity: Identity {
                            cgroup: format!("/pool/{name}"),
                            inode: fs::metadata(root.join("pool").join(name)).unwrap().ino(),
                            uid: 1000,
                            pid: 1,
                            start_ticks: 1,
                        },
                        memory_bytes: 10,
                        swap_bytes: 30,
                        requested_ms: 0,
                        deadline_ms: 10_000,
                        granted: false,
                        owners: vec![],
                        burst: false,
                        runtime_max_ms: None,
                        continuation: None,
                        owners_finished: false,
                    },
                    &policy,
                )
                .unwrap();
        }
        let mut since = std::collections::BTreeMap::from([("pool".into(), 0)]);
        let capacity = Some(Capacity {
            available_bytes: 1000,
            swap_free_bytes: 1000,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        });
        for memory_max in ["1000", "max"] {
            fs::write(root.join("pool/memory.max"), memory_max).unwrap();
            let mut observed = ledger.clone();
            let waits = observed.advance(250, &policy, capacity, &mut since, |r, entries| {
                ancestor_headroom_at(&root, r, entries)
            });
            assert_eq!(observed.committed(), 10);
            assert_eq!(waits["second"], WaitReason::AncestorHeadroom);
        }
        fs::write(root.join("pool/memory.swap.current"), "25").unwrap();
        let waits = ledger.advance(250, &policy, capacity, &mut since, |r, entries| {
            ancestor_headroom_at(&root, r, entries)
        });
        assert_eq!(ledger.committed(), 0);
        assert_eq!(waits["first"], WaitReason::AncestorHeadroom);
        fs::remove_file(root.join("pool/memory.swap.current")).unwrap();
        assert_eq!(
            ancestor_headroom_at(&root, &ledger.reservations[0], &[]),
            None
        );
        fs::write(root.join("pool/memory.swap.max"), "max").unwrap();
        ledger.advance(500, &policy, capacity, &mut since, |r, entries| {
            ancestor_headroom_at(&root, r, entries)
        });
        assert_eq!(ledger.committed(), 20);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn optional_io_failure_does_not_hide_required_memory_failure() {
        let path = std::env::temp_dir().join(format!(
            "amc-host-capacity-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir_all(path.join("pressure")).unwrap();
        fs::write(
            path.join("meminfo"),
            "MemAvailable: 1048576 kB\nSwapFree: 1048576 kB\n",
        )
        .unwrap();
        fs::write(path.join("pressure/memory"), "full avg10=0.00\n").unwrap();
        assert!(capacity_at(&path).unwrap().io_full_psi.is_nan());
        for malformed in ["some avg10=0.00\n", "full avg10=NaN\n", "full avg10=101\n"] {
            fs::write(path.join("pressure/io"), malformed).unwrap();
            assert!(capacity_at(&path).unwrap().io_full_psi.is_nan());
            fs::write(path.join("pressure/memory"), malformed).unwrap();
            assert!(capacity_at(&path).is_err());
            fs::write(path.join("pressure/memory"), "full avg10=0.00\n").unwrap();
        }
        fs::write(path.join("meminfo"), "SwapFree: 1048576 kB\n").unwrap();
        assert!(capacity_at(&path).is_err());
        fs::remove_dir_all(path).unwrap();
    }

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
            burst: false,
            runtime_max_ms: None,
            continuation: None,
            owners_finished: false,
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
        r.owners_finished = true;
        assert_eq!(empty_reservation(&r), Some(true));
    }
}
