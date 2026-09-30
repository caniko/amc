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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub slice: String,
    pub memory_max: u64,
    pub memory_swap_max: u64,
    pub max_running: usize,
    pub pause_file: Option<PathBuf>,
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

impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported admission policy version");
        ensure!(self.budget_bytes > 0, "empty admission budget");
        ensure!(
            (1..=256).contains(&self.queue_limit),
            "queue_limit must be 1..=256"
        );
        ensure!(
            !self.contracts.is_empty() && self.contracts.len() <= 32,
            "invalid contract count"
        );
        for (name, contract) in &self.contracts {
            ensure!(valid_name(name), "invalid contract name");
            contract.validate()?;
            ensure!(
                contract.memory_max <= self.budget_bytes,
                "contract cannot fit budget"
            );
        }
        Ok(())
    }
}

impl Contract {
    pub fn validate(&self) -> Result<()> {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub contract: Contract,
    pub phase: Phase,
    pub deadline_ms: u64,
    pub identity: Option<Identity>,
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
}

impl Ledger {
    pub fn new(boot_id: String) -> Self {
        Self {
            version: 1,
            boot_id,
            entries: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.entries.len() <= 256,
            "invalid admission ledger"
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
            .filter(|e| e.phase != Phase::Queued)
            .fold(0u64, |sum, e| sum.saturating_add(e.contract.memory_max))
    }

    pub fn enqueue(
        &mut self,
        id: String,
        name: &str,
        policy: &Policy,
        deadline_ms: u64,
    ) -> Result<()> {
        ensure!(
            self.entries.len() < policy.queue_limit,
            "admission queue is full"
        );
        ensure!(
            valid_name(&id) && self.get(&id).is_none(),
            "invalid or duplicate ticket"
        );
        let contract = policy
            .contracts
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown contract"))?
            .clone();
        self.entries.push(Entry {
            id,
            name: name.into(),
            contract,
            phase: Phase::Queued,
            deadline_ms,
            identity: None,
        });
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
        let mut decisions = BTreeMap::new();
        let mut blocked = BTreeSet::new();
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            if entry.phase != Phase::Queued {
                continue;
            }
            let committed = self.committed();
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
            } else if entry.contract.memory_max > policy.budget_bytes.saturating_sub(committed) {
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
