use crate::forecast::Settings;
use amc_admission::ledger::valid_name;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Shadow,
    Enforce,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Dedicated backend; systemd Restart=no and KillMode=control-group required.
    Restart,
    /// One disposable invocation. Never replay its command or call start.
    Terminate,
    /// Broker or other domain without an authorized cancellation interface.
    Observe,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Domain {
    pub id: String,
    pub uid: Option<u32>,
    pub unit: String,
    pub lifecycle: Lifecycle,
    pub memory_max: u64,
    pub memory_swap_max: u64,
    /// Recovery preference: disposable work should precede interactive backends.
    pub priority: u32,
    /// For admitted jobs, retain the ledger's invocation/cgroup identity.
    #[serde(default)]
    pub expected: Option<amc_admission::ledger::Identity>,
}

impl Domain {
    pub fn validate(&self) -> Result<()> {
        ensure!(valid_name(&self.id), "invalid supervision domain id");
        ensure!(
            self.unit.ends_with(".service")
                && self.unit.len() <= 128
                && self
                    .unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b)),
            "invalid supervised unit"
        );
        ensure!(
            (self.memory_max > 0 || self.lifecycle == Lifecycle::Observe)
                && self.memory_max <= i64::MAX as u64
                && self.memory_swap_max <= i64::MAX as u64,
            "invalid supervision ceiling"
        );
        if let Some(expected) = &self.expected {
            ensure!(
                self.lifecycle != Lifecycle::Restart
                    && expected.invocation.len() == 32
                    && expected.invocation.bytes().all(|b| b.is_ascii_hexdigit())
                    && expected.inode > 0,
                "invalid expected job invocation"
            );
            amc_admission::native::cgroup_directory(&expected.cgroup)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobPool {
    pub id: String,
    pub uid: u32,
    /// Private admission state; supervisor reads but never releases reservations.
    pub state: PathBuf,
    pub contracts: Vec<String>,
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub priority: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub mode: Mode,
    /// Separate rollout gate: statistical actuation needs chronological/shadow evidence.
    pub forecast_recovery: bool,
    pub reserve_bytes: u64,
    pub emergency_available_bytes: u64,
    pub emergency_full_psi: f64,
    pub term_ms: u64,
    pub kill_ms: u64,
    pub cooldown_ms: u64,
    pub recovery_window_ms: u64,
    pub domain_recovery_limit: usize,
    pub host_recovery_limit: usize,
    pub forecast: Settings,
    pub domains: Vec<Domain>,
    pub job_pools: Vec<JobPool>,
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported supervision policy");
        self.forecast.validate()?;
        ensure!(
            self.reserve_bytes > self.emergency_available_bytes
                && self.emergency_available_bytes > 0,
            "invalid emergency reserve"
        );
        ensure!(
            self.emergency_full_psi.is_finite() && (0.001..=1.0).contains(&self.emergency_full_psi),
            "invalid emergency PSI fraction"
        );
        ensure!(
            (100..=30_000).contains(&self.term_ms) && (100..=30_000).contains(&self.kill_ms),
            "invalid recovery deadline"
        );
        ensure!(
            (1000..=300_000).contains(&self.cooldown_ms)
                && (60_000..=86_400_000).contains(&self.recovery_window_ms),
            "invalid recovery cooldown/window"
        );
        ensure!(
            (1..=16).contains(&self.domain_recovery_limit)
                && (1..=64).contains(&self.host_recovery_limit),
            "invalid recovery budget"
        );
        ensure!(
            self.forecast.horizon as u64 * 1000 >= self.term_ms + self.kill_ms + 3000,
            "forecast window cannot cover recovery and observation latency"
        );
        ensure!(
            self.domains.len() <= 32
                && self.job_pools.len() <= 8
                && !(self.domains.is_empty() && self.job_pools.is_empty()),
            "invalid supervision coverage size"
        );
        let mut ids = BTreeSet::new();
        let mut units = BTreeSet::new();
        for d in &self.domains {
            d.validate()?;
            ensure!(
                ids.insert(d.id.clone()) && units.insert((d.uid, d.unit.clone())),
                "duplicate domain or native boundary"
            );
        }
        for pool in &self.job_pools {
            ensure!(
                valid_name(&pool.id)
                    && pool.id.len() <= 31
                    && pool.lifecycle != Lifecycle::Restart
                    && ids.insert(pool.id.clone())
                    && pool.state.is_absolute()
                    && pool.state.as_os_str().len() <= 4096
                    && !pool.contracts.is_empty()
                    && pool.contracts.len() <= 32
                    && pool.contracts.iter().all(|c| valid_name(c)),
                "invalid job pool"
            );
        }
        Ok(())
    }
}
