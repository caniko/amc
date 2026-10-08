//! Finite completion rights for already-admitted operations. A capability is
//! bounded by user, child domains, ceilings and number of distinct native calls.
//! Each selected child domain has one backed lane. Running children transfer
//! its escrow; nested calls into a different domain can still finish a drain.
use crate::host::{HostLedger, HostPolicy, Reservation};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEnvelope {
    pub memory_bytes: u64,
    pub swap_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationPolicy {
    pub parent_max_bytes: u64,
    pub memory_bytes: u64,
    pub swap_bytes: u64,
    pub max_calls: usize,
    pub domains: Vec<String>,
    /// Optional smaller per-domain lanes, frozen when the parent is admitted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub envelopes: BTreeMap<String, CompletionEnvelope>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationCapability {
    pub parent: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Continuation {
    pub capability: ContinuationCapability,
    pub uid: u32,
    pub policy: ContinuationPolicy,
    pub calls: Vec<String>,
}

impl ContinuationPolicy {
    pub fn envelope(&self, domain: &str) -> CompletionEnvelope {
        self.envelopes
            .get(domain)
            .copied()
            .unwrap_or(CompletionEnvelope {
                memory_bytes: self.memory_bytes,
                swap_bytes: self.swap_bytes,
            })
    }

    fn escrow(&self, swap: bool) -> u64 {
        self.domains.iter().fold(0u64, |sum, domain| {
            let limit = self.envelope(domain);
            sum.saturating_add(if swap {
                limit.swap_bytes
            } else {
                limit.memory_bytes
            })
        })
    }

    pub fn validate(&self, policy: &HostPolicy) -> Result<()> {
        ensure!(
            self.parent_max_bytes > 0
                && self.parent_max_bytes <= i64::MAX as u64
                && self.memory_bytes > 0
                && self.memory_bytes <= policy.budget_bytes
                && self.swap_bytes <= i64::MAX as u64
                && (1..=64).contains(&self.max_calls)
                && (1..=16).contains(&self.domains.len())
                && self
                    .domains
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == self.domains.len()
                && self.envelopes.iter().all(|(domain, limit)| {
                    self.domains.contains(domain)
                        && limit.memory_bytes > 0
                        && limit.memory_bytes <= self.memory_bytes
                        && limit.swap_bytes <= self.swap_bytes
                })
                && self
                    .escrow(false)
                    .checked_add(self.parent_max_bytes)
                    .is_some_and(|bytes| bytes <= policy.budget_bytes)
                && self.domains.iter().all(|name| {
                    let limit = self.envelope(name);
                    policy.domains.iter().any(|d| {
                        &d.name == name
                            && !d.burst
                            && limit.memory_bytes <= d.ceiling_bytes
                            && limit.swap_bytes <= d.swap_bytes
                            && (d.uid != 0
                                || (limit.memory_bytes == d.ceiling_bytes
                                    && limit.swap_bytes == d.swap_bytes))
                    })
                }),
            "invalid completion rights"
        );
        Ok(())
    }
}

impl HostLedger {
    pub(crate) fn completion_spec<'a>(
        &self,
        r: &Reservation,
        policy: &'a HostPolicy,
    ) -> Option<&'a ContinuationPolicy> {
        if r.continuation.is_some() || r.burst {
            return None;
        }
        policy
            .domains
            .iter()
            .find(|d| d.name == r.domain)
            .and_then(|d| d.continuation.as_ref())
            .filter(|c| r.memory_bytes <= c.parent_max_bytes)
    }

    pub(crate) fn completion_escrow(&self, swap: bool) -> u64 {
        self.continuations.iter().fold(0u64, |sum, c| {
            sum.saturating_add(c.policy.domains.iter().fold(0u64, |total, domain| {
                let limit = c.policy.envelope(domain);
                let used = self
                    .reservations
                    .iter()
                    .filter(|r| {
                        r.granted
                            && r.domain == *domain
                            && r.continuation.as_deref() == Some(&c.capability.parent)
                    })
                    .fold(0u64, |used, r| {
                        used.saturating_add(if swap { r.swap_bytes } else { r.memory_bytes })
                    });
                total.saturating_add(
                    (if swap {
                        limit.swap_bytes
                    } else {
                        limit.memory_bytes
                    })
                    .saturating_sub(used),
                )
            }))
        })
    }

    pub(crate) fn admission_charge(&self, r: &Reservation, policy: &HostPolicy, swap: bool) -> u64 {
        if self.is_continuation(r) {
            return 0;
        }
        (if swap { r.swap_bytes } else { r.memory_bytes }).saturating_add(
            self.completion_spec(r, policy)
                .map_or(0, |c| c.escrow(swap)),
        )
    }

    pub(crate) fn completion_slot_busy(&self, r: &Reservation) -> bool {
        self.is_continuation(r)
            && self.reservations.iter().any(|other| {
                other.granted
                    && other.id != r.id
                    && other.domain == r.domain
                    && other.continuation == r.continuation
            })
    }

    fn lane(
        &self,
        parent: &str,
        domain: &str,
        memory: u64,
        swap: u64,
        policy: &HostPolicy,
    ) -> Option<Reservation> {
        let d = policy.domains.iter().find(|d| d.name == domain)?;
        Some(Reservation {
            id: format!("escrow-{parent}-{domain}"),
            domain: domain.into(),
            identity: crate::host::Identity {
                cgroup: d.cgroup.clone(),
                inode: 0,
                uid: d.uid,
                pid: 0,
                start_ticks: 0,
            },
            memory_bytes: memory,
            swap_bytes: swap.min(d.swap_bytes),
            requested_ms: 0,
            deadline_ms: u64::MAX,
            granted: true,
            owners: vec![],
            owners_finished: false,
            burst: false,
            runtime_max_ms: None,
            continuation: None,
        })
    }

    /// Native-only projections of escrow, never persisted or returned as real
    /// grants. Subtract this child's lane transfer before checking its ancestors.
    pub fn native_claims(
        &self,
        policy: &HostPolicy,
        child: Option<&Reservation>,
    ) -> Option<Vec<Reservation>> {
        let mut claims = self.reservations.clone();
        claims.extend(crate::namespace_runner::claims(policy));
        for preparation in self
            .preparations
            .iter()
            .filter(|p| p.phase == crate::preparation::PreparationPhase::Ready)
        {
            // Consume transfers this very claim into its own native boundary.
            if child.is_some_and(|r| r.id == preparation.id) {
                continue;
            }
            let mut claim = self.lane(
                &preparation.id,
                &preparation.domain,
                preparation.memory_bytes,
                preparation.swap_bytes,
                policy,
            )?;
            claim.id = preparation.id.clone();
            claims.push(claim);
        }
        for c in &self.continuations {
            for domain in &c.policy.domains {
                let limit = c.policy.envelope(domain);
                let used = self
                    .reservations
                    .iter()
                    .filter(|r| {
                        r.granted
                            && &r.domain == domain
                            && r.continuation.as_deref() == Some(&c.capability.parent)
                    })
                    .fold((0u64, 0u64), |(memory, swap), r| {
                        (
                            memory.saturating_add(r.memory_bytes),
                            swap.saturating_add(r.swap_bytes),
                        )
                    });
                let transfer = child.filter(|r| {
                    self.is_continuation(r)
                        && &r.domain == domain
                        && r.continuation.as_deref() == Some(&c.capability.parent)
                });
                let memory = limit
                    .memory_bytes
                    .saturating_sub(used.0)
                    .saturating_sub(transfer.map_or(0, |r| r.memory_bytes));
                let swap = limit
                    .swap_bytes
                    .saturating_sub(used.1)
                    .saturating_sub(transfer.map_or(0, |r| r.swap_bytes));
                if memory > 0 || swap > 0 {
                    claims.push(self.lane(&c.capability.parent, domain, memory, swap, policy)?);
                }
            }
        }
        Some(claims)
    }

    pub(crate) fn completion_ancestry_wait(
        &self,
        r: &Reservation,
        policy: &HostPolicy,
        ancestry: &mut impl FnMut(&Reservation, &[Reservation]) -> Option<u64>,
    ) -> Option<crate::host::WaitReason> {
        use crate::host::WaitReason;
        let Some(mut claims) = self.native_claims(policy, Some(r)) else {
            return Some(WaitReason::Unknown);
        };
        let mut lanes = vec![];
        if let Some(c) = self.completion_spec(r, policy) {
            for domain in &c.domains {
                let limit = c.envelope(domain);
                let Some(lane) =
                    self.lane(&r.id, domain, limit.memory_bytes, limit.swap_bytes, policy)
                else {
                    return Some(WaitReason::Unknown);
                };
                lanes.push(lane);
            }
        }
        let first_lane = claims.len();
        claims.extend(lanes.iter().cloned());
        match ancestry(r, &claims) {
            None => return Some(WaitReason::Unknown),
            Some(bytes) if r.memory_bytes > bytes => return Some(WaitReason::AncestorHeadroom),
            _ => {}
        }
        let mut parent = r.clone();
        parent.granted = true;
        claims.push(parent);
        for (index, lane) in lanes.iter().enumerate() {
            let others: Vec<_> = claims
                .iter()
                .enumerate()
                .filter(|(position, _)| *position != first_lane + index)
                .map(|(_, claim)| claim.clone())
                .collect();
            match ancestry(lane, &others) {
                None => return Some(WaitReason::Unknown),
                Some(bytes) if lane.memory_bytes > bytes => {
                    return Some(WaitReason::AncestorHeadroom);
                }
                _ => {}
            }
        }
        None
    }

    pub fn continuation_capability(
        &mut self,
        id: &str,
        policy: &HostPolicy,
    ) -> Result<Option<ContinuationCapability>> {
        let r = self
            .reservations
            .iter()
            .find(|r| r.id == id && r.granted)
            .ok_or_else(|| anyhow::anyhow!("no granted operation"))?;
        if let Some(parent) = &r.continuation {
            return Ok(self
                .continuations
                .iter()
                .find(|c| c.capability.parent == *parent)
                .map(|c| c.capability.clone()));
        }
        if let Some(c) = self
            .continuations
            .iter()
            .find(|c| c.capability.parent == id)
        {
            return Ok(Some(c.capability.clone()));
        }
        let _ = policy;
        // Old grants cannot acquire unbacked completion rights on policy reload.
        Ok(None)
    }

    pub(crate) fn mint_completion(&mut self, id: &str, policy: &HostPolicy) -> Result<()> {
        let r = self
            .reservations
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| anyhow::anyhow!("missing operation"))?;
        let Some(spec) = self.completion_spec(r, policy) else {
            return Ok(());
        };
        let c = Continuation {
            capability: ContinuationCapability {
                parent: id.into(),
                key: crate::store::fresh_id()?,
            },
            uid: r.identity.uid,
            policy: spec.clone(),
            calls: vec![],
        };
        self.transaction(|candidate| {
            candidate.continuations.push(c);
            Ok(())
        })
    }

    pub fn authorize_continuation(
        &mut self,
        capability: &ContinuationCapability,
        uid: u32,
        domain: &str,
        memory: u64,
        swap: u64,
        call: &str,
    ) -> Result<String> {
        let c = self
            .continuations
            .iter_mut()
            .find(|c| {
                c.capability.parent == capability.parent && c.capability.key == capability.key
            })
            .ok_or_else(|| anyhow::anyhow!("unknown completion capability"))?;
        let limit = c.policy.envelope(domain);
        ensure!(
            uid == c.uid
                && (c.calls.iter().any(|id| id == call)
                    || self.reservations.iter().any(|r| r.granted
                        && (r.id == capability.parent
                            || r.continuation.as_ref() == Some(&capability.parent))))
                && c.policy.domains.iter().any(|d| d == domain)
                && memory <= limit.memory_bytes
                && swap <= limit.swap_bytes,
            "completion is outside its original operation contract"
        );
        if !c.calls.iter().any(|id| id == call) {
            ensure!(
                c.calls.len() < c.policy.max_calls,
                "operation completion allowance exhausted"
            );
            c.calls.push(call.into());
        }
        Ok(capability.parent.clone())
    }

    pub fn is_continuation(&self, r: &Reservation) -> bool {
        r.continuation.as_ref().is_some_and(|parent| {
            self.continuations.iter().any(|c| {
                let limit = c.policy.envelope(&r.domain);
                c.capability.parent == *parent
                    && c.policy.domains.contains(&r.domain)
                    && r.memory_bytes <= limit.memory_bytes
                    && r.swap_bytes <= limit.swap_bytes
                    && (r.identity.uid == c.uid || r.identity.uid == 0)
                    && (c.calls.contains(&r.id)
                        || std::iter::once((r.identity.pid, r.identity.start_ticks))
                            .chain(r.owners.iter().map(|o| (o.pid, o.start_ticks)))
                            .any(|(pid, start)| {
                                let prefix = format!("owner-{pid}-{start}");
                                c.calls.iter().any(|call| {
                                    call == &prefix || call.starts_with(&format!("{prefix}-"))
                                })
                            }))
            })
        })
    }

    pub(crate) fn retain_continuations(&mut self) {
        self.continuations.retain(|c| {
            self.reservations.iter().any(|r| {
                (r.granted && r.id == c.capability.parent)
                    || r.continuation.as_ref() == Some(&c.capability.parent)
            })
        });
    }
}
