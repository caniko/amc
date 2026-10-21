use amc_admission::ledger::{Backpressure, Headroom, Ledger, Phase, Policy, QueueRequest};

fn policy() -> Policy {
    serde_json::from_value(serde_json::json!({
        "version": 1, "budget_bytes": 1000, "reserve_bytes": 100, "queue_limit": 2,
        "contracts": {"tool": {
            "slice": "agent-tools.slice", "memory_max": 600,
            "memory_swap_max": 0, "max_running": 2, "pause_file": null
        }}
    }))
    .unwrap()
}

fn request(wait_ms: Option<u64>) -> QueueRequest {
    QueueRequest {
        memory_max: 600,
        runtime_max_sec: 120,
        wait_ms,
    }
}

fn capacity(bytes: u64) -> Option<Headroom> {
    Some(Headroom {
        shared_bytes: bytes,
        slice_bytes: bytes,
        memory_max: bytes,
        memory_swap_max: 0,
        paused: false,
    })
}

#[test]
fn resource_wait_has_no_implicit_expiry_but_native_entry_is_still_bounded() {
    let p = policy();
    let mut ledger = Ledger::new("boot".into());
    ledger
        .enqueue_waiting("waiting".into(), "tool", &p, 1000, request(None))
        .unwrap();
    ledger.advance(1000, &p, |_| capacity(100));
    let later = 7_201_000;
    ledger.advance(later, &p, |_| capacity(100));
    let entry = ledger.get("waiting").unwrap();
    assert_eq!(entry.phase, Phase::Queued);
    assert_eq!(entry.queue.as_ref().unwrap().submitted_ms, 1000);
    assert_eq!(entry.queue.as_ref().unwrap().deadline_ms, None);
    assert_eq!(entry.contract.runtime_max_sec, Some(120));
    assert_eq!(ledger.committed(), 0);

    ledger.advance(later, &p, |_| capacity(2000));
    let entry = ledger.get("waiting").unwrap();
    assert_eq!(entry.phase, Phase::Reserved);
    assert_eq!(entry.deadline_ms, later + 30_000);
    assert_eq!(entry.contract.runtime_max_sec, Some(120));
    ledger.advance(later + 30_000, &p, |_| capacity(2000));
    assert!(ledger.get("waiting").is_none());
}

#[test]
fn an_explicit_queue_deadline_is_independent_of_the_entry_grace() {
    let p = policy();
    let mut ledger = Ledger::new("boot".into());
    ledger
        .enqueue_waiting("expired".into(), "tool", &p, 1000, request(Some(100)))
        .unwrap();
    ledger.advance(1099, &p, |_| capacity(0));
    assert!(ledger.get("expired").is_some());
    ledger.advance(1100, &p, |_| capacity(2000));
    assert!(ledger.get("expired").is_none());

    ledger
        .enqueue_waiting("ready".into(), "tool", &p, 2000, request(Some(100)))
        .unwrap();
    ledger.advance(2099, &p, |_| capacity(2000));
    assert_eq!(ledger.get("ready").unwrap().deadline_ms, 32_099);
    ledger.advance(2100, &p, |_| capacity(2000));
    assert_eq!(ledger.get("ready").unwrap().phase, Phase::Reserved);
}

#[test]
fn full_capacity_is_typed_backpressure_without_mutating_accepted_work() {
    let p = policy();
    let mut ledger = Ledger::new("boot".into());
    for id in ["first", "second"] {
        ledger
            .enqueue_waiting(id.into(), "tool", &p, 1000, request(None))
            .unwrap();
    }
    let before = serde_json::to_value(&ledger).unwrap();
    let error = ledger
        .enqueue_waiting("third".into(), "tool", &p, 1000, request(None))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Backpressure>().unwrap().retry_after_ms,
        250
    );
    assert_eq!(serde_json::to_value(&ledger).unwrap(), before);
    assert!(ledger.cancel_pending("first"));
    ledger
        .enqueue_waiting("third".into(), "tool", &p, 1000, request(None))
        .unwrap();
    ledger.validate().unwrap();
}

#[test]
fn impossible_requests_are_not_reported_as_transient_queue_pressure() {
    let mut p = policy();
    p.queue_limit = 1;
    let mut ledger = Ledger::new("boot".into());
    ledger
        .enqueue_waiting("accepted".into(), "tool", &p, 1000, request(None))
        .unwrap();
    for invalid in [
        QueueRequest {
            memory_max: 601,
            ..request(None)
        },
        QueueRequest {
            runtime_max_sec: 0,
            ..request(None)
        },
        request(Some(0)),
    ] {
        let error = ledger
            .enqueue_waiting("invalid".into(), "tool", &p, 1000, invalid)
            .unwrap_err();
        assert!(error.downcast_ref::<Backpressure>().is_none());
    }
    assert_eq!(ledger.entries.len(), 1);
}

#[test]
fn untimed_queued_metadata_round_trips_without_granting_a_reservation() {
    let p = policy();
    let mut ledger = Ledger::new("boot".into());
    ledger
        .enqueue_waiting("accepted".into(), "tool", &p, 1000, request(None))
        .unwrap();
    let mut restored: Ledger =
        serde_json::from_value(serde_json::to_value(&ledger).unwrap()).unwrap();
    restored.validate().unwrap();
    restored.advance(9_000_000, &p, |_| None);
    assert_eq!(restored.get("accepted").unwrap().phase, Phase::Queued);
    assert_eq!(restored.committed(), 0);
    assert!(restored.cancel_pending("accepted"));
    assert!(restored.entries.is_empty());
}
