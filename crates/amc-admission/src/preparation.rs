//! Advance intent is a barrier, not a grant. Only a ready intent owns capacity.
//! Consuming it replaces that claim with one native reservation atomically.
use crate::host::{Capacity, HostLedger, HostPolicy, Identity, Reservation, WaitReason};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationProfile {
    pub name: String,
    pub domain: String,
    pub memory_bytes: u64,
    pub swap_bytes: u64,
    /// Finite operations to finish before launch. Long-lived domains can remain.
    pub drain_domains: Vec<String>,
    pub wait_ms: u64,
    pub ready_ms: u64,
}

impl PreparationProfile {
    pub fn validate(&self, policy: &HostPolicy) -> Result<()> {
        let domain = policy
            .domains
            .iter()
            .find(|d| d.name == self.domain)
            .ok_or_else(|| anyhow::anyhow!("unknown preparation domain"))?;
        ensure!(
            crate::ledger::valid_name(&self.name)
                && domain.uid != 0
                && !domain.burst
                && domain.continuation.is_none()
                && domain.cgroup.ends_with(".slice")
                && self.memory_bytes > 0
                && self.memory_bytes <= domain.ceiling_bytes
                && self.swap_bytes <= domain.swap_bytes
                && (1000..=3_600_000).contains(&self.wait_ms)
                // Polling, the five-second bus call and five-second placement
                // check must fit before the once-only native transfer expires.
                && (15_000..=60_000).contains(&self.ready_ms)
                && self.drain_domains.len() <= policy.domains.len()
                && self
                    .drain_domains
                    .iter()
                    .all(|name| policy.domains.iter().any(|d| &d.name == name)),
            "invalid preparation profile"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreparationPhase {
    Draining,
    Ready,
    Active,
    Reconciling,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preparation {
    pub id: String,
    /// Never include the capability in public status replies.
    pub key: String,
    pub uid: u32,
    pub profile: String,
    pub domain: String,
    pub memory_bytes: u64,
    pub swap_bytes: u64,
    pub requested_ms: u64,
    pub expires_ms: u64,
    pub ready_ms: u64,
    pub phase: PreparationPhase,
    pub drain: Vec<String>,
    pub waiting: Option<WaitReason>,
}

impl HostLedger {
    pub fn preparation_barrier(&self) -> bool {
        self.preparations.iter().any(|p| {
            matches!(
                p.phase,
                PreparationPhase::Draining | PreparationPhase::Ready
            )
        })
    }

    /// Retries by existing root handlers keep their already-backed pool rights.
    pub fn may_join(&self, identity: &Identity) -> bool {
        !self.preparation_barrier() && self.recovery.is_none()
            || self.reservations.iter().any(|r| {
                r.granted
                    && r.identity.cgroup == identity.cgroup
                    && r.identity.inode == identity.inode
                    && r.identity.uid == identity.uid
                    && r.owners
                        .iter()
                        .any(|o| o.pid == identity.pid && o.start_ticks == identity.start_ticks)
            })
    }

    pub fn prepare(
        &mut self,
        id: String,
        key: String,
        uid: u32,
        profile: &str,
        now: u64,
        policy: &HostPolicy,
    ) -> Result<()> {
        ensure!(
            crate::ledger::valid_name(&id) && crate::ledger::valid_name(&key),
            "invalid preparation identity"
        );
        if let Some(p) = self.preparations.iter().find(|p| p.id == id) {
            ensure!(
                p.uid == uid && p.key == key && p.profile == profile,
                "preparation identity changed"
            );
            return Ok(());
        }
        let spec = policy
            .preparations
            .iter()
            .find(|p| {
                p.name == profile
                    && policy
                        .domains
                        .iter()
                        .any(|d| d.name == p.domain && d.uid == uid)
            })
            .ok_or_else(|| anyhow::anyhow!("unknown preparation profile"))?;
        let domain = policy
            .domains
            .iter()
            .find(|d| d.name == spec.domain && d.uid == uid)
            .ok_or_else(|| anyhow::anyhow!("preparation profile belongs to another user"))?;
        ensure!(
            self.preparations.len() + self.reservations.len() < policy.queue_limit
                && !self.reservations.iter().any(|r| r.id == id),
            "host preparation queue is full or ticket exists"
        );
        let drain = self
            .reservations
            .iter()
            .filter(|r| {
                let operation = r.continuation.as_deref().unwrap_or(&r.id);
                (r.granted || self.is_continuation(r))
                    && (spec.drain_domains.contains(&r.domain)
                        || self.continuations.iter().any(|c| {
                            c.capability.parent == operation
                                && c.policy
                                    .domains
                                    .iter()
                                    .any(|domain| spec.drain_domains.contains(domain))
                        }))
            })
            .map(|r| r.continuation.clone().unwrap_or_else(|| r.id.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        self.preparations.push(Preparation {
            id,
            key,
            uid,
            profile: profile.into(),
            domain: domain.name.clone(),
            memory_bytes: spec.memory_bytes,
            swap_bytes: spec.swap_bytes,
            requested_ms: now,
            expires_ms: now.saturating_add(spec.wait_ms),
            ready_ms: spec.ready_ms,
            phase: PreparationPhase::Draining,
            drain,
            waiting: Some(WaitReason::Draining),
        });
        Ok(())
    }

    pub fn cancel_preparation(&mut self, id: &str, key: &str, uid: u32) -> Result<()> {
        let Some(p) = self.preparations.iter_mut().find(|p| p.id == id) else {
            return Ok(());
        };
        ensure!(p.key == key && p.uid == uid, "preparation owner mismatch");
        if matches!(
            p.phase,
            PreparationPhase::Active | PreparationPhase::Reconciling
        ) {
            p.phase = PreparationPhase::Reconciling;
        } else {
            self.preparations.retain(|p| p.id != id);
        }
        Ok(())
    }

    /// Caller has re-observed native enforcement and capacity immediately before
    /// this transaction. A lost reply is retryable only by the same native peer.
    pub fn consume(
        &mut self,
        id: &str,
        key: &str,
        uid: u32,
        mut native: Reservation,
        now: u64,
    ) -> Result<()> {
        let p = self
            .preparations
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown or expired preparation"))?;
        ensure!(
            p.uid == uid
                && p.key == key
                && native.identity.uid == uid
                && native.domain == p.domain
                && native.memory_bytes == p.memory_bytes
                && native.swap_bytes == p.swap_bytes
                && !native.burst
                && native.runtime_max_ms.is_none(),
            "preparation capability or native envelope mismatch"
        );
        if matches!(
            p.phase,
            PreparationPhase::Active | PreparationPhase::Reconciling
        ) {
            ensure!(
                self.reservations
                    .iter()
                    .any(|r| r.id == id && r.granted && r.identity == native.identity),
                "preparation already consumed by another native execution"
            );
            return Ok(());
        }
        ensure!(
            p.phase == PreparationPhase::Ready
                && now < p.expires_ms
                && !self
                    .reservations
                    .iter()
                    .any(|r| r.id == id || r.identity.cgroup == native.identity.cgroup),
            "preparation is not ready or already bound"
        );
        native.id = id.into();
        native.granted = true;
        native.owners = vec![crate::ledger::ClientIdentity {
            pid: native.identity.pid,
            start_ticks: native.identity.start_ticks,
        }];
        self.reservations.push(native);
        p.phase = PreparationPhase::Active;
        p.waiting = None;
        Ok(())
    }

    pub(crate) fn advance_preparations(
        &mut self,
        now: u64,
        policy: &HostPolicy,
        capacity: Option<Capacity>,
        healthy: &BTreeMap<String, u64>,
        ancestry: &mut impl FnMut(&Reservation, &[Reservation]) -> Option<u64>,
    ) {
        self.preparations.retain(|p| match p.phase {
            PreparationPhase::Draining | PreparationPhase::Ready => now < p.expires_ms,
            PreparationPhase::Active | PreparationPhase::Reconciling => {
                self.reservations.iter().any(|r| r.id == p.id)
            }
        });
        // FIFO among outstanding intentions; a ready claim cannot be leapfrogged.
        let Some(index) = self.preparations.iter().position(|p| {
            matches!(
                p.phase,
                PreparationPhase::Draining | PreparationPhase::Ready
            )
        }) else {
            return;
        };
        if self.preparations[index].phase == PreparationPhase::Ready {
            return;
        }
        let p = &self.preparations[index];
        let spec_matches = policy.preparations.iter().any(|spec| {
            spec.name == p.profile
                && spec.domain == p.domain
                && spec.memory_bytes == p.memory_bytes
                && spec.swap_bytes == p.swap_bytes
        });
        let candidate = Reservation {
            id: p.id.clone(),
            domain: p.domain.clone(),
            identity: Identity {
                cgroup: policy
                    .domains
                    .iter()
                    .find(|d| d.name == p.domain)
                    .map_or(String::new(), |d| d.cgroup.clone()),
                inode: 0,
                uid: p.uid,
                pid: 0,
                start_ticks: 0,
            },
            memory_bytes: p.memory_bytes,
            swap_bytes: p.swap_bytes,
            requested_ms: p.requested_ms,
            deadline_ms: p.expires_ms,
            granted: false,
            owners: vec![],
            burst: false,
            runtime_max_ms: None,
            continuation: None,
            owners_finished: false,
        };
        let waiting = if !spec_matches
            || capacity.is_none()
            || (policy.reserve_swap_return && self.swap_return_bytes.is_none())
        {
            Some(WaitReason::Unknown)
        } else if self.recovery.is_some() {
            Some(WaitReason::Recovery)
        } else if p.drain.iter().any(|id| {
            self.reservations
                .iter()
                .any(|r| r.id == *id || r.continuation.as_ref() == Some(id))
        }) {
            Some(WaitReason::Draining)
        } else if !healthy
            .get(&p.domain)
            .is_some_and(|since| now.saturating_sub(*since) >= policy.resume_ms)
        {
            Some(WaitReason::Pressure)
        } else if p.memory_bytes > policy.budget_bytes.saturating_sub(self.committed()) {
            Some(WaitReason::Budget)
        } else if p.memory_bytes
            > capacity
                .map_or(0, |c| c.available_bytes)
                .saturating_sub(policy.reserve_bytes)
                .saturating_sub(self.committed())
                .saturating_sub(self.return_claim(policy))
        {
            Some(WaitReason::SwapReturn)
        } else if p.swap_bytes
            > capacity
                .map_or(0, |c| c.swap_free_bytes)
                .saturating_sub(policy.swap_reserve_bytes)
                .saturating_sub(self.swap_committed())
        {
            Some(WaitReason::SwapHeadroom)
        } else {
            match self
                .native_claims(policy, None)
                .and_then(|claims| ancestry(&candidate, &claims))
            {
                None => Some(WaitReason::Unknown),
                Some(bytes) if bytes < p.memory_bytes => Some(WaitReason::AncestorHeadroom),
                Some(_) => None,
            }
        };
        let p = &mut self.preparations[index];
        p.waiting = waiting;
        if waiting.is_none() {
            p.phase = PreparationPhase::Ready;
            p.expires_ms = now.saturating_add(p.ready_ms);
        }
    }
}
