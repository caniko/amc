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
    /// Optional, separately bounded short-call allowance above the normal budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preparations: Vec<crate::preparation::PreparationProfile>,
    #[serde(default, skip_serializing_if = "crate::ledger::is_false")]
    pub reserve_swap_return: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_recovery: Option<crate::recovery::RecoveryPolicy>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BurstPolicy {
    pub budget_bytes: u64,
    pub max_job_bytes: u64,
    pub max_running: usize,
    pub max_runtime_ms: u64,
    /// Per-UID start interval; survives broker restart and completed jobs.
    pub min_interval_ms: u64,
}

impl BurstPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.budget_bytes > 0
                && self.budget_bytes <= i64::MAX as u64
                && self.max_job_bytes > 0
                && self.max_job_bytes <= self.budget_bytes
                && (1..=8).contains(&self.max_running)
                && (1000..=30_000).contains(&self.max_runtime_ms)
                && (self.max_runtime_ms + 1000..=3_600_000).contains(&self.min_interval_ms),
            "invalid short-call burst policy"
        );
        Ok(())
    }
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
    /// Host I/O PSI is optional diagnostic telemetry only for explicitly
    /// selected domains. Existing policies retain enforced I/O admission.
    #[serde(default)]
    pub io_pressure: IoPressure,
    /// Additional host RAM floor; ceiling-backed reservations still apply.
    #[serde(default)]
    pub min_available_bytes: u64,
    /// Only this enrolled subtree can use the burst allowance.
    #[serde(default, skip_serializing_if = "crate::ledger::is_false")]
    pub burst: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<crate::continuation::ContinuationPolicy>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IoPressure {
    #[default]
    Enforce,
    Diagnostic,
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
        if let Some(burst) = &self.burst {
            burst.validate()?;
            ensure!(
                self.budget_bytes
                    .checked_add(burst.budget_bytes)
                    .is_some_and(|total| total <= i64::MAX as u64),
                "host burst budget overflow"
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
                    && d.min_available_bytes <= i64::MAX as u64
                    && d.fair_share_bytes > 0,
                "host domain cannot fit budget"
            );
            if d.burst {
                let burst = self
                    .burst
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("burst domain needs burst policy"))?;
                ensure!(
                    d.uid != 0 && d.swap_bytes == 0 && d.ceiling_bytes <= burst.max_job_bytes,
                    "burst domain must be a small zero-swap user execution domain"
                );
            }
            if let Some(continuation) = &d.continuation {
                ensure!(
                    d.uid != 0 && !d.burst,
                    "only non-burst user operations can own completion rights"
                );
                continuation.validate(self)?;
                ensure!(
                    continuation.domains.iter().all(|name| {
                        self.domains.iter().any(|child| {
                            &child.name == name && (child.uid == 0 || child.uid == d.uid)
                        })
                    }),
                    "completion rights cross user ownership"
                );
            }
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
        let mut profiles = BTreeSet::new();
        ensure!(
            self.preparations.len() <= 64,
            "too many preparation profiles"
        );
        for profile in &self.preparations {
            profile.validate(self)?;
            let uid = self
                .domains
                .iter()
                .find(|d| d.name == profile.domain)
                .map(|d| d.uid);
            ensure!(
                profiles.insert((uid, &profile.name)),
                "duplicate preparation profile for user"
            );
        }
        if let Some(recovery) = &self.swap_recovery {
            recovery.validate()?;
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
    /// Positive evidence that every root worker finished or exited. An empty
    /// legacy owner list alone is unknown, never authority to release a pool.
    #[serde(default, skip_serializing_if = "crate::ledger::is_false")]
    pub owners_finished: bool,
    #[serde(default, skip_serializing_if = "crate::ledger::is_false")]
    pub burst: bool,
    /// Native manager-observed deadline, never a client's duration prediction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_max_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostLedger {
    pub version: u32,
    pub boot_id: String,
    pub reservations: Vec<Reservation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub burst_last_granted: BTreeMap<u32, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preparations: Vec<crate::preparation::Preparation>,
    /// Observed uncovered return demand. Invalidated on broker restart.
    #[serde(default)]
    pub swap_return_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<crate::recovery::RecoveryLease>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuations: Vec<crate::continuation::Continuation>,
    /// Root pool ownership is bound to one finite native Nix worker operation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pool_operations: BTreeMap<String, u64>,
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
    BurstPolicy,
    BurstBudget,
    BurstConcurrency,
    BurstRate,
    Preparation,
    Draining,
    SwapReturn,
    Recovery,
    ContinuationSlot,
}

#[derive(Clone, Copy, Debug)]
pub struct Capacity {
    pub available_bytes: u64,
    pub swap_free_bytes: u64,
    pub memory_full_psi: f64,
    /// NaN denotes unavailable optional telemetry. Enforced domains fail closed.
    pub io_full_psi: f64,
}

impl HostLedger {
    pub fn new(boot_id: String) -> Self {
        Self {
            version: 1,
            boot_id,
            reservations: Vec::new(),
            burst_last_granted: BTreeMap::new(),
            preparations: Vec::new(),
            swap_return_bytes: None,
            recovery: None,
            continuations: vec![],
            pool_operations: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.reservations.len() <= 256
                && self.burst_last_granted.len() <= 256,
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
                if r.burst {
                    r.identity.uid != 0
                        && r.swap_bytes == 0
                        && r.runtime_max_ms
                            .is_some_and(|ms| (1..=30_000).contains(&ms))
                } else {
                    r.runtime_max_ms.is_none()
                },
                "invalid burst reservation proof"
            );
            ensure!(
                (r.owners.len() <= 256 && (r.identity.uid == 0 || !r.owners.is_empty()))
                    && r.owners.iter().all(|o| o.pid > 0 && o.start_ticks > 0),
                "invalid native execution owners"
            );
        }
        ensure!(
            self.preparations.len() + self.reservations.len() <= 512,
            "host preparation ledger exceeds bound"
        );
        for p in &self.preparations {
            ensure!(
                crate::ledger::valid_name(&p.id)
                    && crate::ledger::valid_name(&p.key)
                    && crate::ledger::valid_name(&p.profile)
                    && crate::ledger::valid_name(&p.domain)
                    && p.uid != 0
                    && p.memory_bytes > 0
                    && p.memory_bytes <= i64::MAX as u64
                    && p.swap_bytes <= i64::MAX as u64
                    && p.expires_ms >= p.requested_ms
                    && (1000..=60_000).contains(&p.ready_ms)
                    && p.drain.len() <= 256,
                "invalid durable preparation"
            );
            let active = matches!(
                p.phase,
                crate::preparation::PreparationPhase::Active
                    | crate::preparation::PreparationPhase::Reconciling
            );
            ensure!(
                active
                    == self.reservations.iter().any(|r| r.id == p.id
                        && r.granted
                        && r.domain == p.domain
                        && r.identity.uid == p.uid
                        && r.memory_bytes == p.memory_bytes
                        && r.swap_bytes == p.swap_bytes),
                "preparation/native claim mismatch"
            );
        }
        let unique: BTreeSet<_> = self.preparations.iter().map(|p| &p.id).collect();
        ensure!(
            unique.len() == self.preparations.len(),
            "duplicate preparation identity"
        );
        if let Some(lease) = &self.recovery {
            lease.validate()?;
        }
        ensure!(
            self.continuations.len() <= 256,
            "too many operation completion contracts"
        );
        let mut operation_ids = BTreeSet::new();
        for c in &self.continuations {
            ensure!(
                crate::ledger::valid_name(&c.capability.parent)
                    && operation_ids.insert(&c.capability.parent)
                    && crate::ledger::valid_name(&c.capability.key)
                    && c.calls.len() <= c.policy.max_calls
                    && (1..=64).contains(&c.policy.max_calls)
                    && c.calls.iter().all(|id| crate::ledger::valid_name(id))
                    && (1..=16).contains(&c.policy.domains.len())
                    && c.policy
                        .domains
                        .iter()
                        .all(|name| crate::ledger::valid_name(name))
                    && c.uid != 0
                    && c.policy.swap_bytes <= i64::MAX as u64
                    && c.policy.memory_bytes > 0
                    && c.policy.memory_bytes <= i64::MAX as u64,
                "invalid durable completion rights"
            );
        }
        ensure!(
            self.pool_operations.len() <= 256
                && self
                    .pool_operations
                    .keys()
                    .all(|k| crate::root_pool::valid_operation_key(k)),
            "invalid root operation ownership"
        );
        Ok(())
    }

    pub fn committed(&self) -> u64 {
        self.reservations
            .iter()
            .filter(|r| r.granted)
            .fold(0u64, |sum, r| sum.saturating_add(r.memory_bytes))
            .saturating_add(
                self.preparations
                    .iter()
                    .filter(|p| p.phase == crate::preparation::PreparationPhase::Ready)
                    .fold(0u64, |sum, p| sum.saturating_add(p.memory_bytes)),
            )
            .saturating_add(self.completion_escrow(false))
    }

    pub fn swap_committed(&self) -> u64 {
        self.reservations
            .iter()
            .filter(|r| r.granted)
            .fold(0u64, |sum, r| sum.saturating_add(r.swap_bytes))
            .saturating_add(
                self.preparations
                    .iter()
                    .filter(|p| p.phase == crate::preparation::PreparationPhase::Ready)
                    .fold(0u64, |sum, p| sum.saturating_add(p.swap_bytes)),
            )
            .saturating_add(self.completion_escrow(true))
    }

    pub fn return_claim(&self, policy: &HostPolicy) -> u64 {
        let debt = if policy.reserve_swap_return {
            self.swap_return_bytes.unwrap_or(u64::MAX)
        } else {
            0
        };
        self.recovery.as_ref().map_or(debt, |r| {
            debt.max(r.return_bytes).saturating_add(r.helper_bytes)
        })
    }

    pub fn burst_committed(&self) -> u64 {
        self.reservations
            .iter()
            .filter(|r| r.granted && r.burst)
            .fold(0u64, |sum, r| sum.saturating_add(r.memory_bytes))
    }

    fn burst_wait(&self, r: &Reservation, policy: &HostPolicy, now: u64) -> Option<WaitReason> {
        let Some(burst) = &policy.burst else {
            return Some(WaitReason::BurstPolicy);
        };
        if r.memory_bytes > burst.max_job_bytes
            || r.swap_bytes != 0
            || !r
                .runtime_max_ms
                .is_some_and(|ms| ms > 0 && ms <= burst.max_runtime_ms)
            || !policy
                .domains
                .iter()
                .any(|d| d.name == r.domain && d.uid == r.identity.uid && d.burst)
        {
            Some(WaitReason::BurstPolicy)
        } else if r.memory_bytes > burst.budget_bytes.saturating_sub(self.burst_committed()) {
            Some(WaitReason::BurstBudget)
        } else if self
            .reservations
            .iter()
            .filter(|r| r.granted && r.burst)
            .count()
            >= burst.max_running
        {
            Some(WaitReason::BurstConcurrency)
        } else if self
            .burst_last_granted
            .get(&r.identity.uid)
            .is_some_and(|last| now.saturating_sub(*last) < burst.min_interval_ms)
        {
            Some(WaitReason::BurstRate)
        } else {
            None
        }
    }

    /// Idempotent retries bind to the native identity, not the socket lifetime.
    pub fn request(&mut self, reservation: Reservation, policy: &HostPolicy) -> Result<String> {
        ensure!(
            self.is_continuation(&reservation)
                || self.may_join(&reservation.identity)
                || !self
                    .reservations
                    .iter()
                    .any(|r| r.identity.cgroup == reservation.identity.cgroup
                        && r.identity.inode == reservation.identity.inode),
            "new execution owners cannot join a draining pool"
        );
        let domain = policy
            .domains
            .iter()
            .find(|d| d.name == reservation.domain && d.uid == reservation.identity.uid)
            .ok_or_else(|| anyhow::anyhow!("unknown host domain"))?;
        ensure!(
            reservation.burst == domain.burst,
            "native burst class mismatch"
        );
        if reservation.burst {
            let burst = policy
                .burst
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("burst admission is disabled"))?;
            ensure!(
                reservation.identity.uid != 0
                    && reservation.swap_bytes == 0
                    && reservation.memory_bytes <= burst.max_job_bytes
                    && reservation
                        .runtime_max_ms
                        .is_some_and(|ms| ms > 0 && ms <= burst.max_runtime_ms),
                "burst requires a small zero-swap boundary and verified short native deadline"
            );
        } else {
            ensure!(
                reservation.runtime_max_ms.is_none(),
                "unexpected burst deadline proof"
            );
        }
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
                    && r.swap_bytes == reservation.swap_bytes
                    && r.burst == reservation.burst
                    && r.runtime_max_ms == reservation.runtime_max_ms,
                "native identity or ceiling changed"
            );
            let owner = crate::ledger::ClientIdentity {
                pid: reservation.identity.pid,
                start_ticks: reservation.identity.start_ticks,
            };
            if r.identity.uid == 0 && !r.owners.contains(&owner) {
                ensure!(r.owners.len() < 256, "execution owner bound exceeded");
                r.owners.push(owner);
                r.owners_finished = false;
            }
            if !r.granted && reservation.continuation.is_some() {
                r.continuation = reservation.continuation.clone();
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
        healthy_since: &mut BTreeMap<String, u64>,
        mut ancestry: impl FnMut(&Reservation, &[Reservation]) -> Option<u64>,
    ) -> BTreeMap<String, WaitReason> {
        self.reservations
            .retain(|r| r.granted || now < r.deadline_ms);
        self.retain_continuations();
        // Every supported cooldown is <= one hour; older history can be pruned.
        self.burst_last_granted
            .retain(|_, last| now.saturating_sub(*last) <= 3_600_000);
        healthy_since.retain(|name, _| policy.domains.iter().any(|d| &d.name == name));
        for domain in &policy.domains {
            let healthy = capacity.is_some_and(|c| {
                c.available_bytes >= policy.reserve_bytes.max(domain.min_available_bytes)
                    && c.swap_free_bytes >= policy.swap_reserve_bytes
                    && (0.0..=100.0).contains(&c.memory_full_psi)
                    && c.memory_full_psi < policy.max_memory_full_psi
                    && (domain.io_pressure == IoPressure::Diagnostic
                        || ((0.0..=100.0).contains(&c.io_full_psi)
                            && c.io_full_psi < policy.max_io_full_psi))
            });
            if healthy {
                healthy_since.entry(domain.name.clone()).or_insert(now);
            } else {
                healthy_since.remove(&domain.name);
            }
        }
        self.advance_preparations(now, policy, capacity, healthy_since, &mut ancestry);
        let mut waiting = BTreeMap::new();
        let mut aged_block = false;
        let mut burst_aged_block = false;
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
            let ready = healthy_since
                .get(&r.domain)
                .is_some_and(|since| now.saturating_sub(*since) >= policy.resume_ms);
            let burst_wait = r.burst.then(|| self.burst_wait(r, policy, now)).flatten();
            let budget = policy.budget_bytes.saturating_add(if r.burst {
                policy.burst.as_ref().map_or(0, |b| b.budget_bytes)
            } else {
                0
            });
            let memory_charge = self.admission_charge(r, policy, false);
            let swap_charge = self.admission_charge(r, policy, true);
            let reason = if self.preparation_barrier() && !self.is_continuation(r) {
                Some(WaitReason::Preparation)
            } else if self.recovery.is_some() {
                Some(WaitReason::Recovery)
            } else if capacity.is_none()
                || (policy.reserve_swap_return && self.swap_return_bytes.is_none())
            {
                Some(WaitReason::Unknown)
            } else if !ready {
                Some(WaitReason::Pressure)
            } else if let Some(reason) = burst_wait {
                Some(reason)
            } else if self.completion_slot_busy(r) {
                Some(WaitReason::ContinuationSlot)
            } else if aged_block && (!r.burst || burst_aged_block) {
                Some(WaitReason::AgedRequest)
            } else if memory_charge > budget.saturating_sub(committed) {
                Some(WaitReason::Budget)
            } else if memory_charge
                > capacity
                    .map_or(0, |c| c.available_bytes)
                    .saturating_sub(policy.reserve_bytes)
                    .saturating_sub(committed)
                    .saturating_sub(self.return_claim(policy))
            {
                Some(if self.return_claim(policy) > 0 {
                    WaitReason::SwapReturn
                } else {
                    WaitReason::HostHeadroom
                })
            } else if swap_charge
                > capacity
                    .map_or(0, |c| {
                        c.swap_free_bytes.saturating_sub(policy.swap_reserve_bytes)
                    })
                    .saturating_sub(self.swap_committed())
            {
                Some(WaitReason::SwapHeadroom)
            } else {
                // Full ceilings remain counted over live usage in v1. Credit
                // requires boundary-specific proof, not an RSS approximation.
                self.completion_ancestry_wait(r, policy, &mut ancestry)
            };
            if let Some(reason) = reason {
                // An aged request whose own pressure gate is closed cannot
                // veto healthy peers. Capacity/fairness waits still age-block.
                aged_block |= !r.burst
                    && matches!(
                        reason,
                        WaitReason::Budget
                            | WaitReason::HostHeadroom
                            | WaitReason::SwapReturn
                            | WaitReason::SwapHeadroom
                            | WaitReason::AncestorHeadroom
                            | WaitReason::AgedRequest
                    )
                    && now.saturating_sub(r.requested_ms) >= policy.aging_ms;
                // A waiting bulk job that could fit once bursts drain must get
                // that quiet window. A full normal budget can still serve bursts.
                if !r.burst
                    && matches!(
                        reason,
                        WaitReason::Budget
                            | WaitReason::HostHeadroom
                            | WaitReason::SwapReturn
                            | WaitReason::SwapHeadroom
                            | WaitReason::AncestorHeadroom
                            | WaitReason::AgedRequest
                    )
                    && now.saturating_sub(r.requested_ms) >= policy.aging_ms
                {
                    let burst_committed = self.burst_committed();
                    let normal_committed = committed.saturating_sub(burst_committed);
                    // If even reclaiming every burst byte cannot make this
                    // request fit, an aged bulk wait must not veto diagnostics.
                    // This is only a backfill veto, never headroom for a grant.
                    let possible_host_bytes = capacity
                        .map_or(0, |c| c.available_bytes)
                        .saturating_add(burst_committed)
                        .saturating_sub(policy.reserve_bytes)
                        .saturating_sub(normal_committed);
                    burst_aged_block |= r.memory_bytes
                        <= policy.budget_bytes.saturating_sub(normal_committed)
                        && r.memory_bytes <= possible_host_bytes;
                }
                waiting.insert(r.id.clone(), reason);
            } else {
                if r.burst {
                    self.burst_last_granted.insert(r.identity.uid, now);
                }
                self.reservations[i].granted = true;
                let id = self.reservations[i].id.clone();
                // This is part of the same persisted grant transaction.
                // Entropy failure fails closed rather than publishing an unbacked grant.
                if self.mint_completion(&id, policy).is_err() {
                    self.reservations[i].granted = false;
                    waiting.insert(id, WaitReason::Unknown);
                }
            }
        }
        waiting
    }

    pub fn reconcile(&mut self, mut empty: impl FnMut(&Reservation) -> Option<bool>) {
        self.reservations.retain(|r| empty(r) != Some(true));
        self.preparations.retain(|p| {
            !matches!(
                p.phase,
                crate::preparation::PreparationPhase::Active
                    | crate::preparation::PreparationPhase::Reconciling
            ) || self.reservations.iter().any(|r| r.id == p.id)
        });
        self.retain_continuations();
    }
}
