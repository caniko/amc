use amc_admission::ledger::{Ledger, Phase, Policy};
use serde_json::json;

fn policy() -> Policy {
    serde_json::from_value(json!({
        "version": 1, "budget_bytes": 100, "reserve_bytes": 20, "queue_limit": 1,
        "contracts": {
            "tool": {"slice": "agent-tools.slice", "memory_max": 80,
                     "memory_swap_max": 0, "max_running": 1, "pause_file": null},
            "quick": {"slice": "agent-quick.slice", "memory_max": 10,
                      "memory_swap_max": 0, "max_running": 2, "pause_file": null,
                      "runtime_max_sec": 15,
                      "protected_pool": {"source_contract": "tool", "memory_bytes": 30, "queue_limit": 2, "wait_max_sec": 15,
                                         "max_job_bytes": 10, "max_running": 2, "runtime_max_sec": 15,
                                         "cpu_quota_usec": 50000, "tasks_max": 32}}
        }
    })).unwrap()
}

#[test]
fn protected_pool_policy_is_opt_in_bounded_and_zero_swap() {
    let p = policy();
    p.validate().unwrap();
    for (field, value) in [
        ("memory_bytes", json!(19)),
        ("memory_bytes", json!(101)),
        ("queue_limit", json!(0)),
        ("cpu_quota_usec", json!(0)),
        ("tasks_max", json!(0)),
    ] {
        let mut value_p = serde_json::to_value(&p).unwrap();
        value_p["contracts"]["quick"]["protected_pool"][field] = value;
        let invalid: Policy = serde_json::from_value(value_p).unwrap();
        assert!(invalid.validate().is_err(), "{field}");
    }
}

#[test]
fn protected_contracts_cannot_be_selected_directly() {
    let p = policy();
    let mut l = Ledger::new("boot".into());
    assert!(l.enqueue("legacy".into(), "quick", &p, 30000).is_err());
    assert!(
        l.enqueue_sized("sized".into(), "quick", &p, 30000, Some(1))
            .is_err()
    );
    assert!(
        l.enqueue_bounded("direct".into(), "quick", &p, 30000, 1, 1)
            .is_err()
    );
    assert!(l.entries.is_empty());
    l.enqueue_bounded("bounded".into(), "tool", &p, 15000, 1, 1)
        .unwrap();
    assert_eq!(l.get("bounded").unwrap().name, "quick");
}

#[test]
fn protected_queue_expiry_does_not_remove_reserved_entry_grace() {
    use amc_admission::ledger::Headroom;
    let p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue_bounded("near".into(), "tool", &p, 15000, 1, 1)
        .unwrap();
    l.enqueue_bounded("expired".into(), "tool", &p, 14999, 1, 1)
        .unwrap();
    l.advance(14999, &p, |_| {
        Some(Headroom {
            shared_bytes: 0,
            slice_bytes: 0,
            memory_max: 30,
            memory_swap_max: 0,
            paused: false,
        })
    });
    assert!(l.get("expired").is_none(), "queue wait remains bounded");
    let entry = l.get("near").unwrap();
    assert_eq!(entry.phase, Phase::Reserved);
    assert_eq!(entry.deadline_ms, 45000, "native entry retains its grace");
    l.advance(15001, &p, |_| {
        panic!("reserved entries are not queued again")
    });
    assert_eq!(
        l.get("near").unwrap().deadline_ms,
        45000,
        "grace is not renewed by polling"
    );
    l.advance(45000, &p, |_| panic!("expired entries cannot reserve"));
    assert!(l.get("near").is_none());
}

#[test]
fn full_bulk_queue_cannot_consume_the_protected_queue() {
    let p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue("bulk".into(), "tool", &p, 30000).unwrap();
    assert!(l.enqueue("bulk-two".into(), "tool", &p, 30000).is_err());
    l.enqueue_bounded("quick-one".into(), "tool", &p, 30000, 10, 15)
        .unwrap();
    l.enqueue_bounded("quick-two".into(), "tool", &p, 30000, 10, 15)
        .unwrap();
    assert!(
        l.enqueue_bounded("quick-three".into(), "tool", &p, 30000, 10, 15)
            .is_err()
    );
    assert!(l.entries.iter().all(|entry| entry.phase == Phase::Queued));
}

fn host_policy() -> amc_admission::host::HostPolicy {
    serde_json::from_value(json!({
        "version": 1, "budget_bytes": 100, "reserve_bytes": 20,
        "swap_reserve_bytes": 0, "max_memory_full_psi": 1.0, "max_io_full_psi": 20.0,
        "resume_ms": 250, "aging_ms": 1000, "queue_limit": 1,
        "domains": [
            {"name": "bulk", "uid": 1000, "cgroup": "/bulk", "ceiling_bytes": 100,
             "swap_bytes": 0, "fair_share_bytes": 100},
            {"name": "quick", "uid": 1000, "cgroup": "/quick", "ceiling_bytes": 10,
             "swap_bytes": 0, "fair_share_bytes": 100,
             "protected_pool": policy().contracts["quick"].protected_pool}
        ]
    }))
    .unwrap()
}

#[test]
fn host_protected_wait_is_capped_once_without_extending_retries() {
    use amc_admission::host::HostLedger;
    let p = host_policy();
    let mut l = HostLedger::new("boot".into());
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    let mut r = job("bounded", true, 10);
    r.requested_ms = 1000;
    r.deadline_ms = 3_601_000;
    l.request(r.clone(), &p).unwrap();
    assert_eq!(l.reservations[0].deadline_ms, 16000);
    r.requested_ms = 2000;
    l.request(r, &p).unwrap();
    assert_eq!(l.reservations[0].deadline_ms, 16000);
    let mut short = job("short", true, 10);
    short.identity.inode = 2;
    short.deadline_ms = 10;
    l.request(short, &p).unwrap();
    assert_eq!(l.reservations[1].deadline_ms, 10);
}

fn job(id: &str, quick: bool, bytes: u64) -> amc_admission::host::Reservation {
    let domain = if quick { "quick" } else { "bulk" };
    serde_json::from_value(json!({
        "id": id, "domain": domain,
        "identity": {"uid": 1000, "cgroup": format!("/{domain}/{id}"),
                     "inode": 1, "pid": 1, "start_ticks": 1},
        "memory_bytes": bytes, "swap_bytes": 0, "requested_ms": 0,
        "deadline_ms": 30000, "granted": false,
        "runtime_max_ms": if quick { Some(15000) } else { None }
    }))
    .unwrap()
}

#[test]
fn idle_backing_survives_cleanup_restart_and_unknown_retirement() {
    use amc_admission::host::HostLedger;
    let mut p = host_policy();
    let mut l = HostLedger::new("boot".into());
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    assert_eq!(l.committed(), 30);
    l.reconcile(|_| Some(true));
    assert_eq!(l.committed(), 30, "idle is not decommissioned");
    let mut restored: HostLedger =
        serde_json::from_value(serde_json::to_value(l).unwrap()).unwrap();
    restored.validate().unwrap();
    p.domains.retain(|d| d.protected_pool.is_none());
    restored.sync_protected_pools(&p, |_| None).unwrap();
    assert_eq!(restored.committed(), 30);
    restored.sync_protected_pools(&p, |_| Some(false)).unwrap();
    assert_eq!(restored.committed(), 30);
    restored.sync_protected_pools(&p, |_| Some(true)).unwrap();
    assert_eq!(restored.committed(), 0);
}

#[test]
fn full_bulk_budget_and_queue_do_not_charge_a_protected_child_twice() {
    use amc_admission::host::{Capacity, HostLedger};
    use std::collections::BTreeMap;
    let p = host_policy();
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    let mut bulk = job("held", false, 70);
    l.request(bulk.clone(), &p).unwrap();
    // Already-admitted bulk uses the rest of the budget.
    bulk.granted = true;
    l.reservations[0] = bulk;
    assert_eq!(l.committed(), 100);
    l.request(job("small", true, 10), &p).unwrap();
    let waits = l.advance_with_burst_runtime(
        1500,
        &p,
        Some(Capacity {
            available_bytes: 21,
            swap_free_bytes: 100,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        }),
        &mut BTreeMap::from([("quick".into(), 0)]),
        |r, _| {
            assert_eq!(
                r.memory_bytes, 30,
                "verify the complete parent backing, not extra child headroom"
            );
            Some(30)
        },
        |_| Some(15000),
    );
    assert!(!waits.contains_key("small"), "{waits:?}");
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "small")
            .unwrap()
            .granted
    );
    assert_eq!(l.committed(), 100);
}

#[test]
fn bounded_selection_is_pre_submission_opt_in_and_never_promotes_a_full_pool() {
    let p = policy();
    let mut l = Ledger::new("boot".into());
    // Legacy/sized-only requests keep their ordinary contract.
    l.enqueue_sized("legacy".into(), "tool", &p, 30000, Some(1))
        .unwrap();
    assert_eq!(l.get("legacy").unwrap().name, "tool");
    l.cancel_pending("legacy");
    for (id, bytes, seconds, expected) in [
        ("large", 11, 15, "tool"),
        ("long", 1, 16, "tool"),
        ("small", 10, 15, "quick"),
    ] {
        l.enqueue_bounded(id.into(), "tool", &p, 30000, bytes, seconds)
            .unwrap();
        assert_eq!(l.get(id).unwrap().name, expected);
        l.cancel_pending(id);
    }
    l.enqueue_bounded("one".into(), "tool", &p, 30000, 10, 15)
        .unwrap();
    l.enqueue_bounded("two".into(), "tool", &p, 30000, 10, 15)
        .unwrap();
    assert!(
        l.enqueue_bounded("three".into(), "tool", &p, 30000, 10, 15)
            .is_err()
    );
    assert_eq!(l.entries.len(), 2);
}

#[test]
fn user_backing_and_slots_survive_restart_until_positive_cleanup() {
    use amc_admission::ledger::{Headroom, Identity};
    let mut p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue_sized("bulk".into(), "tool", &p, 30000, Some(70))
        .unwrap();
    l.entries[0].phase = Phase::Reserved;
    l.enqueue_bounded("small".into(), "tool", &p, 30000, 10, 15)
        .unwrap();
    assert_eq!(l.committed(), 100);
    assert!(
        l.advance(1, &p, |_| Some(Headroom {
            shared_bytes: 0,
            slice_bytes: 0,
            memory_max: 30,
            memory_swap_max: 0,
            paused: false,
        }))
        .is_empty(),
        "root broker, not the occupied bulk slice, verifies host safety"
    );
    l.enter(
        "small",
        Identity {
            invocation: "01".repeat(16),
            cgroup: "/quick/small".into(),
            inode: 1,
        },
    )
    .unwrap();
    assert!(!l.cancel_pending("small"));
    let mut l: Ledger = serde_json::from_value(serde_json::to_value(l).unwrap()).unwrap();
    l.validate().unwrap();
    l.reconcile(|_| false);
    assert_eq!(l.committed(), 100);
    p.contracts.remove("quick");
    l.reconcile_protected_pools(&p, |_| Some(true));
    assert_eq!(l.committed(), 100, "live entry keeps superseded backing");
    l.reconcile(|_| true);
    l.reconcile_protected_pools(&p, |_| None);
    assert_eq!(
        l.committed(),
        100,
        "unknown native pool cleanup keeps backing"
    );
    l.reconcile_protected_pools(&p, |_| Some(true));
    assert_eq!(l.committed(), 70);
}

#[test]
fn protected_children_keep_emergency_recovery_runtime_and_concurrency_gates() {
    use amc_admission::host::{Capacity, HostLedger, WaitReason};
    use std::collections::BTreeMap;
    let p = host_policy();
    let mut base = HostLedger::new("boot".into());
    base.sync_protected_pools(&p, |_| Some(true)).unwrap();
    base.request(job("small", true, 10), &p).unwrap();
    for (capacity, runtime, ancestor, expected) in [
        (None, Some(15000), Some(30), WaitReason::Unknown),
        (Some(19), Some(15000), Some(30), WaitReason::Pressure),
        (Some(21), None, Some(30), WaitReason::Unknown),
        (Some(21), Some(16000), Some(30), WaitReason::Unknown),
        (Some(21), Some(15000), None, WaitReason::Unknown),
        (
            Some(21),
            Some(15000),
            Some(29),
            WaitReason::AncestorHeadroom,
        ),
    ] {
        let mut l = base.clone();
        let waits = l.advance_with_burst_runtime(
            1500,
            &p,
            capacity.map(|available_bytes| Capacity {
                available_bytes,
                swap_free_bytes: 100,
                memory_full_psi: 0.0,
                io_full_psi: 0.0,
            }),
            &mut BTreeMap::from([("quick".into(), 0)]),
            |_, _| ancestor,
            |_| runtime,
        );
        assert_eq!(waits["small"], expected);
        assert!(!l.reservations[0].granted);
    }
    let mut l = base;
    l.reservations[0].granted = true;
    let mut second = job("second", true, 10);
    second.identity.inode = 2;
    l.request(second, &p).unwrap();
    l.reservations[1].granted = true;
    // Increase queue capacity for one waiting caller; occupied slots stay held.
    let mut p = p;
    p.domains[1].protected_pool.as_mut().unwrap().queue_limit = 3;
    let mut third = job("third", true, 10);
    third.identity.inode = 3;
    l.request(third, &p).unwrap();
    l.reconcile(|_| None);
    let waits = l.advance_with_burst_runtime(
        1500,
        &p,
        Some(Capacity {
            available_bytes: 1000,
            swap_free_bytes: 100,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        }),
        &mut BTreeMap::from([("quick".into(), 0)]),
        |_, _| Some(30),
        |_| Some(15000),
    );
    assert_eq!(waits["third"], WaitReason::ProtectedConcurrency);
    assert_eq!(l.committed(), 30);
}

#[test]
fn occupied_pool_reductions_preserve_old_backing_and_native_claims() {
    use amc_admission::host::HostLedger;
    let mut p = host_policy();
    let mut l = HostLedger::new("boot".into());
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    l.request(job("small", true, 10), &p).unwrap();
    l.reservations[0].granted = true;
    p.domains[1].protected_pool.as_mut().unwrap().memory_bytes = 25;
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    assert_eq!(l.committed(), 30);
    let claims = l.native_claims(&p, None).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].memory_bytes, 30);
    let mut restored: HostLedger =
        serde_json::from_value(serde_json::to_value(l).unwrap()).unwrap();
    restored.validate().unwrap();
    restored.reconcile(|_| Some(true));
    restored.sync_protected_pools(&p, |_| Some(false)).unwrap();
    assert_eq!(restored.committed(), 30);
    restored.sync_protected_pools(&p, |_| Some(true)).unwrap();
    assert_eq!(restored.committed(), 25);
}

#[test]
fn original_controller_bounds_survive_policy_relaxation_and_legacy_snapshots() {
    use amc_admission::host::HostLedger;
    let mut p = host_policy();
    let mut l = HostLedger::new("boot".into());
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    l.request(job("survivor", true, 10), &p).unwrap();
    l.reservations[0].granted = true;
    let pool = p.domains[1].protected_pool.as_mut().unwrap();
    pool.cpu_quota_usec = 75000;
    pool.tasks_max = 64;
    l.sync_protected_pools(&p, |_| Some(true)).unwrap();
    let mut restored: HostLedger =
        serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    restored.validate().unwrap();
    assert_eq!(
        restored.protected_pools["quick"].cpu_quota_usec,
        Some(50000)
    );
    assert_eq!(restored.protected_pools["quick"].tasks_max, Some(32));
    restored.reconcile(|_| Some(true));
    restored.sync_protected_pools(&p, |_| None).unwrap();
    assert_eq!(
        restored.protected_pools["quick"].cpu_quota_usec,
        Some(50000)
    );
    let legacy = restored.protected_pools.get_mut("quick").unwrap();
    legacy.cpu_quota_usec = None;
    legacy.tasks_max = None;
    restored.sync_protected_pools(&p, |_| None).unwrap();
    assert_eq!(restored.protected_pools["quick"].cpu_quota_usec, None);
    restored.sync_protected_pools(&p, |_| Some(true)).unwrap();
    assert_eq!(
        restored.protected_pools["quick"].cpu_quota_usec,
        Some(75000)
    );
    assert_eq!(restored.protected_pools["quick"].tasks_max, Some(64));
}
