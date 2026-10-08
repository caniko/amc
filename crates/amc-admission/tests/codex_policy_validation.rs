use amc_admission::{host::HostPolicy, ledger::Policy};

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
            {"name":"game", "uid":1000, "cgroup":"/game.slice",
             "ceiling_bytes":40, "swap_bytes":0, "fair_share_bytes":50,
             "continuation":{"parent_max_bytes":40, "memory_bytes":10,
                 "swap_bytes":0, "max_calls":8, "domains":["work"]}}
        ],
        "preparations":[{"name":"game", "domain":"game", "memory_bytes":40,
            "swap_bytes":0, "drain_domains":["work"], "wait_ms":10000, "ready_ms":1000}]
    }))
    .unwrap();
    let profile = policy.preparations.pop().unwrap();
    policy.validate().unwrap();
    assert!(profile.validate(&policy).is_err());
    policy.domains[1].continuation = None;
    profile.validate(&policy).unwrap();
}
