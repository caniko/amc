use amc_admission::{host::*, preparation::*};
use std::collections::BTreeMap;

fn policy() -> HostPolicy {
    serde_json::from_value(serde_json::json!({
        "version": 1, "budget_bytes": 100, "reserve_bytes": 20, "swap_reserve_bytes": 10,
        "max_memory_full_psi": 1.0, "max_io_full_psi": 20.0,
        "resume_ms": 250, "aging_ms": 1000, "queue_limit": 32,
        "domains": [
            {"name":"work", "uid":1000, "cgroup":"/work.slice", "ceiling_bytes":50,
             "swap_bytes":10, "fair_share_bytes":50},
            {"name":"game", "uid":1000, "cgroup":"/game.slice", "ceiling_bytes":40,
             "swap_bytes":0, "fair_share_bytes":50},
            {"name":"builders", "uid":0, "cgroup":"/builders", "ceiling_bytes":50,
             "swap_bytes":10, "fair_share_bytes":50}
        ],
        "preparations":[{"name":"game", "domain":"game", "memory_bytes":40,
            "swap_bytes":0, "drain_domains":["work", "builders"],
            "wait_ms":10000, "ready_ms":15000}],
        "reserve_swap_return":true
    }))
    .unwrap()
}

fn job(id: &str, domain: &str, uid: u32, bytes: u64) -> Reservation {
    Reservation {
        id: id.into(),
        domain: domain.into(),
        identity: Identity {
            uid,
            pid: 42,
            start_ticks: 7,
            inode: 1,
            cgroup: format!("/{domain}.slice/app-amc-job-{id}.service"),
        },
        memory_bytes: bytes,
        swap_bytes: 0,
        requested_ms: 0,
        deadline_ms: 10000,
        granted: false,
        owners: vec![],
        owners_finished: false,
        burst: false,
        runtime_max_ms: None,
        continuation: None,
    }
}

fn capacity() -> Option<Capacity> {
    Some(Capacity {
        available_bytes: 200,
        swap_free_bytes: 100,
        memory_full_psi: 0.0,
        io_full_psi: 0.0,
    })
}

fn advance(l: &mut HostLedger, p: &HostPolicy, now: u64) -> BTreeMap<String, WaitReason> {
    let mut healthy = BTreeMap::from([
        ("work".into(), 0),
        ("game".into(), 0),
        ("builders".into(), 0),
    ]);
    l.advance(now, p, capacity(), &mut healthy, |_, _| Some(200))
}

#[test]
fn preparation_closes_admission_before_readiness_and_preserves_live_work() {
    let p = policy();
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.request(job("old", "work", 1000, 20), &p).unwrap();
    advance(&mut l, &p, 250);
    l.prepare("intent".into(), "key".into(), 1000, "game", 300, &p)
        .unwrap();
    l.request(job("new", "work", 1000, 20), &p).unwrap();
    assert_eq!(advance(&mut l, &p, 500)["new"], WaitReason::Preparation);
    assert!(l.reservations[0].granted);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Draining);
    l.reconcile(|r| Some(r.id == "old"));
    advance(&mut l, &p, 750);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Ready);
    assert_eq!(l.committed(), 40);
    assert!(!l.reservations[0].granted);
}

#[test]
fn preparation_ready_windows_cover_the_bounded_native_registration_path() {
    let mut p = policy();
    p.preparations[0].ready_ms = 1000;
    assert!(p.validate().is_err());
    p.preparations[0].ready_ms = 15000;
    p.validate().unwrap();
    p.preparations[0].ready_ms = 60000;
    p.validate().unwrap();
    p.preparations[0].ready_ms = 60001;
    assert!(p.validate().is_err());
}

#[test]
fn ready_transfer_is_once_only_and_unknown_cleanup_survives_restart_and_expiry() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.prepare("intent".into(), "key".into(), 1000, "game", 0, &p)
        .unwrap();
    advance(&mut l, &p, 250);
    let native = job("intent", "game", 1000, 40);
    assert!(
        l.consume("intent", "wrong", 1000, native.clone(), 500)
            .is_err()
    );
    l.consume("intent", "key", 1000, native.clone(), 500)
        .unwrap();
    assert_eq!(l.committed(), 40);
    l.consume("intent", "key", 1000, native.clone(), 500)
        .unwrap();
    let mut other = native;
    other.identity.inode = 2;
    assert!(l.consume("intent", "key", 1000, other, 500).is_err());
    l.cancel_preparation("intent", "key", 1000).unwrap();
    let mut restored: HostLedger =
        serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    restored.validate().unwrap();
    restored.reconcile(|_| None);
    advance(&mut restored, &p, 50000);
    assert_eq!(restored.committed(), 40);
    restored.reconcile(|_| Some(true));
    advance(&mut restored, &p, 50001);
    assert_eq!(restored.committed(), 0);
    assert!(restored.preparations.is_empty());
}

#[test]
fn cancellation_and_ready_expiry_do_not_remove_another_intents_barrier() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    for id in ["first", "second"] {
        l.prepare(id.into(), id.into(), 1000, "game", 0, &p)
            .unwrap();
    }
    advance(&mut l, &p, 250);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Ready);
    assert_eq!(l.preparations[1].phase, PreparationPhase::Draining);
    l.cancel_preparation("first", "first", 1000).unwrap();
    assert!(l.preparation_barrier());
    advance(&mut l, &p, 500);
    assert_eq!(l.committed(), 40);
    advance(&mut l, &p, 500 + p.preparations[0].ready_ms);
    assert!(!l.preparation_barrier());
    assert_eq!(l.committed(), 0);
}

#[test]
fn swap_returns_get_ram_before_new_admissions_and_unknown_observations_block() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("new", "work", 1000, 40), &p).unwrap();
    assert_eq!(advance(&mut l, &p, 250)["new"], WaitReason::Unknown);
    l.swap_return_bytes = Some(150);
    assert_eq!(advance(&mut l, &p, 500)["new"], WaitReason::SwapReturn);
    l.swap_return_bytes = Some(0);
    advance(&mut l, &p, 750);
    assert_eq!(l.committed(), 40);
}

#[test]
fn a_drain_includes_live_parents_that_can_still_launch_selected_completion_children() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    let mut parent_domain = p.domains[0].clone();
    parent_domain.name = "parent".into();
    parent_domain.cgroup = "/parent.slice".into();
    parent_domain.continuation = Some(ContinuationPolicy {
        parent_max_bytes: 20,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 1,
        domains: vec!["work".into()],
        envelopes: Default::default(),
    });
    p.domains.push(parent_domain);
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.request(job("parent", "parent", 1000, 20), &p).unwrap();
    l.advance(
        250,
        &p,
        capacity(),
        &mut BTreeMap::from([("parent".into(), 0)]),
        |_, _| Some(200),
    );
    assert!(l.reservations[0].granted);
    let cap = l.continuation_capability("parent", &p).unwrap().unwrap();
    l.prepare("intent".into(), "key".into(), 1000, "game", 550, &p)
        .unwrap();
    advance(&mut l, &p, 750);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Draining);
    assert_eq!(l.preparations[0].drain, vec!["parent"]);

    let mut child = job("child", "work", 1000, 20);
    child.continuation = Some(
        l.authorize_continuation(&cap, 1000, "work", 20, 0, "child")
            .unwrap(),
    );
    l.request(child, &p).unwrap();
    l.reconcile(|r| Some(r.id == "parent"));
    let mut l: HostLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    l.validate().unwrap();
    advance(&mut l, &p, 1000);
    assert!(l.reservations[0].granted);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Draining);
    l.reconcile(|_| Some(true));
    advance(&mut l, &p, 1250);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Ready);
    assert_eq!(l.committed(), 40);
}

#[test]
fn root_pool_retries_continue_but_new_owners_cannot_join_during_draining() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    let mut pool = job("pool", "builders", 0, 50);
    pool.identity.cgroup = "/builders".into();
    l.request(pool.clone(), &p).unwrap();
    advance(&mut l, &p, 250);
    l.prepare("intent".into(), "key".into(), 1000, "game", 300, &p)
        .unwrap();
    assert!(l.may_join(&pool.identity));
    pool.identity.pid = 43;
    assert!(!l.may_join(&pool.identity));
    assert!(l.request(pool, &p).is_err());
    assert_eq!(l.reservations[0].owners.len(), 1);
}

#[test]
fn granted_root_pools_reject_a_different_completion_parent_before_joining() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    // Root pools have a fixed enforced envelope; this fixture uses a 20-byte
    // pool so two upfront parent lanes fit its 100-byte host budget.
    p.domains[2].ceiling_bytes = 20;
    p.domains[2].swap_bytes = 0;
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 20,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 2,
        domains: vec!["builders".into()],
        envelopes: Default::default(),
    });
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    for parent in ["first", "second"] {
        l.request(job(parent, "work", 1000, 20), &p).unwrap();
    }
    advance(&mut l, &p, 250);
    let first = l.continuation_capability("first", &p).unwrap().unwrap();
    let second = l.continuation_capability("second", &p).unwrap().unwrap();
    let mut pool = job("pool", "builders", 0, 20);
    pool.identity.cgroup = "/builders".into();
    pool.continuation = Some(
        l.authorize_continuation(&first, 1000, "builders", 20, 0, "pool")
            .unwrap(),
    );
    l.request(pool.clone(), &p).unwrap();
    advance(&mut l, &p, 500);
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "pool")
            .unwrap()
            .granted
    );

    let mut other = pool.clone();
    other.id = "other".into();
    other.identity.pid = 43;
    other.continuation = Some(
        l.authorize_continuation(&second, 1000, "builders", 20, 0, "other")
            .unwrap(),
    );
    let before = serde_json::to_vec(&l).unwrap();
    assert!(l.request(other, &p).is_err());
    assert_eq!(serde_json::to_vec(&l).unwrap(), before);
    pool.identity.pid = 44;
    assert_eq!(l.request(pool, &p).unwrap(), "pool");
    let held = l.reservations.iter().find(|r| r.id == "pool").unwrap();
    assert_eq!(held.continuation.as_deref(), Some("first"));
    assert_eq!(held.owners.len(), 2);
}

#[test]
fn finite_authenticated_children_finish_during_drain_without_admitting_unrelated_work() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 10,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 1,
        domains: vec!["work".into()],
        envelopes: Default::default(),
    });
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.request(job("parent", "work", 1000, 10), &p).unwrap();
    advance(&mut l, &p, 250);
    let cap = l.continuation_capability("parent", &p).unwrap().unwrap();
    l.prepare("intent".into(), "key".into(), 1000, "game", 300, &p)
        .unwrap();
    assert!(
        l.authorize_continuation(&cap, 1001, "work", 20, 0, "child")
            .is_err()
    );
    assert!(
        l.authorize_continuation(&cap, 1000, "game", 20, 0, "child")
            .is_err()
    );
    let parent = l
        .authorize_continuation(&cap, 1000, "work", 20, 0, "child")
        .unwrap();
    l.authorize_continuation(&cap, 1000, "work", 20, 0, "child")
        .unwrap();
    assert!(
        l.authorize_continuation(&cap, 1000, "work", 20, 0, "another")
            .is_err()
    );
    let mut child = job("child", "work", 1000, 20);
    child.identity.inode = 2;
    child.continuation = Some(parent);
    l.request(child, &p).unwrap();
    l.request(job("unrelated", "work", 1000, 20), &p).unwrap();
    let waits = advance(&mut l, &p, 500);
    assert!(!waits.contains_key("child"));
    assert_eq!(waits["unrelated"], WaitReason::Preparation);
    l.reconcile(|r| Some(r.id == "parent"));
    // A granted child retains finite operation rights until its own cleanup.
    assert_eq!(l.continuations.len(), 1);
    l.reconcile(|r| Some(r.id == "child"));
    advance(&mut l, &p, 750);
    assert!(l.continuations.is_empty());
}

#[test]
fn pending_root_pools_cannot_change_their_first_completion_parent() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 20,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 2,
        domains: vec!["builders".into()],
        envelopes: Default::default(),
    });
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    for parent in ["first", "second"] {
        l.request(job(parent, "work", 1000, 20), &p).unwrap();
    }
    advance(&mut l, &p, 250);
    let first = l.continuation_capability("first", &p).unwrap().unwrap();
    let second = l.continuation_capability("second", &p).unwrap().unwrap();
    let mut pool = job("pool", "builders", 0, 20);
    pool.identity.cgroup = "/builders".into();
    pool.continuation = Some(
        l.authorize_continuation(&first, 1000, "builders", 20, 0, "pool")
            .unwrap(),
    );
    l.request(pool.clone(), &p).unwrap();
    assert!(
        !l.reservations
            .iter()
            .find(|r| r.id == "pool")
            .unwrap()
            .granted
    );
    let mut other = pool.clone();
    other.identity.pid = 43;
    other.continuation = Some(
        l.authorize_continuation(&second, 1000, "builders", 20, 0, "other")
            .unwrap(),
    );
    let before = serde_json::to_vec(&l).unwrap();
    assert!(l.request(other, &p).is_err());
    assert_eq!(serde_json::to_vec(&l).unwrap(), before);
    pool.identity.pid = 44;
    l.request(pool, &p).unwrap();
    let held = l.reservations.iter().find(|r| r.id == "pool").unwrap();
    assert_eq!(held.continuation.as_deref(), Some("first"));
    assert_eq!(held.owners.len(), 2);
    let identity = held.identity.clone();
    l.bind_pool_operation("builders", &identity, 1).unwrap();
    l.reconcile(|r| Some(r.id == "first"));
    let restored: HostLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    assert!(restored.owns_pool_operation("builders", &identity, Some(1)));
    assert!(!restored.owns_pool_operation("builders", &identity, Some(2)));
}

#[test]
fn a_real_prepared_claim_with_a_lane_id_still_backs_the_shared_ancestor() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 10,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 1,
        domains: vec!["builders".into()],
        envelopes: Default::default(),
    });
    p.domains[1].cgroup = "/shared/game.slice".into();
    p.domains[2].cgroup = "/shared/builders.slice".into();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    let collision = "escrow-parent-builders";
    l.prepare(collision.into(), "key".into(), 1000, "game", 0, &p)
        .unwrap();
    advance(&mut l, &p, 250);
    let mut native = job(collision, "game", 1000, 40);
    native.identity.cgroup = format!("/shared/game.slice/app-amc-prepared-{collision}.scope");
    l.consume(collision, "key", 1000, native, 300).unwrap();
    l.request(job("parent", "work", 1000, 10), &p).unwrap();
    let waits = l.advance(
        500,
        &p,
        capacity(),
        &mut BTreeMap::from([("work".into(), 0)]),
        |r, claims| {
            Some(if r.domain == "builders" {
                50u64.saturating_sub(
                    claims
                        .iter()
                        .filter(|r| r.granted && r.identity.cgroup.starts_with("/shared/"))
                        .map(|r| r.memory_bytes)
                        .sum::<u64>(),
                )
            } else {
                u64::MAX
            })
        },
    );
    assert_eq!(waits.get("parent"), Some(&WaitReason::AncestorHeadroom));
    assert_eq!(l.committed(), 40);
}

#[test]
fn queued_child_keeps_its_obligation_after_parent_exit_and_aged_new_work_cannot_deadlock_drain() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 10,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 2,
        domains: vec!["work".into()],
        envelopes: Default::default(),
    });
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.request(job("parent", "work", 1000, 10), &p).unwrap();
    advance(&mut l, &p, 250);
    let cap = l.continuation_capability("parent", &p).unwrap().unwrap();
    l.request(job("aged-new", "work", 1000, 50), &p).unwrap();
    let parent = l
        .authorize_continuation(&cap, 1000, "work", 20, 0, "child")
        .unwrap();
    let mut child = job("child", "work", 1000, 20);
    child.identity.inode = 2;
    child.continuation = Some(parent);
    l.request(child, &p).unwrap();
    l.prepare("intent".into(), "key".into(), 1000, "game", 300, &p)
        .unwrap();
    l.reconcile(|r| Some(r.id == "parent"));
    assert_eq!(l.continuations.len(), 1);
    assert_eq!(
        l.authorize_continuation(&cap, 1000, "work", 20, 0, "child")
            .unwrap(),
        "parent"
    );
    assert!(
        l.authorize_continuation(&cap, 1000, "work", 20, 0, "fresh-after-exit")
            .is_err()
    );
    let waits = advance(&mut l, &p, 1500);
    assert!(!waits.contains_key("child"), "{waits:?}");
    assert_eq!(l.preparations[0].phase, PreparationPhase::Draining);
    l.reconcile(|r| Some(r.id == "child"));
    advance(&mut l, &p, 1750);
    assert_eq!(l.preparations[0].phase, PreparationPhase::Ready);
}

#[test]
fn parents_reserve_completion_capacity_up_front_and_children_transfer_that_backing() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 10,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 3,
        domains: vec!["work".into()],
        envelopes: Default::default(),
    });
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.request(job("parent", "work", 1000, 10), &p).unwrap();
    advance(&mut l, &p, 250);
    assert_eq!(l.committed(), 30);
    let cap = l.continuation_capability("parent", &p).unwrap().unwrap();
    let mut child = job("child", "work", 1000, 20);
    child.identity.inode = 2;
    child.continuation = Some(
        l.authorize_continuation(&cap, 1000, "work", 20, 0, "child")
            .unwrap(),
    );
    l.request(child, &p).unwrap();
    advance(&mut l, &p, 500);
    assert_eq!(
        l.committed(),
        30,
        "transfer cannot charge an entitlement twice"
    );
    let mut second = job("second", "work", 1000, 20);
    second.identity.inode = 3;
    second.continuation = Some(
        l.authorize_continuation(&cap, 1000, "work", 20, 0, "second")
            .unwrap(),
    );
    l.request(second, &p).unwrap();
    assert_eq!(
        advance(&mut l, &p, 750)["second"],
        WaitReason::ContinuationSlot
    );
    l.reconcile(|r| Some(r.id == "child"));
    advance(&mut l, &p, 1000);
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "second")
            .unwrap()
            .granted
    );
    assert_eq!(l.committed(), 30);
}

#[test]
fn completion_lanes_are_backed_in_their_native_ancestors_before_the_parent_starts() {
    use amc_admission::continuation::ContinuationPolicy;
    let mut p = policy();
    p.domains[0].continuation = Some(ContinuationPolicy {
        parent_max_bytes: 10,
        memory_bytes: 20,
        swap_bytes: 0,
        max_calls: 2,
        domains: vec!["builders".into()],
        envelopes: Default::default(),
    });
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    let ancestry = |r: &Reservation, claims: &[Reservation]| {
        Some(if r.domain == "builders" {
            50u64.saturating_sub(
                claims
                    .iter()
                    .filter(|c| c.granted && c.domain == "builders")
                    .map(|c| c.memory_bytes)
                    .sum::<u64>(),
            )
        } else {
            u64::MAX
        })
    };
    let mut peer = job("native-peer", "builders", 0, 40);
    peer.identity.cgroup = "/builders/native-peer".into();
    l.request(peer, &p).unwrap();
    l.advance(
        250,
        &p,
        capacity(),
        &mut BTreeMap::from([("builders".into(), 0)]),
        ancestry,
    );
    l.request(job("parent", "work", 1000, 10), &p).unwrap();
    let waits = l.advance(
        500,
        &p,
        capacity(),
        &mut BTreeMap::from([("work".into(), 0)]),
        ancestry,
    );
    assert_eq!(waits["parent"], WaitReason::AncestorHeadroom);
    l.reconcile(|r| Some(r.id == "native-peer"));
    l.advance(
        750,
        &p,
        capacity(),
        &mut BTreeMap::from([("work".into(), 0)]),
        ancestry,
    );
    assert_eq!(l.committed(), 30);
    let mut peer = job("new-native-peer", "builders", 0, 40);
    peer.identity.cgroup = "/builders/new-native-peer".into();
    l.request(peer, &p).unwrap();
    let waits = l.advance(
        1000,
        &p,
        capacity(),
        &mut BTreeMap::from([("builders".into(), 0)]),
        ancestry,
    );
    assert_eq!(waits["new-native-peer"], WaitReason::AncestorHeadroom);
}

#[test]
fn asymmetric_completion_envelopes_preserve_each_childs_backing_and_authority() {
    let mut p = policy();
    p.domains[0].continuation = Some(
        serde_json::from_value(serde_json::json!({
            "parent_max_bytes":10,"memory_bytes":50,"swap_bytes":10,"max_calls":4,
            "domains":["work","game"],
            "envelopes":{"game":{"memory_bytes":40,"swap_bytes":0}}
        }))
        .unwrap(),
    );
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.request(job("parent", "work", 1000, 10), &p).unwrap();
    advance(&mut l, &p, 250);
    assert_eq!(l.committed(), 100);
    assert_eq!(l.swap_committed(), 10);
    let cap = l.continuation_capability("parent", &p).unwrap().unwrap();
    assert!(
        l.authorize_continuation(&cap, 1000, "game", 41, 0, "too-large")
            .is_err()
    );
    assert!(
        l.authorize_continuation(&cap, 1000, "game", 40, 1, "wrong-swap")
            .is_err()
    );
    let mut child = job("child", "game", 1000, 40);
    child.continuation = Some(
        l.authorize_continuation(&cap, 1000, "game", 40, 0, "child")
            .unwrap(),
    );
    l.request(child, &p).unwrap();
    advance(&mut l, &p, 500);
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "child")
            .unwrap()
            .granted
    );
    let mut restored: HostLedger =
        serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    restored.validate().unwrap();
    assert_eq!(restored.committed(), 100);
    assert_eq!(restored.swap_committed(), 10);
    assert_eq!(
        restored
            .authorize_continuation(&cap, 1000, "work", 50, 10, "builder")
            .unwrap(),
        "parent"
    );
}

#[test]
fn releasing_one_finite_root_worker_cannot_release_another_owner_or_replay_an_old_serial() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    let mut first = job("root", "builders", 0, 50);
    first.identity.cgroup = "/builders".into();
    let mut second = first.clone();
    second.id = "second-root".into();
    second.identity.pid = 43;
    l.request(first.clone(), &p).unwrap();
    advance(&mut l, &p, 250);
    l.request(second.clone(), &p).unwrap();
    l.bind_pool_operation("builders", &first.identity, 1)
        .unwrap();
    l.bind_pool_operation("builders", &second.identity, 1)
        .unwrap();
    l.release_pool_operation("builders", &first.identity, 2);
    assert_eq!(l.reservations[0].owners.len(), 2);
    l.release_pool_operation("builders", &first.identity, 1);
    assert_eq!(l.reservations[0].owners.len(), 1);
    assert!(!l.reservations[0].owners_finished);
    assert!(!l.owns_pool_operation("builders", &first.identity, Some(1)));
    assert!(l.owns_pool_operation("builders", &second.identity, Some(1)));
    l.release_pool_operation("builders", &second.identity, 1);
    assert!(l.reservations[0].owners_finished);
    l.reconcile(|_| None);
    assert_eq!(
        l.committed(),
        50,
        "operation end alone cannot prove descendant cleanup"
    );
    l.reconcile(|_| Some(true));
    assert_eq!(l.committed(), 0);
}

#[test]
fn nested_root_workers_keep_the_outer_operation_live_across_restart() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    let mut root = job("root", "builders", 0, 50);
    root.identity.cgroup = "/builders".into();
    l.request(root.clone(), &p).unwrap();
    advance(&mut l, &p, 250);
    l.bind_pool_operation("builders", &root.identity, 1)
        .unwrap();
    l.bind_pool_operation("builders", &root.identity, 2)
        .unwrap();
    let mut l: HostLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    l.validate().unwrap();
    l.release_pool_operation("builders", &root.identity, 2);
    assert!(l.owns_pool_operation("builders", &root.identity, Some(1)));
    assert!(!l.owns_pool_operation("builders", &root.identity, Some(2)));
    assert_eq!(l.reservations[0].owners.len(), 1);
    assert!(!l.reservations[0].owners_finished);
    l.release_pool_operation("builders", &root.identity, 1);
    assert!(l.reservations[0].owners.is_empty() && l.reservations[0].owners_finished);
}

#[test]
fn operation_keys_cover_the_longest_valid_root_pool_domain_and_native_identity() {
    let domain = "d".repeat(80);
    let mut p = policy();
    p.domains[2].name = domain.clone();
    p.preparations[0].drain_domains[1] = domain.clone();
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    let mut root = job("root", &domain, 0, 50);
    root.identity.cgroup = "/builders".into();
    root.identity.pid = i32::MAX;
    root.identity.start_ticks = u64::MAX;
    l.request(root.clone(), &p).unwrap();
    l.advance(
        250,
        &p,
        capacity(),
        &mut BTreeMap::from([(domain.clone(), 0)]),
        |_, _| Some(200),
    );
    assert!(l.reservations[0].granted);
    l.bind_pool_operation(&domain, &root.identity, u64::MAX)
        .unwrap();
    let mut l: HostLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    l.validate().unwrap();
    assert!(l.owns_pool_operation(&domain, &root.identity, Some(u64::MAX)));
    assert!(!l.owns_pool_operation(&domain, &root.identity, Some(1)));
    l.release_pool_operation(&domain, &root.identity, u64::MAX);
    assert!(l.reservations[0].owners.is_empty() && l.reservations[0].owners_finished);
}

#[test]
fn a_ready_claim_is_native_backing_until_its_once_only_consume_transfer() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.swap_return_bytes = Some(0);
    l.prepare("ready".into(), "key".into(), 1000, "game", 0, &p)
        .unwrap();
    advance(&mut l, &p, 250);
    let claims = l.native_claims(&p, None).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].memory_bytes, 40);
    assert_eq!(claims[0].identity.cgroup, "/game.slice");
    let native = job("ready", "game", 1000, 40);
    assert!(l.native_claims(&p, Some(&native)).unwrap().is_empty());
    l.consume("ready", "key", 1000, native, 500).unwrap();
    let claims = l.native_claims(&p, None).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].identity.pid, 42);
    assert_eq!(l.committed(), 40);
}
