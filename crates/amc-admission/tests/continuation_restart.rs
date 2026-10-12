use amc_admission::host::{Capacity, HostLedger, HostPolicy, Reservation};
use std::collections::BTreeMap;

#[test]
fn removing_a_child_domain_revokes_escrow_without_releasing_native_grants() {
    let policy: HostPolicy = serde_json::from_value(serde_json::json!({
        "version":1,"budget_bytes":100,"reserve_bytes":20,"swap_reserve_bytes":0,
        "max_memory_full_psi":1.0,"max_io_full_psi":20.0,
        "resume_ms":250,"aging_ms":1000,"queue_limit":32,
        "domains":[{"name":"other","uid":1000,"cgroup":"/other.slice",
            "ceiling_bytes":40,"swap_bytes":0,"fair_share_bytes":40}]
    }))
    .unwrap();
    let mut ledger = HostLedger::new("boot".into());
    for (id, domain, memory, granted, continuation) in [
        ("parent", "parent", 40, true, None),
        ("active-child", "removed", 10, true, Some("parent")),
        ("queued-child", "removed", 20, false, Some("parent")),
        ("unrelated", "other", 20, false, None),
    ] {
        let reservation: Reservation = serde_json::from_value(serde_json::json!({
            "id":id,"domain":domain,"identity":{"uid":1000,"pid":42,"start_ticks":7,
                "inode":1,"cgroup":format!("/{domain}.slice/{id}.service")},
            "memory_bytes":memory,"swap_bytes":0,"requested_ms":0,"deadline_ms":10000,
            "granted":granted,"owners":[],"owners_finished":false,"burst":false,
            "runtime_max_ms":null,"continuation":continuation
        }))
        .unwrap();
        ledger.reservations.push(reservation);
    }
    ledger.continuations.push(
        serde_json::from_value(serde_json::json!({
            "capability":{"parent":"parent","key":"completion-key"},"uid":1000,
            "policy":{"parent_max_bytes":40,"memory_bytes":30,"swap_bytes":0,
                "max_calls":4,"domains":["removed"]},
            "calls":["active-child","queued-child"]
        }))
        .unwrap(),
    );
    // Recreate the durable restart rather than mutating a fresh in-memory grant.
    let mut restored: HostLedger =
        serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
    let mut healthy = BTreeMap::from([("other".into(), 0)]);
    let waiting = restored.advance(
        250,
        &policy,
        Some(Capacity {
            available_bytes: 1000,
            swap_free_bytes: 100,
            memory_full_psi: 0.0,
            io_full_psi: 0.0,
        }),
        &mut healthy,
        |_, _| Some(1000),
    );
    assert!(
        restored.native_claims(&policy, None).is_some(),
        "a revoked child domain must not make every backing projection unavailable"
    );
    assert!(restored.continuations.is_empty());
    assert!(!restored.reservations.iter().any(|r| r.id == "queued-child"));
    assert!(
        restored
            .reservations
            .iter()
            .any(|r| r.id == "parent" && r.granted)
    );
    assert!(
        restored
            .reservations
            .iter()
            .any(|r| r.id == "active-child" && r.granted)
    );
    assert!(
        restored
            .reservations
            .iter()
            .any(|r| r.id == "unrelated" && r.granted),
        "{waiting:?}"
    );
    assert_eq!(restored.committed(), 70);
}
