//! Request mutations must commit together with their finite completion rights.
use crate::host::HostLedger;
use anyhow::{Result, ensure};

// Tick-owned expiry/recovery telemetry must still fit after admission stops.
const STATE_GROWTH_LIMIT: usize = crate::store::MAX_STATE_BYTES as usize - 16 * 1024;

impl HostLedger {
    pub(crate) fn transaction<T>(
        &mut self,
        change: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let mut candidate = self.clone();
        let reply = change(&mut candidate)?;
        candidate.validate()?;
        let after = serde_json::to_vec(&candidate)?.len();
        ensure!(
            after <= STATE_GROWTH_LIMIT || after <= serde_json::to_vec(self)?.len(),
            "host admission state headroom exhausted"
        );
        *self = candidate;
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        continuation::{Continuation, ContinuationCapability, ContinuationPolicy},
        host::{Capacity, HostPolicy, Reservation},
    };
    use std::collections::BTreeMap;

    #[test]
    fn a_rejected_enqueue_cannot_consume_an_operations_only_completion_call() {
        let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":100,"reserve_bytes":10,"swap_reserve_bytes":0,
            "max_memory_full_psi":100.0,"max_io_full_psi":100.0,
            "resume_ms":250,"aging_ms":1000,"queue_limit":1,
            "domains":[{"name":"work","uid":1000,"cgroup":"/work.slice",
                "ceiling_bytes":50,"swap_bytes":0,"fair_share_bytes":50,
                "continuation":{"parent_max_bytes":10,"memory_bytes":20,
                    "swap_bytes":0,"max_calls":1,"domains":["work"]}}]
        }))
        .unwrap();
        policy.validate().unwrap();
        let parent: Reservation = serde_json::from_value(serde_json::json!({
            "id":"parent","domain":"work","identity":{"uid":1000,"pid":42,
                "start_ticks":7,"inode":1,"cgroup":"/work.slice/app-amc-job-parent.service"},
            "memory_bytes":10,"swap_bytes":0,"requested_ms":0,"deadline_ms":10000,
            "granted":false,"owners":[]
        }))
        .unwrap();
        let mut ledger = HostLedger::new("boot".into());
        ledger.request(parent.clone(), &policy).unwrap();
        ledger.advance(
            250,
            &policy,
            Some(Capacity {
                available_bytes: 200,
                swap_free_bytes: 100,
                memory_full_psi: 0.0,
                io_full_psi: 0.0,
            }),
            &mut BTreeMap::from([("work".into(), 0)]),
            |_, _| Some(200),
        );
        let capability = ledger
            .continuation_capability("parent", &policy)
            .unwrap()
            .unwrap();
        let before = serde_json::to_vec(&ledger).unwrap();
        let enqueue = |ledger: &mut HostLedger, id: &str, policy: &HostPolicy| {
            let mut child = parent.clone();
            child.id = id.into();
            child.identity.inode = 2;
            child.identity.cgroup = format!("/work.slice/app-amc-job-{id}.service");
            child.memory_bytes = 20;
            child.continuation =
                Some(ledger.authorize_continuation(&capability, 1000, "work", 20, 0, id)?);
            ledger.request(child, policy)
        };
        assert!(
            ledger
                .transaction(|candidate| enqueue(candidate, "rejected", &policy))
                .is_err()
        );
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
        policy.queue_limit = 2;
        assert_eq!(
            ledger
                .transaction(|candidate| enqueue(candidate, "retry", &policy))
                .unwrap(),
            "retry"
        );
        assert_eq!(ledger.continuations[0].calls, ["retry"]);
        assert!(ledger.reservations.iter().any(|r| r.id == "retry"));
    }

    fn large_state() -> HostLedger {
        let mut ledger = HostLedger::new("boot".into());
        for index in 0..192 {
            ledger.continuations.push(Continuation {
                capability: ContinuationCapability {
                    parent: format!("parent-{index}"),
                    key: "key".into(),
                },
                uid: 1000,
                policy: ContinuationPolicy {
                    parent_max_bytes: 1,
                    memory_bytes: 1,
                    swap_bytes: 0,
                    max_calls: 64,
                    domains: (0..16).map(|i| format!("d{i:079}")).collect(),
                },
                calls: (0..64).map(|i| format!("c{i:079}")).collect(),
            });
        }
        ledger
    }

    #[test]
    fn aggregate_completion_state_must_fit_the_real_durable_snapshot() {
        let ledger = large_state();
        assert!(serde_json::to_vec(&ledger).unwrap().len() as u64 > crate::store::MAX_STATE_BYTES);
        assert!(ledger.validate().is_err());
    }

    #[test]
    fn excessive_state_growth_cannot_replace_existing_native_obligations() {
        let mut ledger = HostLedger::new("boot".into());
        let before = serde_json::to_vec(&ledger).unwrap();
        assert!(
            ledger
                .transaction(|candidate| {
                    candidate.continuations = large_state().continuations;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
    }

    #[test]
    fn state_headroom_stops_growth_but_still_allows_existing_obligations_to_settle() {
        let mut ledger = large_state();
        while serde_json::to_vec(&ledger).unwrap().len() > STATE_GROWTH_LIMIT {
            ledger.continuations.pop().unwrap();
        }
        ledger.validate().unwrap();
        let mut next = ledger.continuations[0].clone();
        next.capability.parent = "next".into();
        let before = serde_json::to_vec(&ledger).unwrap();
        let mut larger = ledger.clone();
        larger.continuations.push(next.clone());
        assert!(serde_json::to_vec(&larger).unwrap().len() > STATE_GROWTH_LIMIT);
        larger.validate().unwrap();
        assert!(
            ledger
                .transaction(|candidate| {
                    candidate.continuations.push(next);
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(serde_json::to_vec(&ledger).unwrap(), before);
        // A pre-existing snapshot above the admission watermark is retained,
        // and cleanup remains usable rather than requiring a broker reset.
        larger
            .transaction(|candidate| {
                candidate.continuations.pop();
                Ok(())
            })
            .unwrap();
        assert_eq!(serde_json::to_vec(&larger).unwrap(), before);
    }
}
