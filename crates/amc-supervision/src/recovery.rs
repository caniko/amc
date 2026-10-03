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
    pub identity: Identity,
    pub phase: Phase,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub domain: String,
    pub unix_ms: u64,
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
            self.boot_id
                .bytes()
                .enumerate()
                .all(|(i, b)| if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }),
            "invalid recovery boot identity"
        );
        ensure!(
            self.accounting_window_ms <= 86_400_000,
            "invalid persisted recovery window"
        );
        for attempt in &self.attempts {
            ensure!(
                amc_admission::ledger::valid_name(&attempt.domain)
                    && attempt.unix_ms <= self.last_unix_ms,
                "invalid recovery history"
            );
        }
        if let Some(active) = &self.active {
            active.domain.validate()?;
            let i = &active.identity;
            ensure!(
                i.invocation.len() == 32
                    && i.invocation.bytes().all(|b| b.is_ascii_hexdigit())
                    && i.pid > 0
                    && i.start_ticks > 0
                    && i.inode > 0,
                "invalid persisted recovery identity"
            );
            amc_admission::native::cgroup_directory(&i.cgroup)?;
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
        ensure!(
            self.attempts.len() < policy.host_recovery_limit
                && self
                    .attempts
                    .iter()
                    .filter(|a| a.domain == domain.id)
                    .count()
                    < policy.domain_recovery_limit,
            "recovery budget exhausted"
        );
        self.attempts.push_back(Attempt {
            domain: domain.id.clone(),
            unix_ms,
        });
        self.active = Some(Recovery {
            domain,
            identity,
            phase: Phase::Terminating {
                deadline_ms: ms.saturating_add(policy.term_ms),
            },
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
            Phase::Starting { .. } if started && healthy => {
                self.active = None;
                return Action::Finished;
            }
            Phase::Starting { deadline_ms } if ms >= deadline_ms => {
                active.phase = Phase::Tripped;
                return Action::Trip;
            }
            _ => (),
        }
        Action::None
    }
}
