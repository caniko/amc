use amc_admission::host::*;

#[test]
fn legacy_policies_keep_enforced_io_and_unknown_modes_are_rejected() {
    let mut json = serde_json::to_value(policy()).unwrap();
    for domain in json["domains"].as_array_mut().unwrap() {
        domain.as_object_mut().unwrap().remove("io_pressure");
        domain
            .as_object_mut()
            .unwrap()
            .remove("min_available_bytes");
    }
    let p: HostPolicy = serde_json::from_value(json.clone()).unwrap();
    assert!(
        p.domains
            .iter()
            .all(|d| d.io_pressure == IoPressure::Enforce && d.min_available_bytes == 0)
    );
    json["domains"][0]["io_pressure"] = serde_json::json!("disabled");
    assert!(serde_json::from_value::<HostPolicy>(json).is_err());
}

#[test]
fn diagnostic_io_domains_progress_without_bypassing_memory_or_peer_io_hysteresis() {
    let mut json = serde_json::to_value(policy()).unwrap();
    json["domains"][1]["io_pressure"] = serde_json::json!("diagnostic");
    json["domains"][1]["min_available_bytes"] = serde_json::json!(60);
    let p: HostPolicy = serde_json::from_value(json).unwrap();
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    l.request(job("builder", 1000, 10), &p).unwrap();
    l.request(job("evaluation", 1001, 10), &p).unwrap();
    let mut since = Default::default();
    let high_io = Some(Capacity {
        io_full_psi: 60.72,
        ..capacity().unwrap()
    });
    l.advance(0, &p, high_io, &mut since, |_, _| Some(200));
    let waits = l.advance(1500, &p, high_io, &mut since, |_, _| Some(200));
    assert_eq!(waits["builder"], WaitReason::Pressure);
    assert!(!waits.contains_key("evaluation"));
    assert_eq!(l.committed(), 10);
    // The enforced peer needs its own full recovery window, even though the
    // diagnostic domain has been healthy throughout the I/O spike.
    l.advance(1750, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(l.committed(), 10);
    l.advance(2000, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(l.committed(), 20);

    for c in [
        Capacity {
            memory_full_psi: 1.0,
            ..capacity().unwrap()
        },
        Capacity {
            memory_full_psi: f64::NAN,
            ..capacity().unwrap()
        },
        Capacity {
            available_bytes: 59,
            ..capacity().unwrap()
        },
    ] {
        let mut l = HostLedger::new("boot".into());
        let mut since = Default::default();
        l.request(job("evaluation", 1001, 10), &p).unwrap();
        l.advance(0, &p, Some(c), &mut since, |_, _| Some(200));
        assert_eq!(
            l.advance(250, &p, Some(c), &mut since, |_, _| Some(200))["evaluation"],
            WaitReason::Pressure
        );
        assert_eq!(l.committed(), 0);
    }
}

#[test]
fn unavailable_io_telemetry_blocks_only_enforced_domains() {
    let mut json = serde_json::to_value(policy()).unwrap();
    json["domains"][1]["io_pressure"] = serde_json::json!("diagnostic");
    let p: HostPolicy = serde_json::from_value(json).unwrap();
    for io in [f64::NAN, -1.0, 101.0] {
        let mut l = HostLedger::new("boot".into());
        let mut since = Default::default();
        l.request(job("builder", 1000, 10), &p).unwrap();
        l.request(job("evaluation", 1001, 10), &p).unwrap();
        let c = Some(Capacity {
            io_full_psi: io,
            ..capacity().unwrap()
        });
        l.advance(0, &p, c, &mut since, |_, _| Some(200));
        let waits = l.advance(250, &p, c, &mut since, |_, _| Some(200));
        assert_eq!(waits["builder"], WaitReason::Pressure);
        assert!(!waits.contains_key("evaluation"));
        assert_eq!(l.committed(), 10);
        l.request(job("missing-memory", 1001, 10), &p).unwrap();
        assert_eq!(
            l.advance(500, &p, None, &mut since, |_, _| Some(200))["missing-memory"],
            WaitReason::Unknown
        );
        assert_eq!(l.committed(), 10);
    }
}

fn policy() -> HostPolicy {
    HostPolicy {
        preparations: vec![],
        reserve_swap_return: false,
        swap_recovery: None,
        version: 1,
        budget_bytes: 80,
        reserve_bytes: 20,
        swap_reserve_bytes: 20,
        max_memory_full_psi: 1.0,
        max_io_full_psi: 20.0,
        resume_ms: 250,
        aging_ms: 1000,
        queue_limit: 32,
        burst: None,
        domains: (1000..=1003)
            .map(|uid| Domain {
                name: format!("tools-{uid}"),
                uid,
                cgroup: format!("/users/tools-{uid}"),
                ceiling_bytes: 80,
                swap_bytes: 40,
                fair_share_bytes: 40,
                io_pressure: IoPressure::Enforce,
                min_available_bytes: 0,
                continuation: None,
                burst: false,
            })
            .collect(),
    }
}
fn job(id: &str, uid: u32, bytes: u64) -> Reservation {
    Reservation {
        id: id.into(),
        domain: format!("tools-{uid}"),
        identity: Identity {
            cgroup: format!("/users/tools-{uid}/{id}"),
            inode: uid as u64,
            pid: uid as i32,
            uid,
            start_ticks: 1,
        },
        memory_bytes: bytes,
        swap_bytes: 0,
        requested_ms: 0,
        deadline_ms: 10_000,
        granted: false,
        owners: vec![],
        burst: false,
        runtime_max_ms: None,
        continuation: None,
        owners_finished: false,
    }
}
fn capacity() -> Option<Capacity> {
    Some(Capacity {
        available_bytes: 100,
        swap_free_bytes: 100,
        memory_full_psi: 0.0,
        io_full_psi: 0.0,
    })
}

fn healthy_since() -> std::collections::BTreeMap<String, u64> {
    policy().domains.into_iter().map(|d| (d.name, 0)).collect()
}

#[test]
fn swap_never_extends_ram_capacity_and_future_swap_growth_is_reserved() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    let mut r = job("work", 1000, 50);
    r.swap_bytes = 40;
    l.request(r, &p).unwrap();
    let c = Some(Capacity {
        swap_free_bytes: 50,
        ..capacity().unwrap()
    });
    assert_eq!(
        l.advance(250, &p, c, &mut since, |_, _| Some(100))["work"],
        WaitReason::SwapHeadroom
    );
    let c = Some(Capacity {
        available_bytes: 60,
        swap_free_bytes: 1_000_000,
        ..capacity().unwrap()
    });
    assert_eq!(
        l.advance(500, &p, c, &mut since, |_, _| Some(100))["work"],
        WaitReason::HostHeadroom
    );
}

#[test]
fn shared_ancestor_accounts_grants_made_earlier_in_the_same_tick() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("first", 1000, 30), &p).unwrap();
    l.request(job("second", 1001, 30), &p).unwrap();
    let waits = l.advance(250, &p, capacity(), &mut since, |_, entries| {
        Some(
            50 - entries
                .iter()
                .filter(|r| r.granted)
                .map(|r| r.memory_bytes)
                .sum::<u64>(),
        )
    });
    assert_eq!(l.committed(), 30);
    assert_eq!(waits["second"], WaitReason::AncestorHeadroom);
}

#[test]
fn idle_shares_are_lent_but_a_busy_participant_cannot_outrank_an_idle_peer() {
    let p = policy();
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("first", 1000, 40), &p).unwrap();
    l.advance(250, &p, capacity(), &mut since, |_, _| Some(200));
    l.request(job("borrow", 1000, 40), &p).unwrap();
    l.advance(500, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(l.committed(), 80); // Idle peers never strand reserved shares.
    l.reconcile(|r| Some(r.id == "borrow"));
    let mut a = job("busy", 1000, 40);
    a.requested_ms = 600;
    l.request(a, &p).unwrap();
    let mut b = job("idle", 1001, 40);
    b.requested_ms = 610;
    l.request(b, &p).unwrap();
    l.advance(750, &p, capacity(), &mut since, |_, _| Some(200));
    assert!(
        l.reservations
            .iter()
            .find(|r| r.id == "idle")
            .unwrap()
            .granted
    );
    assert!(
        !l.reservations
            .iter()
            .find(|r| r.id == "busy")
            .unwrap()
            .granted
    );
}

#[test]
fn policy_reduction_preserves_live_grants_and_forbids_more() {
    let mut p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("first", 1000, 60), &p).unwrap();
    l.advance(250, &p, capacity(), &mut since, |_, _| Some(200));
    p.budget_bytes = 50;
    l.request(job("second", 1001, 10), &p).unwrap();
    assert_eq!(
        l.advance(500, &p, capacity(), &mut since, |_, _| Some(200))["second"],
        WaitReason::Budget
    );
    assert_eq!(l.committed(), 60);
}

#[test]
fn duplicate_native_requests_do_not_allocate_twice_or_change_the_ceiling() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("first", 1000, 40), &p).unwrap();
    let mut retry = job("first", 1000, 40);
    retry.id = "retry".into();
    assert_eq!(l.request(retry.clone(), &p).unwrap(), "first");
    retry.memory_bytes = 50;
    assert!(l.request(retry, &p).is_err());
    let mut foreign = job("foreign", 1001, 40);
    foreign.identity.cgroup = "/users/tools-1000/foreign".into();
    assert!(l.request(foreign, &p).is_err());
    assert_eq!(l.reservations.len(), 1);
}

#[test]
fn expired_ungranted_work_cannot_claim_capacity() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("first", 1000, 40), &p).unwrap();
    l.advance(10_000, &p, capacity(), &mut since, |_, _| Some(200));
    assert!(l.reservations.is_empty());
}

#[test]
fn independent_root_handlers_join_one_bounded_pool_without_releasing_each_other() {
    let mut p = policy();
    p.domains.push(Domain {
        name: "builders".into(),
        uid: 0,
        cgroup: "/builders".into(),
        ceiling_bytes: 40,
        swap_bytes: 0,
        fair_share_bytes: 40,
        io_pressure: IoPressure::Enforce,
        min_available_bytes: 0,
        continuation: None,
        burst: false,
    });
    p.validate().unwrap();
    let mut l = HostLedger::new("boot".into());
    let mut r = job("first", 1000, 40);
    r.domain = "builders".into();
    r.identity.uid = 0;
    r.identity.cgroup = "/builders".into();
    l.request(r.clone(), &p).unwrap();
    let mut since = healthy_since();
    since.insert("builders".into(), 0);
    l.advance(250, &p, capacity(), &mut since, |_, _| Some(200));
    r.id = "second".into();
    r.identity.pid = 1001;
    assert_eq!(l.request(r.clone(), &p).unwrap(), "first");
    assert_eq!(l.request(r, &p).unwrap(), "first");
    assert_eq!(l.committed(), 40);
    assert_eq!(l.reservations.len(), 1);
    assert_eq!(l.reservations[0].owners.len(), 2);
    l.validate().unwrap();
    let mut restored: HostLedger =
        serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    restored.reconcile(|_| None);
    restored.advance(20_000, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(restored.committed(), 40);
    assert_eq!(restored.reservations[0].owners.len(), 2);
}

#[test]
fn exhausted_host_swap_inhibits_even_a_zero_swap_contract_and_recovery_is_debounced() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    l.request(job("no-swap", 1000, 10), &p).unwrap();
    let mut since = healthy_since();
    let mut c = capacity().unwrap();
    c.swap_free_bytes = p.swap_reserve_bytes - 1;
    assert_eq!(
        l.advance(250, &p, Some(c), &mut since, |_, _| Some(200))["no-swap"],
        WaitReason::Pressure
    );
    assert_eq!(l.committed(), 0);
    l.advance(500, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(l.committed(), 0);
    l.advance(750, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(l.committed(), 10);
}

#[test]
fn simultaneous_users_cannot_spend_the_same_capacity() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("can", 1000, 50), &p).unwrap();
    l.request(job("dejana", 1001, 50), &p).unwrap();
    let waits = l.advance(250, &p, capacity(), &mut since, |_, _| Some(100));
    assert_eq!(l.committed(), 50);
    assert_eq!(waits["dejana"], WaitReason::Budget);
    l.reconcile(|r| Some(r.identity.uid == 1000));
    l.advance(500, &p, capacity(), &mut since, |_, _| Some(100));
    assert_eq!(l.committed(), 50);
}

#[test]
fn lost_clients_unknown_cleanup_and_deadlines_never_release_running_work() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("work", 1000, 50), &p).unwrap();
    l.advance(250, &p, capacity(), &mut since, |_, _| Some(100));
    l.reconcile(|_| None);
    l.advance(20_000, &p, None, &mut since, |_, _| None);
    assert_eq!(l.committed(), 50);
    let restored: HostLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
    assert_eq!(restored.committed(), 50);
    l.reconcile(|_| Some(true));
    assert_eq!(l.committed(), 0);
}

#[test]
fn small_jobs_backfill_until_an_older_large_request_ages() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("running", 1000, 50), &p).unwrap();
    l.advance(250, &p, capacity(), &mut since, |_, _| Some(100));
    l.request(job("large", 1001, 40), &p).unwrap();
    l.request(job("small", 1002, 20), &p).unwrap();
    l.advance(500, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(l.committed(), 70);
    l.request(job("late", 1003, 5), &p).unwrap();
    let waits = l.advance(1500, &p, capacity(), &mut since, |_, _| Some(200));
    assert_eq!(waits["late"], WaitReason::AgedRequest);
}

#[test]
fn pressure_inhibits_immediately_and_recovery_is_debounced() {
    let p = policy();
    let mut l = HostLedger::new("boot".into());
    let mut since = healthy_since();
    l.request(job("work", 1000, 50), &p).unwrap();
    let c = Some(Capacity {
        memory_full_psi: 1.0,
        ..capacity().unwrap()
    });
    assert_eq!(
        l.advance(250, &p, c, &mut since, |_, _| Some(100))["work"],
        WaitReason::Pressure
    );
    l.advance(500, &p, capacity(), &mut since, |_, _| Some(100));
    assert_eq!(l.committed(), 0);
    l.advance(750, &p, capacity(), &mut since, |_, _| Some(100));
    assert_eq!(l.committed(), 50);
}
