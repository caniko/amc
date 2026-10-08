//! A static host-reserve-backed envelope bounds ALL waited namespace runners
//! and preparation helpers for one user. It is not a drainable job.
use crate::host::{HostPolicy, Identity, Reservation};
use anyhow::{Result, ensure};
use std::{collections::BTreeSet, fs, path::Path};

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
    if policy.namespace_runner_bytes == 0 {
        return vec![];
    }
    users(policy)
        .into_iter()
        .map(|uid| {
            let mut claim = crate::recovery::helper_claim(
                &Identity {
                    cgroup: cgroup(uid),
                    uid,
                    inode: 0,
                    pid: 0,
                    start_ticks: 0,
                },
                policy.namespace_runner_bytes,
            );
            claim.id = format!("namespace-runners-{uid}");
            claim.domain = claim.id.clone();
            claim
        })
        .collect()
}

fn bounded(directory: &Path, bytes: u64) -> Result<bool> {
    Ok(fs::read_to_string(directory.join("memory.max"))?
        .trim()
        .parse::<u64>()?
        == bytes
        && fs::read_to_string(directory.join("memory.swap.max"))?.trim() == "0")
}

pub fn enforcement(policy: &HostPolicy) -> Option<bool> {
    for uid in users(policy) {
        let directory = crate::native::cgroup_directory(&cgroup(uid)).ok()?;
        match fs::metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
            Ok(_) if policy.namespace_runner_bytes == 0 => {
                if !fs::read_to_string(directory.join("cgroup.events"))
                    .ok()?
                    .lines()
                    .any(|line| line == "populated 0")
                {
                    return Some(false);
                }
            }
            Ok(_) if !bounded(&directory, policy.namespace_runner_bytes).ok()? => {
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
    fn preparation_profiles_require_a_backed_helper_allowance() {
        let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":67108864,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "domains":[{"name":"game","uid":1000,"cgroup":"/game.slice","ceiling_bytes":100,"swap_bytes":0,"fair_share_bytes":100}],
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
