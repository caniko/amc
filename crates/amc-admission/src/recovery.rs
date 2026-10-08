//! Root-owned bounded page return and explicit whole-device recovery. Capacity belongs to affected pages,
//! not to the tiny helper. No ordinary free-swap gate can inhibit this repair.
use crate::host::{Capacity, HostLedger, HostPolicy, Identity, WaitReason};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub(crate) fn helper_claim(identity: &Identity, bytes: u64) -> crate::host::Reservation {
    crate::host::Reservation {
        id: "swap-recovery".into(),
        domain: "swap-recovery".into(),
        identity: identity.clone(),
        memory_bytes: bytes,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryTarget {
    pub name: String,
    pub path: String,
    pub priority: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPolicy {
    pub cgroup: String,
    pub helper_bytes: u64,
    pub minimum_bytes: u64,
    pub targets: Vec<RecoveryTarget>,
    /// Incremental recovery is restricted to these operator-selected subtrees.
    #[serde(default)]
    pub page_cgroups: Vec<String>,
    #[serde(default = "default_batch_bytes")]
    pub batch_bytes: u64,
}

fn default_batch_bytes() -> u64 {
    2 * 1024 * 1024
}

impl RecoveryPolicy {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_page_size(crate::page_return::page_size()?)
    }

    fn validate_with_page_size(&self, page_size: u64) -> Result<()> {
        canonical_cgroup(&self.cgroup)?;
        ensure!(
            std::path::Path::new(&self.cgroup)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name
                    .strip_suffix(".service")
                    .is_some_and(|stem| !stem.is_empty())),
            "recovery requires a native service boundary"
        );
        ensure!(
            (1..=1_073_741_824).contains(&self.helper_bytes)
                && self.minimum_bytes > 0
                && self.targets.len() <= 16
                && (!self.targets.is_empty() || !self.page_cgroups.is_empty())
                && self.page_cgroups.len() <= 64
                && (4096..=16 * 1024 * 1024).contains(&self.batch_bytes)
                && self.batch_bytes >= page_size,
            "invalid recovery policy"
        );
        validate_targets(&self.targets)?;
        for (index, group) in self.page_cgroups.iter().enumerate() {
            canonical_cgroup(group)?;
            ensure!(
                self.page_cgroups
                    .iter()
                    .enumerate()
                    .all(|(other, prefix)| index == other
                        || !(group == prefix || group.starts_with(&format!("{prefix}/")))),
                "overlapping page return subtrees"
            );
        }
        Ok(())
    }
}

fn canonical_cgroup(group: &str) -> Result<()> {
    crate::native::cgroup_directory(group)?;
    ensure!(
        group
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "recovery requires canonical cgroup paths"
    );
    Ok(())
}

/// Shared by broker policy and the broker-independent restoration manifest.
pub fn validate_targets(targets: &[RecoveryTarget]) -> Result<()> {
    ensure!(targets.len() <= 16, "too many recovery targets");
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for target in targets {
        ensure!(
            crate::ledger::valid_name(&target.name)
                && names.insert(&target.name)
                && paths.insert(&target.path)
                && target.path.starts_with('/')
                && !target.path.chars().any(char::is_whitespace)
                && !target.path.contains('\0')
                && (0..=32767).contains(&target.priority),
            "invalid or duplicate recovery target"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLease {
    pub identity: Identity,
    /// Trusted manager identity; legacy leases retain conservative empty-only cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    pub action: RecoveryAction,
    /// Full device size backs additional swapout by already-running workloads.
    pub return_bytes: u64,
    pub helper_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryAction {
    Device {
        target: RecoveryTarget,
        before_used_bytes: u64,
    },
    Pages {
        target: Identity,
        address: u64,
        bytes: u64,
        before_swap_bytes: u64,
        #[serde(default)]
        before_return_bytes: Option<u64>,
        /// Legacy leases can be cleaned up, but cannot authorize guarded reads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        guards: Option<crate::page_return::PageReturnGuards>,
        settled: bool,
    },
}

impl RecoveryLease {
    pub fn validate(&self) -> Result<()> {
        crate::native::cgroup_directory(&self.identity.cgroup)?;
        ensure!(
            self.invocation_id.as_ref().is_none_or(|id| id.len() == 32
                && id.bytes().all(|b| b.is_ascii_hexdigit())
                && id.bytes().any(|b| b != b'0')),
            "invalid recovery invocation identity"
        );
        ensure!(
            self.identity.uid == 0
                && self.identity.pid > 0
                && self.identity.start_ticks > 0
                && self.identity.inode > 0
                && (self.return_bytes > 0
                    || matches!(self.action, RecoveryAction::Pages { settled: true, .. }))
                && self.return_bytes <= i64::MAX as u64
                && self.helper_bytes > 0,
            "invalid recovery lease"
        );
        let targets = match &self.action {
            RecoveryAction::Device {
                target,
                before_used_bytes,
            } => {
                ensure!(
                    *before_used_bytes <= self.return_bytes,
                    "invalid device return demand"
                );
                vec![target.clone()]
            }
            RecoveryAction::Pages {
                target,
                address,
                bytes,
                before_swap_bytes,
                before_return_bytes,
                settled,
                guards,
                ..
            } => {
                crate::native::cgroup_directory(&target.cgroup)?;
                ensure!(
                    target.pid > 0
                        && target.start_ticks > 0
                        && target.inode > 0
                        && *bytes > 0
                        && (if *settled {
                            self.return_bytes == 0
                        } else {
                            *bytes == self.return_bytes
                        })
                        && *bytes <= 16 * 1024 * 1024
                        && before_return_bytes.is_none_or(|demand| demand <= *before_swap_bytes)
                        && address.checked_add(*bytes).is_some(),
                    "invalid page return demand"
                );
                if let Some(guards) = guards {
                    guards.validate(target, &self.identity)?;
                }
                vec![]
            }
        };
        RecoveryPolicy {
            cgroup: self.identity.cgroup.clone(),
            helper_bytes: self.helper_bytes,
            minimum_bytes: 1,
            targets,
            page_cgroups: vec![self.identity.cgroup.clone()],
            batch_bytes: default_batch_bytes(),
        }
        .validate()
    }
}

impl HostLedger {
    /// A batch consumes existing return debt; backing the entire outstanding
    /// debt here would prevent the repair from ever starting under pressure.
    pub fn page_return_wait(
        &self,
        policy: &HostPolicy,
        capacity: Option<Capacity>,
        batch: u64,
        helper: u64,
        native_safe: bool,
    ) -> Option<WaitReason> {
        let Some(c) = capacity else {
            return Some(WaitReason::Unknown);
        };
        if policy.reserve_swap_return && self.swap_return_bytes.is_none() {
            return Some(WaitReason::Unknown);
        }
        if self.recovery.is_some() {
            return Some(WaitReason::Recovery);
        }
        if !(0.0..policy.max_memory_full_psi).contains(&c.memory_full_psi) {
            return Some(WaitReason::Pressure);
        }
        if !native_safe {
            return Some(WaitReason::AncestorHeadroom);
        }
        if helper > policy.budget_bytes.saturating_sub(self.committed()) {
            return Some(WaitReason::Budget);
        }
        if batch.saturating_add(helper)
            > c.available_bytes
                .saturating_sub(policy.reserve_bytes)
                .saturating_sub(self.committed())
        {
            return Some(WaitReason::HostHeadroom);
        }
        None
    }
}

impl HostLedger {
    pub fn recovery_wait(
        &self,
        policy: &HostPolicy,
        capacity: Option<Capacity>,
        return_bytes: u64,
        helper_bytes: u64,
        native_return_safe: bool,
    ) -> Option<WaitReason> {
        let Some(c) = capacity else {
            return Some(WaitReason::Unknown);
        };
        if policy.reserve_swap_return && self.swap_return_bytes.is_none() {
            return Some(WaitReason::Unknown);
        }
        if self.recovery.is_some() {
            return Some(WaitReason::Recovery);
        }
        if !(0.0..policy.max_memory_full_psi).contains(&c.memory_full_psi) {
            return Some(WaitReason::Pressure);
        }
        if !native_return_safe {
            return Some(WaitReason::AncestorHeadroom);
        }
        if helper_bytes > policy.budget_bytes.saturating_sub(self.committed()) {
            return Some(WaitReason::Budget);
        }
        if return_bytes
            .max(self.return_claim(policy))
            .saturating_add(helper_bytes)
            > c.available_bytes
                .saturating_sub(policy.reserve_bytes)
                .saturating_sub(self.committed())
        {
            return Some(WaitReason::HostHeadroom);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_policy_rejects_noncanonical_paths_and_nonservice_helpers() {
        let policy = RecoveryPolicy {
            cgroup: "/system.slice/amc-recovery.service".into(),
            helper_bytes: 1024 * 1024,
            minimum_bytes: 1,
            targets: vec![],
            page_cgroups: vec!["/work/child".into()],
            batch_bytes: 65536,
        };
        assert!(policy.validate().is_ok());
        for group in ["/work/", "/work//child", "//work/child", "/work/./child"] {
            let mut invalid = policy.clone();
            invalid.page_cgroups = vec![group.into()];
            assert!(invalid.validate().is_err(), "accepted selection {group}");
        }
        for group in [
            "/system.slice/amc-recovery.service/",
            "/system.slice//amc-recovery.service",
            "/system.slice/amc-recovery.scope",
            "/system.slice/amc-recovery.slice",
            "/system.slice/recovery",
            "/system.slice/.service",
        ] {
            let mut invalid = policy.clone();
            invalid.cgroup = group.into();
            assert!(invalid.validate().is_err(), "accepted helper {group}");
        }
    }
    #[test]
    fn recovery_batches_must_cover_a_complete_native_page() {
        let mut policy = RecoveryPolicy {
            cgroup: "/recovery.service".into(),
            helper_bytes: 1024 * 1024,
            minimum_bytes: 1,
            targets: vec![],
            page_cgroups: vec!["/target".into()],
            batch_bytes: 4096,
        };
        assert!(policy.validate_with_page_size(4096).is_ok());
        assert!(policy.validate_with_page_size(65536).is_err());
        policy.batch_bytes = 65536;
        assert!(policy.validate_with_page_size(65536).is_ok());
        policy.batch_bytes = 65535;
        assert!(policy.validate_with_page_size(65536).is_err());
    }

    #[test]
    fn recovery_can_enter_below_swap_floor_but_never_without_real_ram_or_native_proof() {
        let policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":100,"reserve_bytes":20,"swap_reserve_bytes":20,
            "resume_ms":250,"aging_ms":1000,"queue_limit":32,"max_memory_full_psi":1.0,"max_io_full_psi":20.0,
            "domains":[{"name":"work","uid":1000,"cgroup":"/work","ceiling_bytes":50,"swap_bytes":0,"fair_share_bytes":50}]
        })).unwrap();
        let ledger = HostLedger::new("boot".into());
        let c = Capacity {
            available_bytes: 100,
            swap_free_bytes: 0,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        };
        assert_eq!(ledger.recovery_wait(&policy, Some(c), 50, 10, true), None);
        assert_eq!(
            ledger.recovery_wait(&policy, Some(c), 80, 10, true),
            Some(WaitReason::HostHeadroom)
        );
        assert_eq!(
            ledger.recovery_wait(&policy, Some(c), 50, 10, false),
            Some(WaitReason::AncestorHeadroom)
        );
        assert_eq!(
            ledger.recovery_wait(&policy, None, 50, 10, true),
            Some(WaitReason::Unknown)
        );
    }
    #[test]
    fn repair_backs_a_finite_chunk_even_when_the_complete_debt_cannot_fit() {
        let policy:HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":100,"reserve_bytes":20,"swap_reserve_bytes":20,
            "resume_ms":250,"aging_ms":1000,"queue_limit":32,"max_memory_full_psi":1.0,"max_io_full_psi":20.0,
            "reserve_swap_return":true,
            "domains":[{"name":"work","uid":1000,"cgroup":"/work","ceiling_bytes":50,"swap_bytes":0,"fair_share_bytes":50}]
        })).unwrap();
        let mut l = HostLedger::new("boot".into());
        l.swap_return_bytes = Some(1000);
        let capacity = Capacity {
            available_bytes: 100,
            swap_free_bytes: 0,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        };
        assert_eq!(
            l.recovery_wait(&policy, Some(capacity), 1000, 10, true),
            Some(WaitReason::HostHeadroom)
        );
        assert_eq!(
            l.page_return_wait(&policy, Some(capacity), 20, 10, true),
            None
        );
        assert_eq!(
            l.page_return_wait(&policy, Some(capacity), 80, 10, true),
            Some(WaitReason::HostHeadroom)
        );
        assert_eq!(
            l.page_return_wait(&policy, Some(capacity), 20, 10, false),
            Some(WaitReason::AncestorHeadroom)
        );
    }
}
