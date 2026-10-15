//! Fixed, policy-owned backing for bounded short calls, not burst capacity.
use crate::host::{HostLedger, HostPolicy, Identity, Reservation, WaitReason};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtectedPool {
    /// Ordinary contract whose explicitly bounded calls may use this pool.
    pub source_contract: String,
    pub memory_bytes: u64,
    pub max_job_bytes: u64,
    pub max_running: usize,
    pub runtime_max_sec: u64,
    pub queue_limit: usize,
    pub wait_max_sec: u64,
    /// CPU time per 100 ms period (100_000 means one CPU).
    pub cpu_quota_usec: u64,
    pub tasks_max: u64,
}

impl ProtectedPool {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            crate::ledger::valid_name(&self.source_contract)
                && self.memory_bytes <= i64::MAX as u64
                && self.max_job_bytes > 0
                && (1..=8).contains(&self.max_running)
                && self
                    .max_job_bytes
                    .checked_mul(self.max_running as u64)
                    .is_some_and(|bytes| bytes < self.memory_bytes)
                && (1..=30).contains(&self.runtime_max_sec)
                && (1..=32).contains(&self.queue_limit)
                && (1..=30).contains(&self.wait_max_sec)
                && (1..=100_000).contains(&self.cpu_quota_usec)
                && (1..=256).contains(&self.tasks_max),
            "invalid protected pool bounds or missing helper headroom"
        );
        Ok(())
    }
}

/// Policy-owned aggregate backing. Unlike a workload, idle pools stay charged.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Backing {
    pub uid: u32,
    pub cgroup: String,
    pub memory_bytes: u64,
}

impl Backing {
    pub fn contains(&self, r: &Reservation) -> bool {
        r.identity.uid == self.uid && r.identity.cgroup.starts_with(&format!("{}/", self.cgroup))
    }

    fn claim(&self, name: &str) -> Reservation {
        Reservation {
            id: format!("protected-{name}"),
            domain: name.into(),
            identity: Identity {
                cgroup: self.cgroup.clone(),
                uid: self.uid,
                inode: 0,
                pid: 0,
                start_ticks: 0,
            },
            memory_bytes: self.memory_bytes,
            swap_bytes: 0,
            requested_ms: 0,
            deadline_ms: u64::MAX,
            granted: true,
            owners: vec![],
            owners_finished: false,
            burst: false,
            runtime_max_ms: None,
            continuation: None,
        }
    }
}

pub(crate) fn validate_backing(ledger: &HostLedger) -> Result<()> {
    ensure!(
        ledger.protected_pools.len() <= 64,
        "too many protected pool backings"
    );
    for (name, p) in &ledger.protected_pools {
        ensure!(
            crate::ledger::valid_name(name)
                && p.uid != 0
                && p.memory_bytes > 0
                && p.memory_bytes <= i64::MAX as u64,
            "invalid protected pool backing"
        );
        crate::native::cgroup_directory(&p.cgroup)?;
        for (other_name, other) in &ledger.protected_pools {
            ensure!(
                name == other_name
                    || !(p.cgroup == other.cgroup
                        || p.cgroup.starts_with(&format!("{}/", other.cgroup))),
                "overlapping protected backings"
            );
        }
    }
    Ok(())
}

impl HostLedger {
    pub fn protected_owner(&self, r: &Reservation) -> Option<&Backing> {
        self.protected_pools
            .get(&r.domain)
            .filter(|p| p.contains(r))
    }

    /// Reductions/removal retain old capacity until positive native cleanup.
    /// Current enrollment always restores backing, even when positively idle.
    pub fn sync_protected_pools(
        &mut self,
        policy: &HostPolicy,
        mut empty: impl FnMut(&Backing) -> Option<bool>,
    ) -> Result<()> {
        self.protected_pools.retain(|name, backing| {
            let unchanged = policy.domains.iter().any(|d| {
                d.name == *name
                    && d.uid == backing.uid
                    && d.cgroup == backing.cgroup
                    && d.protected_pool
                        .as_ref()
                        .is_some_and(|p| p.memory_bytes >= backing.memory_bytes)
            });
            unchanged
                || self.reservations.iter().any(|r| backing.contains(r))
                || empty(backing) != Some(true)
        });
        for d in &policy.domains {
            let Some(p) = &d.protected_pool else { continue };
            let backing = self
                .protected_pools
                .entry(d.name.clone())
                .or_insert_with(|| Backing {
                    uid: d.uid,
                    cgroup: d.cgroup.clone(),
                    memory_bytes: p.memory_bytes,
                });
            ensure!(
                backing.uid == d.uid && backing.cgroup == d.cgroup,
                "protected pool identity changed before native cleanup"
            );
            backing.memory_bytes = backing.memory_bytes.max(p.memory_bytes);
        }
        validate_backing(self)
    }

    pub(crate) fn protected_claims(&self) -> Vec<Reservation> {
        self.protected_pools
            .iter()
            .map(|(name, p)| p.claim(name))
            .collect()
    }

    pub(crate) fn protected_wait(
        &self,
        r: &Reservation,
        policy: &HostPolicy,
        runtime: Option<u64>,
        ancestry: &mut impl FnMut(&Reservation, &[Reservation]) -> Option<u64>,
    ) -> Option<WaitReason> {
        let d = policy.domains.iter().find(|d| d.name == r.domain)?;
        let p = d.protected_pool.as_ref()?;
        let Some(backing) = self.protected_owner(r) else {
            return Some(WaitReason::ProtectedPool);
        };
        if backing.memory_bytes != p.memory_bytes {
            return Some(WaitReason::ProtectedPool);
        }
        if self.committed().saturating_sub(self.burst_committed()) > policy.budget_bytes {
            return Some(WaitReason::Budget);
        }
        if self
            .reservations
            .iter()
            .filter(|other| other.granted && backing.contains(other))
            .count()
            >= p.max_running
        {
            return Some(WaitReason::ProtectedConcurrency);
        }
        if runtime != r.runtime_max_ms
            || !runtime.is_some_and(|ms| ms > 0 && ms <= p.runtime_max_sec * 1000)
        {
            return Some(WaitReason::Unknown);
        }
        let Some(claims) = self.native_claims(policy, None) else {
            return Some(WaitReason::Unknown);
        };
        let peers: Vec<_> = claims
            .into_iter()
            .filter(|c| c.id != format!("protected-{}", r.domain))
            .collect();
        match ancestry(&backing.claim(&r.domain), &peers) {
            Some(bytes) if bytes >= backing.memory_bytes => None,
            Some(_) => Some(WaitReason::AncestorHeadroom),
            None => Some(WaitReason::Unknown),
        }
    }
}

pub(crate) fn empty(backing: &Backing) -> Option<bool> {
    let directory = crate::native::cgroup_directory(&backing.cgroup).ok()?;
    empty_directory(&directory)
}

pub(crate) fn empty_directory(directory: &Path) -> Option<bool> {
    match fs::metadata(directory) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(true),
        Err(_) => return None,
        Ok(_) => (),
    }
    match fs::read_to_string(directory.join("cgroup.events")) {
        Ok(text) if text.lines().any(|line| line == "populated 0") => Some(
            fs::read_to_string(directory.join("memory.current"))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()?
                == 0,
        ),
        Ok(text) if text.lines().any(|line| line == "populated 1") => Some(false),
        _ => None,
    }
}

/// Check the actual aggregate controller limits, not a client's declaration.
pub(crate) fn bounded(directory: &Path, pool: &ProtectedPool) -> Option<bool> {
    let number = |name: &str| {
        fs::read_to_string(directory.join(name))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    };
    let cpu = fs::read_to_string(directory.join("cpu.max")).ok()?;
    let mut fields = cpu.split_whitespace();
    let quota: u64 = fields.next()?.parse().ok()?;
    let period: u64 = fields.next()?.parse().ok()?;
    Some(
        number("memory.max")? == pool.memory_bytes
            && number("memory.swap.max")? == 0
            && number("pids.max")? <= pool.tasks_max
            && period > 0
            && quota > 0
            && u128::from(quota) * 100_000 <= u128::from(pool.cpu_quota_usec) * u128::from(period)
            && fields.next().is_none(),
    )
}

pub(crate) fn enforcement(ledger: &HostLedger, policy: &HostPolicy) -> Option<bool> {
    for (name, backing) in &ledger.protected_pools {
        let directory = crate::native::cgroup_directory(&backing.cgroup).ok()?;
        // An offline user's absent native slice cannot execute children. Keep
        // its policy commitment, but do not block other enrolled users' work.
        match fs::metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
            Ok(_) => (),
        }
        if let Some(pool) = policy
            .domains
            .iter()
            .find(|d| d.name == *name && d.uid == backing.uid && d.cgroup == backing.cgroup)
            .and_then(|d| d.protected_pool.as_ref())
        {
            if bounded(&directory, pool) != Some(true) {
                return Some(false);
            }
        } else if empty(backing) != Some(true) {
            // Removed pools cannot acquire new children; their old boundary must
            // remain finite while accounting is retained.
            let bytes: u64 = fs::read_to_string(directory.join("memory.max"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            if bytes > backing.memory_bytes {
                return Some(false);
            }
        }
    }
    Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_pool_needs_every_controller_and_cache_blocks_retirement() {
        let directory = std::env::temp_dir().join(format!(
            "amc-protected-native-{}",
            crate::store::fresh_id().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        let pool = ProtectedPool {
            source_contract: "tool".into(),
            memory_bytes: 30,
            max_job_bytes: 10,
            max_running: 2,
            runtime_max_sec: 15,
            wait_max_sec: 15,
            queue_limit: 4,
            cpu_quota_usec: 50000,
            tasks_max: 32,
        };
        for (file, value) in [
            ("memory.max", "30"),
            ("memory.swap.max", "0"),
            ("pids.max", "32"),
            ("cpu.max", "50000 100000"),
            ("memory.current", "1"),
            ("cgroup.events", "populated 0\n"),
        ] {
            fs::write(directory.join(file), value).unwrap();
        }
        assert_eq!(bounded(&directory, &pool), Some(true));
        assert_eq!(
            empty_directory(&directory),
            Some(false),
            "idle cache is still charged"
        );
        for (file, invalid, valid) in [
            ("memory.max", "max", "30"),
            ("memory.swap.max", "1", "0"),
            ("pids.max", "33", "32"),
            ("cpu.max", "max 100000", "50000 100000"),
            ("cpu.max", "50001 100000", "50000 100000"),
        ] {
            fs::write(directory.join(file), invalid).unwrap();
            assert_ne!(bounded(&directory, &pool), Some(true), "{file}");
            fs::write(directory.join(file), valid).unwrap();
            fs::remove_file(directory.join(file)).unwrap();
            assert_eq!(bounded(&directory, &pool), None, "missing {file}");
            fs::write(directory.join(file), valid).unwrap();
        }
        fs::write(directory.join("memory.current"), "0").unwrap();
        assert_eq!(empty_directory(&directory), Some(true));
        fs::write(directory.join("cgroup.events"), "populated 1\n").unwrap();
        assert_eq!(empty_directory(&directory), Some(false));
        fs::write(directory.join("cgroup.events"), "unreadable\n").unwrap();
        assert_eq!(empty_directory(&directory), None);
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(empty_directory(&directory), Some(true));
    }
}
