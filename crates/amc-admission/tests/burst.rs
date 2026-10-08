use amc_admission::host::{Capacity, HostLedger, HostPolicy, Reservation, WaitReason};
use serde_json::json;
use std::collections::BTreeMap;

fn policy() -> HostPolicy {
    serde_json::from_value(json!({
        "version": 1, "budget_bytes": 80, "reserve_bytes": 20,
        "swap_reserve_bytes": 0, "max_memory_full_psi": 1.0,
        "max_io_full_psi": 20.0, "resume_ms": 250, "aging_ms": 1000,
        "queue_limit": 32,
        "burst": {"budget_bytes": 20, "max_job_bytes": 10, "max_running": 1,
                  "max_runtime_ms": 5000, "min_interval_ms": 10000},
        "domains": [
            {"name": "normal", "uid": 1000, "cgroup": "/normal", "ceiling_bytes": 80,
             "swap_bytes": 0, "fair_share_bytes": 80},
            {"name": "burst", "uid": 1000, "cgroup": "/burst", "ceiling_bytes": 10,
             "swap_bytes": 0, "fair_share_bytes": 80, "burst": true}
        ]
    }))
    .unwrap()
}

fn job(id: &str, burst: bool, memory: u64) -> Reservation {
    let domain = if burst { "burst" } else { "normal" };
    serde_json::from_value(json!({
        "id": id, "domain": domain,
        "identity": {"cgroup": format!("/{domain}/{id}"), "inode": 1,
                     "uid": 1000, "pid": 1, "start_ticks": 1},
        "memory_bytes": memory, "swap_bytes": 0, "requested_ms": 0,
        "deadline_ms": 30000, "granted": false, "owners": [],
        "burst": burst, "runtime_max_ms": if burst { Some(5000) } else { None }
    }))
    .unwrap()
}

fn advance(
    ledger: &mut HostLedger,
    policy: &HostPolicy,
    now: u64,
    available: u64,
) -> BTreeMap<String, WaitReason> {
    let mut healthy = BTreeMap::from([("normal".into(), 0), ("burst".into(), 0)]);
    ledger.advance(
        now,
        policy,
        Some(Capacity {
            available_bytes: available,
            swap_free_bytes: 100,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        }),
        &mut healthy,
        |_, _| Some(1000),
    )
}

#[test]
fn short_burst_can_exceed_normal_budget_and_bypass_aged_bulk_wait() {
    let p = policy();
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.request(job("held", false, 80), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    l.request(job("bulk-wait", false, 10), &p).unwrap();
    l.request(job("quick", true, 10), &p).unwrap();
    let waits = advance(&mut l, &p, 1500, 1000);
    assert_eq!(waits["bulk-wait"], WaitReason::Budget);
    assert!(!waits.contains_key("quick"));
    assert_eq!(l.committed(), 90);
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "quick")
            .unwrap()
            .granted
    );
}

#[test]
fn burst_never_spends_host_reserve_or_unknown_ancestor_capacity() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("held", false, 80), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    l.request(job("quick", true, 10), &p).unwrap();
    assert_eq!(
        advance(&mut l, &p, 500, 100)["quick"],
        WaitReason::HostHeadroom
    );
    assert_eq!(l.committed(), 80);
    let mut healthy = BTreeMap::from([("burst".into(), 0)]);
    assert_eq!(
        l.advance(750, &p, None, &mut healthy, |_, _| None)["quick"],
        WaitReason::Unknown
    );
    assert_eq!(l.committed(), 80);
}

#[test]
fn burst_concurrency_and_cooldown_survive_cleanup_and_restart() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("first", true, 10), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    l.request(job("second", true, 10), &p).unwrap();
    assert!(advance(&mut l, &p, 500, 1000).contains_key("second"));
    l.reconcile(|r| Some(r.id == "first"));
    let mut restored: HostLedger =
        serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    assert!(advance(&mut restored, &p, 1000, 1000).contains_key("second"));
    assert_eq!(restored.committed(), 0);
    advance(&mut restored, &p, 10250, 1000);
    assert_eq!(restored.committed(), 10);
}

#[test]
fn granted_burst_survives_deadline_policy_disable_and_unknown_cleanup() {
    let mut p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("held", true, 10), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    p.burst = None;
    l.reconcile(|_| None);
    advance(&mut l, &p, 40000, 1000);
    assert_eq!(l.committed(), 10);
    assert!(l.request(job("new", true, 10), &p).is_err());
    l.reconcile(|_| Some(true));
    assert_eq!(l.committed(), 0);
}

#[test]
fn burst_requires_matching_native_class_zero_swap_and_short_deadline() {
    let p = policy();
    for (field, value) in [
        ("memory_bytes", json!(11)),
        ("swap_bytes", json!(1)),
        ("runtime_max_ms", json!(null)),
        ("runtime_max_ms", json!(5001)),
        ("burst", json!(false)),
    ] {
        let mut json = serde_json::to_value(job("invalid", true, 10)).unwrap();
        json[field] = value;
        let mut l = HostLedger::new("boot".into());
        assert!(
            l.request(serde_json::from_value(json).unwrap(), &p)
                .is_err(),
            "{field}"
        );
    }
}

#[test]
fn aged_bulk_job_gets_a_quiet_window_when_only_bursts_prevent_it_fitting() {
    let mut p = policy();
    p.burst.as_mut().unwrap().max_running = 2;
    let mut peer = p.domains[1].clone();
    peer.uid = 1001;
    peer.name = "peer-burst".into();
    peer.cgroup = "/peer-burst".into();
    p.domains.push(peer);
    let mut l = HostLedger::new("boot".into());
    l.request(job("first", true, 10), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    l.request(job("bulk", false, 80), &p).unwrap();
    let mut next = job("next", true, 10);
    next.domain = "peer-burst".into();
    next.identity.uid = 1001;
    next.identity.cgroup = "/peer-burst/next".into();
    l.request(next, &p).unwrap();
    let mut healthy = BTreeMap::from([
        ("normal".into(), 0),
        ("burst".into(), 0),
        ("peer-burst".into(), 0),
    ]);
    assert_eq!(
        l.advance(
            1500,
            &p,
            Some(Capacity {
                available_bytes: 1000,
                swap_free_bytes: 100,
                memory_full_psi: 0.0,
                io_full_psi: 0.0
            }),
            &mut healthy,
            |_, _| Some(1000)
        )["next"],
        WaitReason::AgedRequest
    );
    l.reconcile(|r| Some(r.id == "first"));
    advance(&mut l, &p, 10250, 1000);
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "bulk")
            .unwrap()
            .granted
    );
}

#[test]
fn aged_bulk_host_headroom_wait_does_not_veto_a_physically_fitting_burst() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("held", false, 70), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    l.request(job("bulk", false, 10), &p).unwrap();
    l.request(job("quick", true, 5), &p).unwrap();
    let waits = advance(&mut l, &p, 1500, 95);
    assert_eq!(waits["bulk"], WaitReason::HostHeadroom);
    assert!(!waits.contains_key("quick"));
    assert_eq!(l.committed(), 75);
}

#[test]
fn bursts_backfill_when_drain_cannot_clear_swap_return_or_ancestor_waits() {
    for reason in [
        WaitReason::SwapHeadroom,
        WaitReason::SwapReturn,
        WaitReason::AncestorHeadroom,
    ] {
        let mut p = policy();
        p.domains[0].swap_bytes = 101;
        p.reserve_swap_return = true;
        p.burst.as_mut().unwrap().min_interval_ms = 0;
        p.burst.as_mut().unwrap().max_running = 2;
        let mut l = HostLedger::new("boot".into());
        l.swap_return_bytes = Some(0);
        l.request(job("held", true, 5), &p).unwrap();
        advance(&mut l, &p, 250, 1000);
        let mut bulk = job("bulk", false, 20);
        if reason == WaitReason::SwapHeadroom {
            bulk.swap_bytes = 101;
        }
        if reason == WaitReason::SwapReturn {
            l.swap_return_bytes = Some(970);
        }
        l.request(bulk, &p).unwrap();
        l.request(job("quick", true, 5), &p).unwrap();
        let waits = l.advance(
            1500,
            &p,
            Some(Capacity {
                available_bytes: 1000,
                swap_free_bytes: 100,
                memory_full_psi: 0.0,
                io_full_psi: 0.0,
            }),
            &mut BTreeMap::from([("normal".into(), 0), ("burst".into(), 0)]),
            |r, _| {
                Some(
                    if reason == WaitReason::AncestorHeadroom && r.id == "bulk" {
                        0
                    } else {
                        1000
                    },
                )
            },
        );
        assert_eq!(waits["bulk"], reason);
        assert!(!waits.contains_key("quick"), "{reason:?}: {waits:?}");
    }
}

#[test]
fn burst_quiet_windows_require_the_complete_upfront_completion_charge_to_fit() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 80,
        memory_bytes: 30,
        swap_bytes: 0,
        max_calls: 1,
        domains: vec!["normal".into()],
    });
    let mut l = HostLedger::new("boot".into());
    l.request(job("bulk", false, 60), &p).unwrap();
    l.request(job("quick", true, 5), &p).unwrap();
    let waits = advance(&mut l, &p, 1500, 1000);
    assert_eq!(waits["bulk"], WaitReason::Budget);
    assert!(!waits.contains_key("quick"));
}

#[test]
fn downstream_aged_waits_cannot_veto_burst_backfill() {
    let mut p = policy();
    p.domains[0].swap_bytes = 101;
    p.burst.as_mut().unwrap().min_interval_ms = 0;
    p.burst.as_mut().unwrap().max_running = 2;
    let mut l = HostLedger::new("boot".into());
    l.request(job("held", true, 5), &p).unwrap();
    advance(&mut l, &p, 250, 1000);
    let mut first = job("first", false, 20);
    first.swap_bytes = 101;
    l.request(first, &p).unwrap();
    let mut second = job("second", false, 20);
    second.requested_ms = 1;
    l.request(second, &p).unwrap();
    let mut quick = job("quick", true, 5);
    quick.requested_ms = 2;
    l.request(quick, &p).unwrap();
    let waits = advance(&mut l, &p, 1500, 1000);
    assert_eq!(waits["first"], WaitReason::SwapHeadroom);
    assert_eq!(waits["second"], WaitReason::AgedRequest);
    assert!(!waits.contains_key("quick"), "{waits:?}");
    assert_eq!(l.burst_committed(), 10);
}

#[test]
fn independent_users_share_the_burst_allowance_and_pressure_gates() {
    let mut p = policy();
    p.burst.as_mut().unwrap().max_running = 8;
    p.burst.as_mut().unwrap().budget_bytes = 10;
    let mut peer = p.domains[1].clone();
    peer.uid = 1001;
    peer.name = "peer-burst".into();
    peer.cgroup = "/peer-burst".into();
    p.domains.push(peer);
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.request(job("alice", true, 10), &p).unwrap();
    let mut second = job("bob", true, 5);
    second.domain = "peer-burst".into();
    second.identity.uid = 1001;
    second.identity.cgroup = "/peer-burst/bob".into();
    l.request(second, &p).unwrap();
    let mut healthy = BTreeMap::from([("burst".into(), 0), ("peer-burst".into(), 0)]);
    let capacity = Capacity {
        available_bytes: 1000,
        swap_free_bytes: 100,
        memory_full_psi: 0.0,
        io_full_psi: 0.0,
    };
    let waits = l.advance(250, &p, Some(capacity), &mut healthy, |_, _| Some(1000));
    assert_eq!(l.burst_committed(), 10);
    assert_eq!(waits["bob"], WaitReason::BurstBudget);
    l.reconcile(|r| Some(r.id == "alice"));
    for capacity in [
        Capacity {
            memory_full_psi: 2.0,
            ..capacity
        },
        Capacity {
            io_full_psi: 21.0,
            ..capacity
        },
        Capacity {
            io_full_psi: f64::NAN,
            ..capacity
        },
    ] {
        assert_eq!(
            l.advance(500, &p, Some(capacity), &mut healthy, |_, _| Some(1000))["bob"],
            WaitReason::Pressure
        );
        assert_eq!(l.committed(), 0);
    }
    l.advance(1000, &p, Some(capacity), &mut healthy, |_, _| Some(1000));
    l.advance(1250, &p, Some(capacity), &mut healthy, |_, _| Some(1000));
    assert_eq!(l.burst_committed(), 5);
}
