use amc_supervision::{
    forecast::Settings,
    policy::{Domain, Lifecycle, Mode, Policy},
    recovery::{Action, Identity, Phase, State},
};

fn policy() -> Policy {
    Policy {
        version: 1,
        mode: Mode::Enforce,
        forecast_recovery: false,
        reserve_bytes: 100,
        emergency_available_bytes: 20,
        emergency_full_psi: 0.1,
        term_ms: 1000,
        kill_ms: 1000,
        cooldown_ms: 1000,
        recovery_window_ms: 60_000,
        domain_recovery_limit: 2,
        host_recovery_limit: 3,
        forecast: Settings {
            horizon: 10,
            calibration: 32,
            alpha: 0.1,
            max_gap_ms: 1500,
        },
        domains: vec![domain(Lifecycle::Restart)],
        job_pools: vec![],
    }
}
fn domain(lifecycle: Lifecycle) -> Domain {
    Domain {
        id: "backend".into(),
        unit: "backend.service".into(),
        uid: Some(1000),
        lifecycle,
        memory_max: 1024,
        memory_swap_max: 0,
        priority: 10,
        expected: None,
    }
}
fn identity() -> Identity {
    Identity {
        invocation: "a".repeat(32),
        cgroup: "/user.slice/backend.service".into(),
        inode: 1,
        pid: 1,
        start_ticks: 1,
    }
}
fn state() -> State {
    State::new("a0e7b901-008c-479c-bf78-3fd6a6fb408d".into())
}

#[test]
fn unknown_termination_escalates_then_trips_at_real_deadlines() {
    let p = policy();
    let mut s = state();
    s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
        .unwrap();
    assert_eq!(s.advance(1099, None, false, true, false, &p), Action::None);
    assert_eq!(s.advance(1100, None, false, true, false, &p), Action::Kill);
    assert_eq!(s.advance(2100, None, false, true, false, &p), Action::Trip);
    assert_eq!(s.active.unwrap().phase, Phase::Tripped);
}
#[test]
fn disposable_job_can_never_restart_and_full_termination_is_required() {
    let p = policy();
    let mut s = state();
    s.begin(domain(Lifecycle::Terminate), identity(), 100, 100, &p)
        .unwrap();
    assert_eq!(
        s.advance(200, Some(false), false, true, false, &p),
        Action::None
    );
    assert_eq!(
        s.advance(300, Some(true), false, true, false, &p),
        Action::None
    );
    assert_eq!(
        s.advance(1300, Some(true), false, true, false, &p),
        Action::Finished
    );
    assert!(s.active.is_none());
    assert_eq!(s.attempts.len(), 1);
}
#[test]
fn backend_needs_continuous_healthy_cooldown_then_verified_new_invocation() {
    let p = policy();
    let mut s = state();
    s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
        .unwrap();
    s.advance(200, Some(true), false, true, false, &p);
    s.advance(1199, Some(true), false, false, false, &p);
    assert_eq!(
        s.advance(1200, Some(true), false, true, false, &p),
        Action::None
    );
    assert_eq!(
        s.advance(2199, Some(true), false, true, false, &p),
        Action::Start
    );
    assert_eq!(
        s.advance(2200, Some(false), true, true, true, &p),
        Action::Finished
    );
}
#[test]
fn replacement_during_stop_never_authorizes_restart() {
    let p = policy();
    let mut s = state();
    s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
        .unwrap();
    assert_eq!(
        s.advance(200, Some(true), true, true, false, &p),
        Action::Trip
    );
}
#[test]
fn crash_boot_policy_change_and_clock_rollback_do_not_forgive_budget() {
    let p = policy();
    let mut s = state();
    s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
        .unwrap();
    let mut loaded: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    loaded.reconcile_startup("ed447a46-39ce-496e-b58c-c6e029e23768");
    assert_eq!(loaded.active.as_ref().unwrap().phase, Phase::Tripped);
    assert_eq!(loaded.attempts.len(), 1);
    loaded.active = None;
    assert!(
        loaded
            .begin(domain(Lifecycle::Restart), identity(), 99, 1, &p)
            .is_err()
    );
    loaded
        .begin(domain(Lifecycle::Restart), identity(), 101, 1, &p)
        .unwrap();
    loaded.active = None;
    assert!(
        loaded
            .begin(domain(Lifecycle::Restart), identity(), 102, 1, &p)
            .is_err()
    );
    let mut shorter = p;
    shorter.recovery_window_ms = 1;
    assert!(
        loaded
            .begin(domain(Lifecycle::Restart), identity(), 10_000, 1, &shorter)
            .is_err()
    );
}
#[test]
fn brokers_have_no_signal_authority_and_only_one_host_recovery_can_run() {
    let p = policy();
    let mut s = state();
    assert!(
        s.begin(domain(Lifecycle::Observe), identity(), 100, 100, &p)
            .is_err()
    );
    s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
        .unwrap();
    assert!(
        s.begin(domain(Lifecycle::Terminate), identity(), 100, 100, &p)
            .is_err()
    );
}

#[test]
fn unhealthy_cooldown_is_deadline_bound_and_finished_jobs_release_the_recovery_slot() {
    let p = policy();
    let mut backend = state();
    backend
        .begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
        .unwrap();
    backend.advance(200, Some(true), false, false, false, &p);
    assert_eq!(
        backend.advance(61_200, Some(true), false, false, false, &p),
        Action::Trip
    );
    let mut job = state();
    job.begin(domain(Lifecycle::Terminate), identity(), 100, 100, &p)
        .unwrap();
    job.advance(200, Some(true), false, false, false, &p);
    assert_eq!(
        job.advance(1200, Some(true), false, false, false, &p),
        Action::Finished
    );
}

#[test]
fn a_healthy_replacement_at_or_after_the_start_deadline_trips() {
    let p = policy();
    for observed_at in [11_200, 11_201] {
        let mut s = state();
        s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
            .unwrap();
        s.advance(200, Some(true), false, true, false, &p);
        assert_eq!(
            s.advance(1200, Some(true), false, true, false, &p),
            Action::Start
        );
        assert_eq!(
            s.advance(observed_at, Some(false), true, true, true, &p),
            Action::Trip
        );
        assert_eq!(s.active.unwrap().phase, Phase::Tripped);
    }
}

#[test]
fn new_job_tickets_share_the_persisted_pool_recovery_budget() {
    use amc_supervision::policy::JobPool;
    let mut p = policy();
    p.domain_recovery_limit = 1;
    p.job_pools.push(JobPool {
        id: "jobs".into(),
        uid: 1000,
        state: "/state/jobs".into(),
        contracts: vec!["tool".into()],
        lifecycle: Lifecycle::Terminate,
        priority: 0,
    });
    let job = |ticket: &str| {
        let mut d = domain(Lifecycle::Terminate);
        d.id = format!("jobs-{ticket}");
        d.unit = format!("app-amc-job-{ticket}.service");
        d.expected = Some(amc_admission::ledger::Identity {
            invocation: "a".repeat(32),
            cgroup: "/job".into(),
            inode: 1,
        });
        d
    };
    let mut s = state();
    s.begin(
        job("a0e7b901-008c-479c-bf78-3fd6a6fb408d"),
        identity(),
        100,
        100,
        &p,
    )
    .unwrap();
    s.advance(200, Some(true), false, true, false, &p);
    assert_eq!(
        s.advance(1200, Some(true), false, true, false, &p),
        Action::Finished
    );
    let mut restored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(restored.attempts[0].pool.as_deref(), Some("jobs"));
    assert!(
        restored
            .begin(
                job("ed447a46-39ce-496e-b58c-c6e029e23768"),
                identity(),
                101,
                1201,
                &p
            )
            .is_err()
    );
    assert_eq!(restored.attempts.len(), 1);
    let mut legacy = serde_json::to_value(&s).unwrap();
    legacy["attempts"][0]
        .as_object_mut()
        .unwrap()
        .remove("pool");
    let mut restored: State = serde_json::from_value(legacy).unwrap();
    assert!(
        restored
            .begin(
                job("ed447a46-39ce-496e-b58c-c6e029e23768"),
                identity(),
                101,
                1201,
                &p
            )
            .is_err()
    );
    assert_eq!(restored.attempts[0].pool.as_deref(), Some("jobs"));

    // The published predecessor stored the normalized pool directly as domain.
    let mut legacy = serde_json::to_value(&s).unwrap();
    legacy["attempts"][0]
        .as_object_mut()
        .unwrap()
        .remove("pool");
    legacy["attempts"][0]["domain"] = serde_json::json!("jobs");
    let mut restored: State = serde_json::from_value(legacy).unwrap();
    assert!(
        restored
            .begin(
                job("ed447a46-39ce-496e-b58c-c6e029e23768"),
                identity(),
                101,
                1201,
                &p,
            )
            .is_err()
    );
    assert_eq!(restored.attempts.len(), 1);
    assert_eq!(restored.attempts[0].pool.as_deref(), Some("jobs"));
}

#[test]
fn static_domain_names_cannot_be_reclassified_as_pool_history() {
    use amc_supervision::policy::JobPool;
    let mut p = policy();
    p.domain_recovery_limit = 1;
    p.domains[0].id = "jobs-a0e7b901-008c-479c-bf78-3fd6a6fb408d".into();
    p.job_pools.push(JobPool {
        id: "jobs".into(),
        uid: 1000,
        state: "/state/jobs".into(),
        contracts: vec!["tool".into()],
        lifecycle: Lifecycle::Terminate,
        priority: 0,
    });
    p.validate().unwrap();
    let mut s = state();
    s.begin(p.domains[0].clone(), identity(), 100, 100, &p)
        .unwrap();
    s.advance(200, Some(true), false, true, false, &p);
    s.advance(1200, Some(true), false, true, false, &p);
    assert_eq!(
        s.advance(1201, Some(false), true, true, true, &p),
        Action::Finished
    );
    let mut restored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert!(
        restored
            .begin(p.domains[0].clone(), identity(), 101, 1202, &p)
            .is_err()
    );
    assert_eq!(restored.attempts.len(), 1);
    assert!(restored.attempts[0].pool.is_none());
}

#[test]
fn cold_failed_backend_uses_durable_cooldown_and_never_replays_after_restart() {
    use amc_supervision::recovery::{FailedIdentity, RecoveryIdentity};
    let p = policy();
    let mut s = state();
    s.begin_failed(
        domain(Lifecycle::Restart),
        FailedIdentity {
            invocation: "a".repeat(32),
            parent: "/user.slice".into(),
            parent_inode: 1,
            cgroup: "/user.slice/backend.service".into(),
            inode: None,
        },
        100,
        100,
        &p,
    )
    .unwrap();
    s.validate().unwrap();
    assert!(matches!(
        s.active.as_ref().unwrap().identity,
        RecoveryIdentity::Failed(_)
    ));
    assert_eq!(
        s.advance(1099, Some(true), false, true, false, &p),
        Action::None
    );
    let mut restarted: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    restarted.reconcile_startup(&s.boot_id);
    assert_eq!(
        restarted.advance(1100, Some(true), false, true, false, &p),
        Action::None
    );
    assert_eq!(restarted.active.as_ref().unwrap().phase, Phase::Tripped);
    assert_eq!(restarted.attempts.len(), 1);
    assert_eq!(
        s.advance(1100, Some(true), false, true, false, &p),
        Action::Start
    );
    assert_eq!(
        s.advance(1101, Some(false), true, true, true, &p),
        Action::Finished
    );
    assert_eq!(s.attempts.len(), 1);
}

#[test]
fn lost_cleanup_evidence_during_cooldown_revokes_restart() {
    let p = policy();
    for empty in [None, Some(false)] {
        let mut s = state();
        s.begin(domain(Lifecycle::Restart), identity(), 100, 100, &p)
            .unwrap();
        s.advance(200, Some(true), false, true, false, &p);
        assert_eq!(s.advance(1200, empty, false, true, false, &p), Action::Trip);
        assert_eq!(s.active.as_ref().unwrap().phase, Phase::Tripped);
        assert_eq!(
            s.advance(1201, Some(true), false, true, false, &p),
            Action::None
        );
        assert_eq!(s.attempts.len(), 1);
    }
}

#[test]
fn chronological_replay_reproduces_growth_and_rejects_reordered_frames() {
    use amc_supervision::{native::Boundary, replay};
    let mut trace = String::new();
    for t in 0..150 {
        let b = Boundary {
            key: "memory.current".into(),
            cgroup: "/fixture".into(),
            inode: 1,
            current_bytes: 100 + t * 6,
            limit_bytes: 1000,
        };
        trace.push_str(&serde_json::to_string(&serde_json::json!({"version":1,"boot_id":"a0e7b901-008c-479c-bf78-3fd6a6fb408d",
            "observed_boot_ms":t*1000,"policy":policy(),"domains":[{"id":"backend","status":"observed","identity":identity(),"boundaries":[b]}]})).unwrap());
        trace.push('\n');
    }
    let summary = replay::run(std::io::Cursor::new(&trace)).unwrap();
    assert_eq!(summary["backend"].observations, 150);
    assert!(summary["backend"].strong > 0);
    let reversed = trace.lines().rev().collect::<Vec<_>>().join("\n");
    assert!(replay::run(std::io::Cursor::new(reversed)).is_err());
}
