use amc_admission::{host::HostPolicy, ledger::Policy};

fn host_policy() -> HostPolicy {
    serde_json::from_value(serde_json::json!({
        "version":1, "budget_bytes":100, "reserve_bytes":20, "swap_reserve_bytes":10,
        "max_memory_full_psi":1.0, "max_io_full_psi":20.0,
        "resume_ms":250, "aging_ms":1000, "queue_limit":32,
        "domains":[
            {"name":"work", "uid":1000, "cgroup":"/work.slice",
             "ceiling_bytes":50, "swap_bytes":10, "fair_share_bytes":50},
            {"name":"game", "uid":1000, "cgroup":"/parent/game.slice",
             "ceiling_bytes":40, "swap_bytes":10, "fair_share_bytes":50}
        ]
    }))
    .unwrap()
}

#[test]
fn child_envelope_overrides_are_finite_selected_reductions() {
    let base = serde_json::json!({
        "parent_max_bytes":10,"memory_bytes":50,"swap_bytes":10,"max_calls":1,
        "domains":["work","game"],
        "envelopes":{"game":{"memory_bytes":40,"swap_bytes":10}}
    });
    let mut policy = host_policy();
    policy.domains[0].continuation = Some(serde_json::from_value(base.clone()).unwrap());
    policy.validate().unwrap();
    for overrides in [
        serde_json::json!({"other":{"memory_bytes":1,"swap_bytes":0}}),
        serde_json::json!({"game":{"memory_bytes":0,"swap_bytes":0}}),
        serde_json::json!({"game":{"memory_bytes":51,"swap_bytes":0}}),
        serde_json::json!({"game":{"memory_bytes":40,"swap_bytes":11}}),
        serde_json::json!({"game":{"memory_bytes":41,"swap_bytes":0}}),
    ] {
        let mut invalid = base.clone();
        invalid["envelopes"] = overrides;
        policy.domains[0].continuation = Some(serde_json::from_value(invalid).unwrap());
        assert!(policy.validate().is_err());
    }
}

#[test]
fn root_completion_envelopes_match_the_fixed_native_pool_request() {
    let mut policy = host_policy();
    policy.domains[1].uid = 0;
    policy.domains[0].continuation = Some(
        serde_json::from_value(serde_json::json!({
            "parent_max_bytes":10,"memory_bytes":50,"swap_bytes":10,"max_calls":1,
            "domains":["game"],"envelopes":{"game":{"memory_bytes":40,"swap_bytes":10}}
        }))
        .unwrap(),
    );
    policy.validate().unwrap();
    policy.domains[0]
        .continuation
        .as_mut()
        .unwrap()
        .envelopes
        .get_mut("game")
        .unwrap()
        .memory_bytes = 39;
    assert!(policy.validate().is_err());
    let envelope = policy.domains[0]
        .continuation
        .as_mut()
        .unwrap()
        .envelopes
        .get_mut("game")
        .unwrap();
    envelope.memory_bytes = 40;
    envelope.swap_bytes = 9;
    assert!(policy.validate().is_err());
}

#[test]
fn preparation_profiles_require_the_slice_units_complete_hierarchy() {
    let mut policy = host_policy();
    policy.reserve_bytes = 67108864;
    policy.namespace_runner_bytes = 67108864;
    policy.preparations = serde_json::from_value(serde_json::json!([
        {"name":"game", "domain":"game", "memory_bytes":40, "swap_bytes":0,
         "drain_domains":["work"], "wait_ms":10000, "ready_ms":15000}
    ]))
    .unwrap();
    for group in [
        "/game.slice",
        "/app.slice/app-game.slice",
        "/app.slice/app-game.slice/app-game-child.slice",
        "/app.slice/game.slice",
        "/app-game.slice",
        "/app.slice/app-game.slice/app-other.slice",
        "/user.slice/user-1000.slice/user@1000.service/app.slice/game.slice",
        "/user.slice/user-1001.slice/user@1001.service/game.slice",
    ] {
        policy.domains[1].cgroup = group.into();
        assert!(
            policy.validate().is_err(),
            "accepted a launch slice with a different hierarchy: {group}"
        );
    }
    for group in [
        "/user.slice/user-1000.slice/user@1000.service/game.slice",
        "/user.slice/user-1000.slice/user@1000.service/app.slice/app-game.slice",
        "/user.slice/user-1000.slice/user@1000.service/app.slice/app-game.slice/app-game-child.slice",
    ] {
        policy.domains[1].cgroup = group.into();
        policy.validate().unwrap();
    }
}

#[test]
fn preparation_profiles_require_an_executable_launch_slice_basename() {
    let mut policy = host_policy();
    policy.reserve_bytes = 67108864;
    policy.namespace_runner_bytes = 67108864;
    policy.preparations = serde_json::from_value(serde_json::json!([
        {"name":"game", "domain":"game", "memory_bytes":40, "swap_bytes":0,
         "drain_domains":["work"], "wait_ms":10000, "ready_ms":15000}
    ]))
    .unwrap();
    for basename in ["a".repeat(80), "game".into()] {
        policy.domains[1].cgroup =
            format!("/user.slice/user-1000.slice/user@1000.service/{basename}.slice");
        policy.validate().unwrap();
    }
    for basename in ["a".repeat(81), "game\\x20name".into()] {
        policy.domains[1].cgroup =
            format!("/user.slice/user-1000.slice/user@1000.service/{basename}.slice");
        assert!(policy.validate().is_err(), "{basename}");
    }
}

#[test]
fn completion_lanes_fit_every_selected_child_domain_envelope() {
    let mut policy = host_policy();
    policy.domains[1].continuation = Some(
        serde_json::from_value(serde_json::json!({
            "parent_max_bytes":20, "memory_bytes":50, "swap_bytes":10,
            "max_calls":8, "domains":["work"]
        }))
        .unwrap(),
    );
    policy.validate().unwrap();
    policy.domains[1]
        .continuation
        .as_mut()
        .unwrap()
        .memory_bytes = 51;
    assert!(policy.validate().is_err());
    policy.domains[1]
        .continuation
        .as_mut()
        .unwrap()
        .memory_bytes = 50;
    policy.domains[1].continuation.as_mut().unwrap().swap_bytes = 11;
    assert!(policy.validate().is_err());
}

#[test]
fn recovery_helpers_fit_the_enclosing_host_budget() {
    let mut policy = host_policy();
    policy.swap_recovery = Some(
        serde_json::from_value(serde_json::json!({
            "cgroup":"/recovery.service", "helper_bytes":100, "minimum_bytes":1,
            "targets":[], "page_cgroups":["/work.slice"]
        }))
        .unwrap(),
    );
    policy.validate().unwrap();
    policy.swap_recovery.as_mut().unwrap().helper_bytes = 101;
    assert!(policy.validate().is_err());
}

#[test]
fn a_native_slice_cannot_mix_burst_and_ordinary_contracts() {
    let mut policy: Policy = serde_json::from_value(serde_json::json!({
        "version":1, "budget_bytes":100, "reserve_bytes":10, "queue_limit":32,
        "burst_budget_bytes":20,
        "contracts": {
            "ordinary":{"slice":"jobs.slice", "memory_max":20,
                "memory_swap_max":0, "max_running":1, "pause_file":null},
            "burst":{"slice":"burst.slice", "memory_max":10,
                "memory_swap_max":0, "max_running":1, "pause_file":null,
                "burst":true, "runtime_max_sec":5}
        }
    }))
    .unwrap();
    policy.validate().unwrap();
    policy.contracts.get_mut("burst").unwrap().slice = "jobs.slice".into();
    assert!(policy.validate().is_err());
    policy.contracts.get_mut("ordinary").unwrap().burst = true;
    policy
        .contracts
        .get_mut("ordinary")
        .unwrap()
        .runtime_max_sec = Some(5);
    policy.validate().unwrap();
}

#[test]
fn preparation_rejects_a_target_whose_completion_rights_it_cannot_mint() {
    let mut policy: HostPolicy = serde_json::from_value(serde_json::json!({
        "version":1, "budget_bytes":100, "reserve_bytes":20, "swap_reserve_bytes":10,
        "max_memory_full_psi":1.0, "max_io_full_psi":20.0,
        "resume_ms":250, "aging_ms":1000, "queue_limit":32,
        "domains":[
            {"name":"work", "uid":1000, "cgroup":"/work.slice",
             "ceiling_bytes":50, "swap_bytes":0, "fair_share_bytes":50},
            {"name":"game", "uid":1000,
             "cgroup":"/user.slice/user-1000.slice/user@1000.service/game.slice",
             "ceiling_bytes":40, "swap_bytes":0, "fair_share_bytes":50,
             "continuation":{"parent_max_bytes":40, "memory_bytes":10,
                 "swap_bytes":0, "max_calls":8, "domains":["work"]}}
        ],
        "preparations":[{"name":"game", "domain":"game", "memory_bytes":40,
            "swap_bytes":0, "drain_domains":["work"], "wait_ms":10000, "ready_ms":15000}]
    }))
    .unwrap();
    let profile = policy.preparations.pop().unwrap();
    policy.validate().unwrap();
    assert!(profile.validate(&policy).is_err());
    policy.domains[1].continuation = None;
    profile.validate(&policy).unwrap();
}
