//! Host-wide, ceiling-backed reservations. Native cgroup identity owns release.
//! The root broker accounts both users and execution-owner domains atomically.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostPolicy {
    pub version: u32,
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
    pub swap_reserve_bytes: u64,
    pub max_memory_full_psi: f64,
    pub max_io_full_psi: f64,
    pub resume_ms: u64,
    pub aging_ms: u64,
    pub queue_limit: usize,
    pub domains: Vec<Domain>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Domain {
    pub name: String,
    pub uid: u32,
    /// Absolute cgroup subtree. Requests cannot nominate arbitrary domains.
    pub cgroup: String,
    pub ceiling_bytes: u64,
    pub swap_bytes: u64,
    /// Scheduling weight expressed as a soft share, never idle capacity withheld.
    pub fair_share_bytes: u64,
}

impl HostPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.budget_bytes > 0 && self.reserve_bytes > 0,
            "invalid host budget"
        );
        ensure!(
            (1..=256).contains(&self.queue_limit) && (1..=64).contains(&self.domains.len()),
            "invalid host admission coverage"
        );
        ensure!(
            (250..=300_000).contains(&self.resume_ms)
                && (1000..=3_600_000).contains(&self.aging_ms),
            "invalid host scheduling hysteresis"
        );
        for psi in [self.max_memory_full_psi, self.max_io_full_psi] {
            ensure!(
                psi.is_finite() && psi > 0.0 && psi <= 100.0,
                "invalid host PSI percentage"
            );
        }
        let mut names = BTreeSet::new();
        for d in &self.domains {
            ensure!(
                crate::ledger::valid_name(&d.name) && names.insert(&d.name),
                "invalid or duplicate host domain"
            );
            crate::native::cgroup_directory(&d.cgroup)?;
            ensure!(
                d.ceiling_bytes > 0
                    && d.ceiling_bytes <= self.budget_bytes
                    && d.swap_bytes <= i64::MAX as u64
                    && d.fair_share_bytes > 0,
                "host domain cannot fit budget"
            );
            for other in &self.domains {
                ensure!(
                    d.name == other.name
                        || !(d.cgroup == other.cgroup
                            || d.cgroup.starts_with(&format!("{}/", other.cgroup))),
                    "overlapping host domains"
                );
                ensure!(
                    d.uid != other.uid || d.fair_share_bytes == other.fair_share_bytes,
                    "inconsistent participant soft share"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub cgroup: String,
    pub inode: u64,
    pub uid: u32,
    pub pid: i32,
    pub start_ticks: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub id: String,
    pub domain: String,
    pub identity: Identity,
    pub memory_bytes: u64,
    pub swap_bytes: u64,
    pub requested_ms: u64,
    pub deadline_ms: u64,
    pub granted: bool,
    /// Root execution owners may share one already-bounded aggregate pool.
    #[serde(default)]
    pub owners: Vec<crate::ledger::ClientIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostLedger {
    pub version: u32,
    pub boot_id: String,
    pub reservations: Vec<Reservation>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Pressure,
    Budget,
    HostHeadroom,
    SwapHeadroom,
    AncestorHeadroom,
    AgedRequest,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Capacity {
    pub available_bytes: u64,
    pub swap_free_bytes: u64,
    pub memory_full_psi: f64,
    pub io_full_psi: f64,
}

impl HostLedger {
    pub fn new(boot_id: String) -> Self {
        Self {
            version: 1,
            boot_id,
            reservations: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.reservations.len() <= 256,
            "invalid host ledger"
        );
        let mut ids = BTreeSet::new();
        let mut groups = BTreeSet::new();
        for r in &self.reservations {
            ensure!(
                crate::ledger::valid_name(&r.id)
                    && ids.insert(&r.id)
                    && groups.insert((&r.identity.cgroup, r.identity.inode)),
                "duplicate host reservation"
            );
            crate::native::cgroup_directory(&r.identity.cgroup)?;
            ensure!(
                crate::ledger::valid_name(&r.domain)
                    && r.memory_bytes > 0
                    && r.memory_bytes <= i64::MAX as u64
                    && r.swap_bytes <= i64::MAX as u64
                    && r.identity.inode > 0
                    && r.identity.pid > 0
                    && r.identity.start_ticks > 0
                    && r.deadline_ms >= r.requested_ms,
                "invalid host reservation"
            );
            ensure!(
                (1..=256).contains(&r.owners.len())
                    && r.owners.iter().all(|o| o.pid > 0 && o.start_ticks > 0),
                "invalid native execution owners"
            );
        }
        Ok(())
    }

    pub fn committed(&self) -> u64 {
        self.reservations
            .iter()
            .filter(|r| r.granted)
            .fold(0u64, |sum, r| sum.saturating_add(r.memory_bytes))
    }

    /// Idempotent retries bind to the native identity, not the socket lifetime.
    pub fn request(&mut self, reservation: Reservation, policy: &HostPolicy) -> Result<String> {
        let domain = policy
            .domains
            .iter()
            .find(|d| d.name == reservation.domain && d.uid == reservation.identity.uid)
            .ok_or_else(|| anyhow::anyhow!("unknown host domain"))?;
        ensure!(
            reservation.memory_bytes > 0
                && reservation.memory_bytes <= domain.ceiling_bytes
                && reservation.swap_bytes <= domain.swap_bytes
                && !reservation.granted,
            "invalid host reservation ceiling"
        );
        ensure!(
            crate::ledger::valid_name(&reservation.id)
                && reservation.identity.pid > 0
                && reservation.identity.start_ticks > 0
                && reservation.identity.inode > 0
                && reservation.deadline_ms >= reservation.requested_ms,
            "invalid native request identity"
        );
        ensure!(
            reservation.identity.cgroup == domain.cgroup
                || reservation
                    .identity
                    .cgroup
                    .starts_with(&format!("{}/", domain.cgroup)),
            "request outside enrolled subtree"
        );
        if let Some(r) = self.reservations.iter_mut().find(|r| {
            r.identity.cgroup == reservation.identity.cgroup
                && r.identity.inode == reservation.identity.inode
        }) {
            ensure!(
                (r.identity == reservation.identity
                    || (r.identity.uid == 0
                        && reservation.identity.uid == 0
                        && r.domain == reservation.domain))
                    && r.memory_bytes == reservation.memory_bytes
                    && r.swap_bytes == reservation.swap_bytes,
                "native identity or ceiling changed"
            );
            let owner = crate::ledger::ClientIdentity {
                pid: reservation.identity.pid,
                start_ticks: reservation.identity.start_ticks,
            };
            if r.identity.uid == 0 && !r.owners.contains(&owner) {
                ensure!(r.owners.len() < 256, "execution owner bound exceeded");
                r.owners.push(owner);
            }
            return Ok(r.id.clone());
        }
        ensure!(
            self.reservations.len() < policy.queue_limit
                && self.reservations.iter().all(|r| r.id != reservation.id),
            "host admission queue is full or ticket exists"
        );
        let id = reservation.id.clone();
        let mut reservation = reservation;
        reservation.owners = vec![crate::ledger::ClientIdentity {
            pid: reservation.identity.pid,
            start_ticks: reservation.identity.start_ticks,
        }];
        self.reservations.push(reservation);
        Ok(id)
    }

    /// Missing observations retain all grants and stop new admission. Even
    /// after a deadline, a granted domain remains charged until native cleanup.
    pub fn advance(
        &mut self,
        now: u64,
        policy: &HostPolicy,
        capacity: Option<Capacity>,
        healthy_since: &mut Option<u64>,
        mut ancestry: impl FnMut(&Reservation, &[Reservation]) -> Option<u64>,
    ) -> BTreeMap<String, WaitReason> {
        self.reservations
            .retain(|r| r.granted || now < r.deadline_ms);
        let healthy = capacity.is_some_and(|c| {
            c.available_bytes >= policy.reserve_bytes
                && c.swap_free_bytes >= policy.swap_reserve_bytes
                && c.memory_full_psi.is_finite()
                && c.io_full_psi.is_finite()
                && c.memory_full_psi < policy.max_memory_full_psi
                && c.io_full_psi < policy.max_io_full_psi
        });
        if !healthy {
            *healthy_since = None;
        } else {
            healthy_since.get_or_insert(now);
        }
        let ready = healthy
            && healthy_since.is_some_and(|since| now.saturating_sub(since) >= policy.resume_ms);
        let mut waiting = BTreeMap::new();
        let mut aged_block = false;
        let mut pending: Vec<_> = (0..self.reservations.len())
            .filter(|i| !self.reservations[*i].granted)
            .collect();
        while !pending.is_empty() {
            // An aged request takes priority. Otherwise choose the participant
            // furthest below its soft share, recalculating after every grant.
            pending.sort_by(|a, b| {
                let score = |i: usize| {
                    let r = &self.reservations[i];
                    let aged = now.saturating_sub(r.requested_ms) >= policy.aging_ms;
                    let share = policy
                        .domains
                        .iter()
                        .find(|d| d.name == r.domain && d.uid == r.identity.uid)
                        .map_or(1, |d| d.fair_share_bytes);
                    let used = self
                        .reservations
                        .iter()
                        .filter(|p| p.granted && p.identity.uid == r.identity.uid)
                        .fold(0u64, |sum, p| sum.saturating_add(p.memory_bytes));
                    (aged, used, share, r.requested_ms, i)
                };
                let (aa, au, as_, at, ai) = score(*a);
                let (ba, bu, bs, bt, bi) = score(*b);
                ba.cmp(&aa)
                    .then_with(|| {
                        if aa {
                            at.cmp(&bt)
                        } else {
                            (u128::from(au) * u128::from(bs))
                                .cmp(&(u128::from(bu) * u128::from(as_)))
                        }
                    })
                    .then(at.cmp(&bt))
                    .then(ai.cmp(&bi))
            });
            let i = pending.remove(0);
            let r = &self.reservations[i];
            if r.granted {
                continue;
            }
            let committed = self.committed();
            let reason = if capacity.is_none() {
                Some(WaitReason::Unknown)
            } else if !ready {
                Some(WaitReason::Pressure)
            } else if aged_block {
                Some(WaitReason::AgedRequest)
            } else if r.memory_bytes > policy.budget_bytes.saturating_sub(committed) {
                Some(WaitReason::Budget)
            } else if r.memory_bytes
                > capacity
                    .map_or(0, |c| {
                        c.available_bytes.saturating_sub(policy.reserve_bytes)
                    })
                    .saturating_sub(committed)
            {
                Some(WaitReason::HostHeadroom)
            } else if r.swap_bytes
                > capacity
                    .map_or(0, |c| {
                        c.swap_free_bytes.saturating_sub(policy.swap_reserve_bytes)
                    })
                    .saturating_sub(
                        self.reservations
                            .iter()
                            .filter(|r| r.granted)
                            .fold(0u64, |sum, r| sum.saturating_add(r.swap_bytes)),
                    )
            {
                Some(WaitReason::SwapHeadroom)
            } else {
                // Full ceilings remain counted over live usage in v1. Credit
                // requires boundary-specific proof, not an RSS approximation.
                match ancestry(r, &self.reservations) {
                    None => Some(WaitReason::Unknown),
                    Some(bytes) if r.memory_bytes > bytes => Some(WaitReason::AncestorHeadroom),
                    Some(_) => None,
                }
            };
            if let Some(reason) = reason {
                aged_block |= now.saturating_sub(r.requested_ms) >= policy.aging_ms;
                waiting.insert(r.id.clone(), reason);
            } else {
                self.reservations[i].granted = true;
            }
        }
        waiting
    }

    pub fn reconcile(&mut self, mut empty: impl FnMut(&Reservation) -> Option<bool>) {
        self.reservations.retain(|r| empty(r) != Some(true));
    }
}
