//! Persist-before-actuate recovery accounting; one host-wide recovery at a time.
use crate::policy::{Domain, Lifecycle, Policy};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub invocation: String,
    pub cgroup: String,
    pub inode: u64,
    pub pid: i32,
    pub start_ticks: u64,
}

/// A cold-discovered failure has no live process to pin or signal. Bind it to
/// the retained native invocation and independently verified empty service slot.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FailedIdentity {
    pub invocation: String,
    pub parent: String,
    pub parent_inode: u64,
    pub cgroup: String,
    pub inode: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RecoveryIdentity {
    Process(Identity),
    Failed(FailedIdentity),
}

impl RecoveryIdentity {
    pub fn invocation(&self) -> &str {
        match self {
            Self::Process(i) => &i.invocation,
            Self::Failed(i) => &i.invocation,
        }
    }
}

impl From<Identity> for RecoveryIdentity {
    fn from(identity: Identity) -> Self {
        Self::Process(identity)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum Phase {
    Terminating { deadline_ms: u64 },
    Killing { deadline_ms: u64 },
    Cooling { until_ms: u64, deadline_ms: u64 },
    Starting { deadline_ms: u64 },
    Tripped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub domain: Domain,
    pub identity: RecoveryIdentity,
    pub phase: Phase,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub domain: String,
    /// Stable enrolled pool, independent of the disposable ticket identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    pub unix_ms: u64,
}

// Migrate pre-pool accounting without forgiving already consumed attempts.
// Accept published normalized pool names and canonical pool + UUID identities,
// not arbitrary prefixes.
fn legacy_pool<'a>(id: &str, policy: &'a Policy) -> Option<&'a str> {
    if policy.domains.iter().any(|domain| domain.id == id) {
        return None;
    }
    policy.job_pools.iter().find_map(|pool| {
        if id == pool.id {
            return Some(pool.id.as_str());
        }
        let ticket = id.strip_prefix(&format!("{}-", pool.id))?;
        valid_boot_id(ticket).then_some(pool.id.as_str())
    })
}

fn valid_boot_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub boot_id: String,
    pub attempts: VecDeque<Attempt>,
    pub last_unix_ms: u64,
    pub accounting_window_ms: u64,
    pub active: Option<Recovery>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Kill,
    Start,
    Finished,
    Trip,
}

impl State {
    pub fn new(boot_id: String) -> Self {
        Self {
            version: 1,
            boot_id,
            attempts: VecDeque::new(),
            last_unix_ms: 0,
            accounting_window_ms: 0,
            active: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.boot_id.len() == 36 && self.attempts.len() <= 64,
            "invalid recovery state"
        );
        ensure!(
            valid_boot_id(&self.boot_id),
            "invalid recovery boot identity"
        );
        ensure!(
            self.accounting_window_ms <= 86_400_000,
            "invalid persisted recovery window"
        );
        for attempt in &self.attempts {
            ensure!(
                amc_admission::ledger::valid_name(&attempt.domain)
                    && attempt
                        .pool
                        .as_ref()
                        .is_none_or(|p| amc_admission::ledger::valid_name(p) && p.len() <= 31)
                    && attempt.unix_ms <= self.last_unix_ms,
                "invalid recovery history"
            );
        }
        if let Some(active) = &self.active {
            active.domain.validate()?;
            let invocation = active.identity.invocation();
            ensure!(
                invocation.len() == 32 && invocation.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid persisted recovery identity"
            );
            match &active.identity {
                RecoveryIdentity::Process(i) => {
                    ensure!(
                        i.pid > 0 && i.start_ticks > 0 && i.inode > 0,
                        "invalid persisted process identity"
                    );
                    amc_admission::native::cgroup_directory(&i.cgroup)?;
                }
                RecoveryIdentity::Failed(i) => {
                    ensure!(
                        active.domain.lifecycle == Lifecycle::Restart
                            && active.domain.expected.is_none()
                            && !matches!(
                                active.phase,
                                Phase::Terminating { .. } | Phase::Killing { .. }
                            )
                            && i.parent_inode > 0
                            && i.inode != Some(0)
                            && i.cgroup == format!("{}/{}", i.parent, active.domain.unit),
                        "invalid persisted failed identity"
                    );
                    amc_admission::native::cgroup_directory(&i.parent)?;
                    amc_admission::native::cgroup_directory(&i.cgroup)?;
                }
            }
        }
        Ok(())
    }

    /// Supervisor reboot/restart cannot automatically replay a recovery. A
    /// persisted in-flight attempt trips until operator reconciliation; counters
    /// survive boot and policy changes. A policy edit cannot forgive a restart.
    pub fn reconcile_startup(&mut self, boot_id: &str) {
        if let Some(active) = &mut self.active {
            active.phase = Phase::Tripped;
        }
        self.boot_id = boot_id.into();
    }

    pub fn begin(
        &mut self,
        domain: Domain,
        identity: Identity,
        unix_ms: u64,
        ms: u64,
        policy: &Policy,
    ) -> Result<()> {
        self.begin_with_identity(
            domain,
            identity.into(),
            unix_ms,
            policy,
            Phase::Terminating {
                deadline_ms: ms.saturating_add(policy.term_ms),
            },
        )
    }

    /// Native failure evidence already includes an empty original service slot.
    /// It enters cooldown directly and never acquires TERM/KILL authority.
    pub fn begin_failed(
        &mut self,
        domain: Domain,
        identity: FailedIdentity,
        unix_ms: u64,
        ms: u64,
        policy: &Policy,
    ) -> Result<()> {
        ensure!(
            domain.lifecycle == Lifecycle::Restart && domain.expected.is_none(),
            "only dedicated failed backends may recover"
        );
        self.begin_with_identity(
            domain,
            RecoveryIdentity::Failed(identity),
            unix_ms,
            policy,
            Phase::Cooling {
                until_ms: ms.saturating_add(policy.cooldown_ms),
                deadline_ms: ms.saturating_add(policy.cooldown_ms).saturating_add(60_000),
            },
        )
    }

    fn begin_with_identity(
        &mut self,
        domain: Domain,
        identity: RecoveryIdentity,
        unix_ms: u64,
        policy: &Policy,
        phase: Phase,
    ) -> Result<()> {
        ensure!(
            self.active.is_none(),
            "another recovery owns host intervention"
        );
        ensure!(
            domain.lifecycle != Lifecycle::Observe,
            "domain lacks recovery authority"
        );
        ensure!(
            unix_ms >= self.last_unix_ms,
            "wall clock moved backwards; recovery inhibited"
        );
        self.last_unix_ms = unix_ms;
        // Never shorten the accounting window on policy reload.
        self.accounting_window_ms = self.accounting_window_ms.max(policy.recovery_window_ms);
        self.attempts
            .retain(|a| unix_ms.saturating_sub(a.unix_ms) <= self.accounting_window_ms);
        let pool = if domain.expected.is_some() {
            let ticket = domain
                .unit
                .strip_prefix("app-amc-job-")
                .and_then(|u| u.strip_suffix(".service"));
            let pool = legacy_pool(&domain.id, policy);
            ensure!(
                pool.is_some_and(|id| {
                    ticket.is_some_and(|ticket| domain.id == format!("{id}-{ticket}"))
                        && policy.job_pools.iter().any(|p| {
                            p.id == id
                                && domain.uid == Some(p.uid)
                                && domain.lifecycle == p.lifecycle
                        })
                }),
                "job lacks enrolled recovery pool"
            );
            pool
        } else {
            None
        };
        for attempt in &mut self.attempts {
            if attempt.pool.is_none() {
                attempt.pool = legacy_pool(&attempt.domain, policy).map(str::to_owned);
            }
        }
        ensure!(
            self.attempts.len() < policy.host_recovery_limit
                && self
                    .attempts
                    .iter()
                    .filter(|a| match pool {
                        Some(pool) => a.pool.as_deref() == Some(pool),
                        None => a.pool.is_none() && a.domain == domain.id,
                    })
                    .count()
                    < policy.domain_recovery_limit,
            "recovery budget exhausted"
        );
        self.attempts.push_back(Attempt {
            domain: domain.id.clone(),
            pool: pool.map(str::to_owned),
            unix_ms,
        });
        self.active = Some(Recovery {
            domain,
            identity,
            phase,
        });
        Ok(())
    }

    /// `empty` requires original cgroup termination plus native inactive/failed.
    /// A replacement invocation while stopping is an ambiguity, never success.
    pub fn advance(
        &mut self,
        ms: u64,
        empty: Option<bool>,
        replacement: bool,
        healthy: bool,
        started: bool,
        policy: &Policy,
    ) -> Action {
        let Some(active) = &mut self.active else {
            return Action::None;
        };
        if replacement && !matches!(active.phase, Phase::Starting { .. }) {
            active.phase = Phase::Tripped;
            return Action::Trip;
        }
        if matches!(active.phase, Phase::Cooling { .. }) && empty != Some(true) {
            active.phase = Phase::Tripped;
            return Action::Trip;
        }
        match active.phase {
            Phase::Terminating { .. } | Phase::Killing { .. } if empty == Some(true) => {
                active.phase = Phase::Cooling {
                    until_ms: ms.saturating_add(policy.cooldown_ms),
                    deadline_ms: ms.saturating_add(policy.cooldown_ms).saturating_add(60_000),
                };
            }
            Phase::Terminating { deadline_ms } if ms >= deadline_ms => {
                active.phase = Phase::Killing {
                    deadline_ms: ms.saturating_add(policy.kill_ms),
                };
                return Action::Kill;
            }
            Phase::Killing { deadline_ms } if ms >= deadline_ms => {
                active.phase = Phase::Tripped;
                return Action::Trip;
            }
            Phase::Cooling { deadline_ms, .. } if ms >= deadline_ms => {
                active.phase = Phase::Tripped;
                return Action::Trip;
            }
            Phase::Cooling { deadline_ms, .. }
                if !healthy && active.domain.lifecycle == Lifecycle::Restart =>
            {
                active.phase = Phase::Cooling {
                    until_ms: ms.saturating_add(policy.cooldown_ms),
                    deadline_ms,
                };
            }
            Phase::Cooling { until_ms, .. } if ms >= until_ms => {
                if active.domain.lifecycle == Lifecycle::Restart {
                    active.phase = Phase::Starting {
                        deadline_ms: ms.saturating_add(10_000),
                    };
                    return Action::Start;
                }
                self.active = None;
                return Action::Finished;
            }
            Phase::Starting { deadline_ms } if ms >= deadline_ms => {
                active.phase = Phase::Tripped;
                return Action::Trip;
            }
            Phase::Starting { .. } if started && healthy => {
                self.active = None;
                return Action::Finished;
            }
            _ => (),
        }
        Action::None
    }
}
