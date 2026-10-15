//! Serializable accounting state. No client owns a releasable permit.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
    pub queue_limit: usize,
    pub contracts: BTreeMap<String, Contract>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub burst_budget_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub slice: String,
    pub memory_max: u64,
    pub memory_swap_max: u64,
    pub max_running: usize,
    pub pause_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub burst: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_max_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_pool: Option<crate::protected_pool::ProtectedPool>,
}

/// Shared headroom includes host and ancestors; sibling slices are separate.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Headroom {
    pub shared_bytes: u64,
    pub slice_bytes: u64,
    pub memory_max: u64,
    pub memory_swap_max: u64,
    pub paused: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Fifo,
    Concurrency,
    Budget,
    SharedHeadroom,
    SliceHeadroom,
    Paused,
    NativeUnavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Decision {
    pub reason: WaitReason,
    pub headroom: Option<Headroom>,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported admission policy version");
        ensure!(self.budget_bytes > 0, "empty admission budget");
        ensure!(
            self.budget_bytes
                .checked_add(self.burst_budget_bytes)
                .is_some_and(|total| total <= i64::MAX as u64),
            "admission burst budget overflow"
        );
        ensure!(
            (1..=256).contains(&self.queue_limit),
            "queue_limit must be 1..=256"
        );
        ensure!(
            !self.contracts.is_empty() && self.contracts.len() <= 32,
            "invalid contract count"
        );
        let mut slice_classes = BTreeMap::new();
        let mut sources = BTreeSet::new();
        let mut pool_bytes = 0u64;
        for (name, contract) in &self.contracts {
            ensure!(valid_name(name), "invalid contract name");
            contract.validate()?;
            if let Some(pool) = &contract.protected_pool {
                ensure!(
                    pool.memory_bytes <= self.budget_bytes,
                    "protected pool cannot fit budget"
                );
                ensure!(
                    sources.insert(&pool.source_contract)
                        && self
                            .contracts
                            .get(&pool.source_contract)
                            .is_some_and(|source| source.protected_pool.is_none() && !source.burst),
                    "protected pool needs an exclusive ordinary source contract"
                );
                pool_bytes = pool_bytes
                    .checked_add(pool.memory_bytes)
                    .ok_or_else(|| anyhow::anyhow!("protected pool backing overflow"))?;
                ensure!(
                    self.contracts
                        .values()
                        .filter(|c| c.slice == contract.slice)
                        .count()
                        == 1,
                    "protected pool needs an exclusive native slice"
                );
            }
            ensure!(
                slice_classes
                    .insert(&contract.slice, contract.burst)
                    .is_none_or(|burst| burst == contract.burst),
                "contracts sharing a native slice must use the same burst class"
            );
            ensure!(
                contract.memory_max <= self.budget_bytes,
                "contract cannot fit budget"
            );
            ensure!(
                !contract.burst || contract.memory_max <= self.burst_budget_bytes,
                "burst contract cannot fit burst allowance"
            );
        }
        ensure!(
            pool_bytes <= self.budget_bytes,
            "protected pools cannot fit user budget"
        );
        ensure!(
            self.queue_limit
                + self
                    .contracts
                    .values()
                    .filter_map(|c| c.protected_pool.as_ref())
                    .map(|p| p.queue_limit)
                    .sum::<usize>()
                <= 256,
            "combined queues exceed ledger bound"
        );
        Ok(())
    }
}

impl Contract {
    pub fn validate(&self) -> Result<()> {
        if let Some(pool) = &self.protected_pool {
            pool.validate()?;
            ensure!(
                !self.burst
                    && self.memory_swap_max == 0
                    && self.memory_max <= pool.max_job_bytes
                    && self.max_running <= pool.max_running
                    && self
                        .runtime_max_sec
                        .is_some_and(|seconds| seconds <= pool.runtime_max_sec),
                "protected calls require small zero-swap ceilings and short native deadlines"
            );
        }
        ensure!(
            self.slice.strip_suffix(".slice").is_some_and(valid_name),
            "invalid slice"
        );
        ensure!(
            self.memory_max > 0 && self.memory_max <= i64::MAX as u64,
            "invalid memory ceiling"
        );
        ensure!(
            (1..=64).contains(&self.max_running),
            "invalid contract concurrency"
        );
        ensure!(
            self.pause_file.as_ref().is_none_or(|p| p.is_absolute()),
            "pause marker must be absolute"
        );
        ensure!(
            self.pause_file
                .as_ref()
                .is_none_or(|p| p.as_os_str().len() <= 4096),
            "pause marker exceeds path bound"
        );
        ensure!(
            self.runtime_max_sec
                .is_none_or(|seconds| (1..=86400).contains(&seconds)),
            "invalid native runtime deadline"
        );
        ensure!(
            !self.burst
                || (self.memory_swap_max == 0
                    && self.runtime_max_sec.is_some_and(|seconds| seconds <= 30)),
            "burst contract requires zero swap and a short native deadline"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Reserved,
    Running,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub invocation: String,
    pub cgroup: String,
    pub inode: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub pid: i32,
    pub start_ticks: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub contract: Contract,
    pub phase: Phase,
    pub deadline_ms: u64,
    pub identity: Option<Identity>,
    #[serde(default)]
    pub client: Option<ClientIdentity>,
}

impl Entry {
    pub fn unit(&self) -> String {
        format!("app-amc-job-{}.service", self.id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    pub version: u32,
    pub boot_id: String,
    pub entries: Vec<Entry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub protected_pools: BTreeMap<String, u64>,
}

impl Ledger {
    pub fn new(boot_id: String) -> Self {
        Self {
            version: 1,
            boot_id,
            entries: Vec::new(),
            protected_pools: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.entries.len() <= 256,
            "invalid admission ledger"
        );
        ensure!(
            self.protected_pools.len() <= 32
                && self.protected_pools.iter().all(|(slice, bytes)| slice
                    .strip_suffix(".slice")
                    .is_some_and(valid_name)
                    && *bytes > 0
                    && *bytes <= i64::MAX as u64),
            "invalid durable user pool backing"
        );
        let mut ids = std::collections::BTreeSet::new();
        for entry in &self.entries {
            ensure!(
                valid_name(&entry.id) && ids.insert(&entry.id),
                "invalid or duplicate reservation identity"
            );
            entry.contract.validate()?;
            ensure!(
                (entry.phase == Phase::Running) == entry.identity.is_some(),
                "inconsistent workload identity"
            );
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn committed(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.phase != Phase::Queued && e.contract.protected_pool.is_none())
            .fold(0u64, |sum, e| sum.saturating_add(e.contract.memory_max))
            .saturating_add(
                self.protected_pools
                    .values()
                    .fold(0u64, |sum, bytes| sum.saturating_add(*bytes)),
            )
    }

    pub fn sync_protected_pools(&mut self, policy: &Policy) {
        // Entry snapshots retain superseded backing until native reconciliation.
        for contract in policy
            .contracts
            .values()
            .chain(self.entries.iter().map(|e| &e.contract))
        {
            if let Some(pool) = &contract.protected_pool {
                let bytes = self
                    .protected_pools
                    .entry(contract.slice.clone())
                    .or_default();
                *bytes = (*bytes).max(pool.memory_bytes);
            }
        }
    }

    pub fn reconcile_protected_pools(
        &mut self,
        policy: &Policy,
        mut empty: impl FnMut(&str) -> Option<bool>,
    ) {
        self.protected_pools.retain(|slice, bytes| {
            if self.entries.iter().any(|e| &e.contract.slice == slice) {
                return true;
            }
            let current = policy
                .contracts
                .values()
                .find(|c| &c.slice == slice)
                .and_then(|c| c.protected_pool.as_ref())
                .map(|p| p.memory_bytes);
            if current.is_some_and(|new| new >= *bytes) || empty(slice) != Some(true) {
                return true;
            }
            if let Some(new) = current {
                *bytes = new;
                true
            } else {
                false
            }
        });
        self.sync_protected_pools(policy);
    }

    pub fn enqueue_bounded(
        &mut self,
        id: String,
        name: &str,
        policy: &Policy,
        deadline_ms: u64,
        memory_max: u64,
        runtime_max_sec: u64,
    ) -> Result<()> {
        ensure!(
            (1..=86400).contains(&runtime_max_sec),
            "invalid bounded call deadline"
        );
        let source = policy
            .contracts
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown contract"))?;
        ensure!(
            memory_max > 0 && memory_max <= source.memory_max,
            "requested memory exceeds contract"
        );
        let selected = policy
            .contracts
            .iter()
            .find(|(_, c)| {
                c.protected_pool.as_ref().is_some_and(|p| {
                    p.source_contract == name
                        && memory_max <= c.memory_max
                        && runtime_max_sec <= c.runtime_max_sec.unwrap_or(0)
                })
            })
            .map_or(name, |(name, _)| name.as_str());
        ensure!(
            policy.contracts[selected]
                .runtime_max_sec
                .is_none_or(|max| runtime_max_sec <= max),
            "requested runtime exceeds contract"
        );
        self.enqueue_sized(id, selected, policy, deadline_ms, Some(memory_max))?;
        self.entries
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("missing enqueued call"))?
            .contract
            .runtime_max_sec = Some(runtime_max_sec);
        Ok(())
    }

    pub fn enqueue(
        &mut self,
        id: String,
        name: &str,
        policy: &Policy,
        deadline_ms: u64,
    ) -> Result<()> {
        self.enqueue_sized(id, name, policy, deadline_ms, None)
    }

    pub fn enqueue_sized(
        &mut self,
        id: String,
        name: &str,
        policy: &Policy,
        deadline_ms: u64,
        memory_max: Option<u64>,
    ) -> Result<()> {
        self.sync_protected_pools(policy);
        ensure!(
            valid_name(&id) && self.get(&id).is_none(),
            "invalid or duplicate ticket"
        );
        let mut contract = policy
            .contracts
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown contract"))?
            .clone();
        let count = self
            .entries
            .iter()
            .filter(|entry| match &contract.protected_pool {
                Some(_) => entry.contract.slice == contract.slice,
                None => entry.contract.protected_pool.is_none(),
            })
            .count();
        ensure!(
            count
                < contract
                    .protected_pool
                    .as_ref()
                    .map_or(policy.queue_limit, |pool| pool.queue_limit),
            "admission queue is full"
        );
        if let Some(bytes) = memory_max {
            ensure!(
                bytes > 0 && bytes <= contract.memory_max,
                "requested memory exceeds contract"
            );
            contract.memory_max = bytes;
        }
        self.entries.push(Entry {
            id,
            name: name.into(),
            contract,
            phase: Phase::Queued,
            deadline_ms,
            identity: None,
            client: None,
        });
        Ok(())
    }

    pub fn set_client(&mut self, id: &str, client: ClientIdentity) -> Result<()> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown ticket"))?;
        ensure!(
            client.pid > 0 && client.start_ticks > 0 && entry.client.is_none(),
            "invalid submitting client"
        );
        entry.client = Some(client);
        Ok(())
    }

    /// FIFO within each slice. A long-lived interactive pool cannot block a
    /// disposable pool's queue. Unknown capacity never admits; lowering policy
    /// never revokes. Resident committed memory is conservatively counted twice.
    pub fn advance(
        &mut self,
        now: u64,
        policy: &Policy,
        mut headroom: impl FnMut(&Contract) -> Option<Headroom>,
    ) -> BTreeMap<String, Decision> {
        self.entries
            .retain(|e| e.phase == Phase::Running || now < e.deadline_ms);
        self.sync_protected_pools(policy);
        let mut decisions = BTreeMap::new();
        let mut blocked = BTreeSet::new();
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            if entry.phase != Phase::Queued {
                continue;
            }
            let committed = self.committed();
            let burst_committed = self
                .entries
                .iter()
                .filter(|e| e.phase != Phase::Queued && e.contract.burst)
                .fold(0u64, |sum, e| sum.saturating_add(e.contract.memory_max));
            let budget = policy.budget_bytes.saturating_add(if entry.contract.burst {
                policy.burst_budget_bytes
            } else {
                0
            });
            let prefix = format!("{}-", entry.contract.slice.trim_end_matches(".slice"));
            let slice_committed = self
                .entries
                .iter()
                .filter(|e| {
                    e.phase != Phase::Queued
                        && (e.contract.slice == entry.contract.slice
                            || e.contract.slice.starts_with(&prefix))
                })
                .fold(0u64, |sum, e| sum.saturating_add(e.contract.memory_max));
            let count = self
                .entries
                .iter()
                .filter(|e| e.name == entry.name && e.phase != Phase::Queued)
                .count();
            let mut observed = None;
            let reason = if blocked.contains(&entry.contract.slice) {
                Some(WaitReason::Fifo)
            } else if count >= entry.contract.max_running {
                Some(WaitReason::Concurrency)
            } else if let Some(pool) = &entry.contract.protected_pool {
                observed = headroom(&entry.contract);
                match observed {
                    None => Some(WaitReason::NativeUnavailable),
                    Some(h) if h.paused => Some(WaitReason::Paused),
                    Some(h) if h.memory_max != pool.memory_bytes || h.memory_swap_max != 0 => {
                        Some(WaitReason::NativeUnavailable)
                    }
                    Some(_) if committed.saturating_sub(burst_committed) > policy.budget_bytes => {
                        Some(WaitReason::Budget)
                    }
                    Some(_) => None,
                }
            } else if entry.contract.memory_max > budget.saturating_sub(committed)
                || (entry.contract.burst
                    && entry.contract.memory_max
                        > policy.burst_budget_bytes.saturating_sub(burst_committed))
            {
                Some(WaitReason::Budget)
            } else {
                observed = headroom(&entry.contract);
                match observed {
                    None => Some(WaitReason::NativeUnavailable),
                    Some(h) if h.paused => Some(WaitReason::Paused),
                    Some(h)
                        if entry.contract.memory_max > h.shared_bytes.saturating_sub(committed) =>
                    {
                        Some(WaitReason::SharedHeadroom)
                    }
                    Some(h)
                        if entry.contract.memory_max
                            > h.slice_bytes.saturating_sub(slice_committed) =>
                    {
                        Some(WaitReason::SliceHeadroom)
                    }
                    Some(_) => None,
                }
            };
            if let Some(reason) = reason {
                blocked.insert(entry.contract.slice.clone());
                decisions.insert(
                    entry.id.clone(),
                    Decision {
                        reason,
                        headroom: observed,
                    },
                );
            } else {
                self.entries[index].phase = Phase::Reserved;
            }
        }
        decisions
    }

    /// Called by the native pre-exec helper, not the submitting client. The
    /// server verifies the peer PID's placement before persisting this change.
    pub fn enter(&mut self, id: &str, identity: Identity) -> Result<()> {
        ensure!(
            identity.cgroup.len() <= 4096 && identity.invocation.len() <= 32,
            "workload identity exceeds bounds"
        );
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("ticket expired or unknown"))?;
        ensure!(
            entry.phase == Phase::Reserved,
            "ticket is not ready or already entered"
        );
        entry.phase = Phase::Running;
        entry.identity = Some(identity);
        Ok(())
    }

    pub fn cancel_pending(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| e.id != id || e.phase == Phase::Running);
        before != self.entries.len()
    }

    /// Only authoritative native termination evidence may retire a running
    /// entry. A missing reply, client death, or expired deadline is insufficient.
    pub fn reconcile(&mut self, mut terminated: impl FnMut(&Entry) -> bool) {
        self.entries
            .retain(|e| e.phase != Phase::Running || !terminated(e));
    }
}
