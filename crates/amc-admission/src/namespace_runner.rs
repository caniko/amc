//! A static host-reserve-backed envelope bounds ALL waited namespace runners
//! and preparation helpers for one user. It is not a drainable job.
use crate::host::{HostLedger, HostPolicy, Identity, Reservation};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

pub const SERVICE_BYTES: u64 = 64 * 1024 * 1024;

fn users(policy: &HostPolicy) -> BTreeSet<u32> {
    policy
        .domains
        .iter()
        .filter(|d| d.uid != 0)
        .map(|d| d.uid)
        .collect()
}

pub fn cgroup(uid: u32) -> String {
    format!("/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/app-amchostrunner.slice")
}

pub fn validate(policy: &HostPolicy) -> Result<()> {
    let bytes = policy.namespace_runner_bytes;
    ensure!(
        policy.preparations.is_empty() || bytes >= SERVICE_BYTES,
        "preparation profiles require a backed aggregate helper allowance"
    );
    ensure!(
        bytes == 0
            || ((SERVICE_BYTES..=1024 * 1024 * 1024).contains(&bytes)
                && bytes
                    .checked_mul(users(policy).len() as u64)
                    .is_some_and(|sum| sum <= policy.reserve_bytes)),
        "aggregate namespace runners must fit the host reserve for every enrolled user"
    );
    Ok(())
}

pub fn claims(policy: &HostPolicy) -> Vec<Reservation> {
    projected_claims(&allowances(&BTreeMap::new(), policy))
}

fn allowances(retained: &BTreeMap<u32, u64>, policy: &HostPolicy) -> BTreeMap<u32, u64> {
    let mut allowances = retained.clone();
    if policy.namespace_runner_bytes > 0 {
        for uid in users(policy) {
            let bytes = allowances.entry(uid).or_default();
            *bytes = (*bytes).max(policy.namespace_runner_bytes);
        }
    }
    allowances
}

pub(crate) fn retained_claims(ledger: &HostLedger, policy: &HostPolicy) -> Vec<Reservation> {
    projected_claims(&allowances(&ledger.namespace_runners, policy))
}

fn projected_claims(allowances: &BTreeMap<u32, u64>) -> Vec<Reservation> {
    allowances
        .iter()
        .map(|(&uid, &bytes)| {
            let mut claim = crate::recovery::helper_claim(
                &Identity {
                    cgroup: cgroup(uid),
                    uid,
                    inode: 0,
                    pid: 0,
                    start_ticks: 0,
                },
                bytes,
            );
            claim.id = format!("namespace-runners-{uid}");
            claim.domain = claim.id.clone();
            claim
        })
        .collect()
}

fn aggregate_empty(uid: u32) -> Option<bool> {
    let directory = crate::native::cgroup_directory(&cgroup(uid)).ok()?;
    match fs::metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(true),
        Err(_) => None,
        Ok(_) => {
            let events = fs::read_to_string(directory.join("cgroup.events")).ok()?;
            if events.lines().any(|line| line == "populated 0") {
                Some(true)
            } else if events.lines().any(|line| line == "populated 1") {
                Some(false)
            } else {
                None
            }
        }
    }
}

pub(crate) fn reconcile(ledger: &mut HostLedger, policy: &HostPolicy) {
    reconcile_with(ledger, policy, aggregate_empty);
}

fn reconcile_with(
    ledger: &mut HostLedger,
    policy: &HostPolicy,
    mut empty: impl FnMut(u32) -> Option<bool>,
) {
    // Only positive cleanup can release an old aggregate. Current enrollment
    // is then backed again before any NamespaceRunner handshake or new grant.
    ledger
        .namespace_runners
        .retain(|uid, _| empty(*uid) != Some(true));
    ledger.namespace_runners = allowances(&ledger.namespace_runners, policy);
}

/// Upgrade from older snapshots also discovers still-populated aggregate slices
/// whose users disappeared from the new policy. This runs once before listening.
pub(crate) fn discover(ledger: &mut HostLedger) -> Result<()> {
    discover_at(ledger, Path::new("/sys/fs/cgroup/user.slice"))
}

fn discover_at(ledger: &mut HostLedger, root: &Path) -> Result<()> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    for (index, entry) in entries.enumerate() {
        ensure!(
            index < 256 && std::time::Instant::now() < deadline,
            "runner discovery exceeds bound"
        );
        let entry = entry?;
        let name = entry.file_name();
        let Some(uid) = name
            .to_str()
            .and_then(|s| {
                s.strip_prefix("user-")?
                    .strip_suffix(".slice")?
                    .parse::<u32>()
                    .ok()
            })
            .filter(|uid| *uid != 0)
        else {
            continue;
        };
        let directory = entry.path().join(format!(
            "user@{uid}.service/app.slice/app-amchostrunner.slice"
        ));
        let events = match fs::read_to_string(directory.join("cgroup.events")) {
            Ok(events) => events,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !directory.exists() => {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if events.lines().any(|line| line == "populated 0") {
            continue;
        }
        let bytes: u64 = fs::read_to_string(directory.join("memory.max"))?
            .trim()
            .parse()?;
        ensure!(
            (SERVICE_BYTES..=1024 * 1024 * 1024).contains(&bytes),
            "unbounded surviving runner aggregate"
        );
        let allowance = ledger.namespace_runners.entry(uid).or_default();
        *allowance = (*allowance).max(bytes);
    }
    Ok(())
}

fn bounded(directory: &Path, bytes: u64) -> Result<bool> {
    Ok(fs::read_to_string(directory.join("memory.max"))?
        .trim()
        .parse::<u64>()?
        == bytes
        && fs::read_to_string(directory.join("memory.swap.max"))?.trim() == "0")
}

pub fn enforcement(ledger: &HostLedger, policy: &HostPolicy) -> Option<bool> {
    let retained = allowances(&ledger.namespace_runners, policy);
    if retained
        .values()
        .try_fold(0u64, |sum, bytes| sum.checked_add(*bytes))?
        > policy.reserve_bytes
    {
        return Some(false);
    }
    for (uid, bytes) in retained {
        let directory = crate::native::cgroup_directory(&cgroup(uid)).ok()?;
        match fs::metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
            Ok(_)
                if fs::read_to_string(directory.join("memory.max"))
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()?
                    > bytes
                    || fs::read_to_string(directory.join("memory.swap.max"))
                        .ok()?
                        .trim()
                        != "0" =>
            {
                return Some(false);
            }
            Ok(_) => (),
        }
    }
    Some(true)
}

/// Kernel-authenticated runner peers cannot execute an inner shared job without
/// the exact aggregate native cap and its up-front host/native-ancestor backing.
pub fn verify(pid: i32, uid: u32, policy: &HostPolicy, claims: &[Reservation]) -> Result<()> {
    ensure!(
        policy.namespace_runner_bytes >= SERVICE_BYTES && users(policy).contains(&uid),
        "shared namespace execution needs a host-reserve-backed runner allowance"
    );
    let group = cgroup(uid);
    let placement = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let placement = placement
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| anyhow::anyhow!("missing namespace runner placement"))?;
    let leaf = placement.strip_prefix(&format!("{group}/")).unwrap_or("");
    ensure!(
        (leaf.starts_with("app-amc-host-runner-") || leaf.starts_with("app-amc-prepare-helper-"))
            && leaf.ends_with(".service")
            && !leaf.contains('/'),
        "peer is outside its aggregate namespace runner boundary"
    );
    ensure!(
        bounded(
            &crate::native::cgroup_directory(&group)?,
            policy.namespace_runner_bytes
        )? && bounded(&crate::native::cgroup_directory(placement)?, SERVICE_BYTES)?,
        "namespace runner native limits differ from the reserved allowance"
    );
    let index = claims
        .iter()
        .position(|r| {
            r.domain == format!("namespace-runners-{uid}")
                && r.identity.pid == 0
                && r.identity.cgroup == group
        })
        .ok_or_else(|| anyhow::anyhow!("namespace runner native backing missing"))?;
    let peers: Vec<_> = claims
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, r)| r.clone())
        .collect();
    ensure!(
        crate::host_native::ancestor_headroom(&claims[index], &peers)
            .is_some_and(|bytes| bytes >= policy.namespace_runner_bytes),
        "namespace runner native ancestor backing unavailable"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_users_keep_their_runner_allowance_across_restart_until_positive_cleanup() {
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":134217728,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "namespace_runner_bytes":67108864,
            "domains":[{"name":"bob","uid":1001,"cgroup":"/bob","ceiling_bytes":100,"swap_bytes":0,"fair_share_bytes":100}]
        })).unwrap();
        let mut saved = serde_json::to_value(crate::host::HostLedger::new("boot".into())).unwrap();
        saved["namespace_runners"] = serde_json::json!({"1000":67108864});
        let mut restored: crate::host::HostLedger = serde_json::from_value(saved).unwrap();
        restored.validate().unwrap();
        let claims = restored.native_claims(&policy, None).unwrap();
        assert_eq!(claims.len(), 2);
        assert!(
            claims
                .iter()
                .any(|r| r.identity.uid == 1000 && r.memory_bytes == SERVICE_BYTES)
        );
        for evidence in [None, Some(false)] {
            reconcile_with(&mut restored, &policy, |_| evidence);
            assert_eq!(restored.native_claims(&policy, None).unwrap().len(), 2);
        }
        let mut tight = policy.clone();
        tight.reserve_bytes = SERVICE_BYTES;
        assert_eq!(enforcement(&restored, &tight), Some(false));
        reconcile_with(&mut restored, &policy, |uid| {
            if uid == 1000 { Some(true) } else { None }
        });
        assert_eq!(restored.native_claims(&policy, None).unwrap().len(), 1);
        let mut disabled = policy.clone();
        disabled.namespace_runner_bytes = 0;
        reconcile_with(&mut restored, &disabled, |_| None);
        assert_eq!(restored.native_claims(&disabled, None).unwrap().len(), 1);
        reconcile_with(&mut restored, &disabled, |_| Some(true));
        assert!(restored.native_claims(&disabled, None).unwrap().is_empty());
    }

    #[test]
    fn legacy_snapshot_upgrade_discovers_removed_users_native_runner_ceilings() {
        let root = std::env::temp_dir().join(format!(
            "amc-runner-upgrade-{}",
            crate::store::fresh_id().unwrap()
        ));
        let directory =
            root.join("user-1000.slice/user@1000.service/app.slice/app-amchostrunner.slice");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("cgroup.events"), "populated 1\n").unwrap();
        fs::write(directory.join("memory.max"), SERVICE_BYTES.to_string()).unwrap();
        let mut ledger = HostLedger::new("boot".into());
        discover_at(&mut ledger, &root).unwrap();
        assert_eq!(ledger.namespace_runners.get(&1000), Some(&SERVICE_BYTES));
        ledger.validate().unwrap();
        fs::write(directory.join("memory.max"), "max").unwrap();
        assert!(discover_at(&mut ledger, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_profiles_require_a_backed_helper_allowance() {
        let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":67108864,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "domains":[{"name":"game","uid":1000,"cgroup":"/user.slice/user-1000.slice/user@1000.service/game.slice","ceiling_bytes":100,"swap_bytes":0,"fair_share_bytes":100}],
            "preparations":[{"name":"game","domain":"game","memory_bytes":100,"swap_bytes":0,
                "drain_domains":[],"wait_ms":10000,"ready_ms":15000}]
        })).unwrap();
        assert!(
            policy.validate().is_err(),
            "preparation without helper backing must fail before the broker starts"
        );
        policy.namespace_runner_bytes = SERVICE_BYTES;
        policy.validate().unwrap();
        policy.reserve_bytes -= 1;
        assert!(policy.validate().is_err());
        policy.preparations.clear();
        policy.namespace_runner_bytes = 0;
        policy.validate().unwrap();
    }

    #[test]
    fn aggregate_runners_are_reserved_once_per_user_and_cannot_outgrow_host_reserve() {
        let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":134217728,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "namespace_runner_bytes":67108864,
            "domains":[{"name":"alice","uid":1000,"cgroup":"/alice","ceiling_bytes":100,"swap_bytes":0,"fair_share_bytes":100},
                {"name":"alice-other","uid":1000,"cgroup":"/alice-other","ceiling_bytes":100,"swap_bytes":0,"fair_share_bytes":100},
                {"name":"bob","uid":1001,"cgroup":"/bob","ceiling_bytes":100,"swap_bytes":0,"fair_share_bytes":100}]
        })).unwrap();
        policy.validate().unwrap();
        let ledger = crate::host::HostLedger::new("boot".into());
        let claims = ledger.native_claims(&policy, None).unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(
            claims.iter().map(|r| r.memory_bytes).sum::<u64>(),
            policy.reserve_bytes
        );
        assert!(claims.iter().all(|r| r.granted && r.swap_bytes == 0));
        policy.reserve_bytes -= 1;
        assert!(policy.validate().is_err());
        policy.reserve_bytes += 1;
        policy.namespace_runner_bytes = SERVICE_BYTES - 1;
        assert!(policy.validate().is_err());
        policy.namespace_runner_bytes = 0;
        policy.validate().unwrap();
        assert!(ledger.native_claims(&policy, None).unwrap().is_empty());
    }
}
