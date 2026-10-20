use amc_admission::ledger::{Headroom, Identity, Ledger, Phase, Policy, QueueRequest};

fn policy() -> Policy {
    serde_json::from_value(serde_json::json!({
        "version": 1, "budget_bytes": 1000, "reserve_bytes": 100, "queue_limit": 2,
        "contracts": {"work": {"slice": "work.slice", "memory_max": 600,
            "memory_swap_max": 0, "max_running": 1, "pause_file": null}}
    }))
    .unwrap()
}

fn identity() -> Identity {
    Identity {
        invocation: "abc".into(),
        cgroup: "/work/job".into(),
        inode: 1,
    }
}

fn reserved(wait_ms: Option<u64>) -> (Policy, Ledger) {
    let policy = policy();
    let mut ledger = Ledger::new("boot".into());
    ledger
        .enqueue_waiting(
            "job".into(),
            "work",
            &policy,
            0,
            QueueRequest {
                memory_max: 600,
                runtime_max_sec: 1,
                wait_ms,
            },
        )
        .unwrap();
    ledger.advance(0, &policy, |_| {
        Some(Headroom {
            shared_bytes: 2000,
            slice_bytes: 2000,
            memory_max: 2000,
            memory_swap_max: 0,
            paused: false,
        })
    });
    assert_eq!(ledger.get("job").unwrap().phase, Phase::Reserved);
    (policy, ledger)
}

#[test]
fn host_wait_retains_native_accounting_beyond_entry_grace_and_runtime() {
    let (policy, mut ledger) = reserved(None);
    assert!(
        ledger.enter("job", identity()).is_err(),
        "queued payload cannot skip native startup"
    );
    ledger.start("job", identity(), 10).unwrap();
    ledger.advance(180_000, &policy, |_| None);
    assert_eq!(ledger.get("job").unwrap().phase, Phase::Starting);
    assert_eq!(ledger.committed(), 600);
    assert!(
        !ledger.cancel_pending("job"),
        "cancellation cannot release a live native helper"
    );
    let mut different = identity();
    different.inode += 1;
    assert!(ledger.enter("job", different).is_err());
    ledger.enter("job", identity()).unwrap();
    ledger.validate().unwrap();
    assert_eq!(ledger.get("job").unwrap().phase, Phase::Running);
}

#[test]
fn expired_or_restarted_native_wait_needs_positive_cleanup_before_release() {
    let (policy, mut ledger) = reserved(Some(100));
    ledger.start("job", identity(), 10).unwrap();
    assert_eq!(ledger.get("job").unwrap().deadline_ms, 100);
    let mut restored: Ledger =
        serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
    restored.validate().unwrap();
    restored.advance(180_000, &policy, |_| None);
    restored.reconcile(|_| false);
    assert_eq!(restored.committed(), 600);
    restored.reconcile(|_| true);
    assert_eq!(restored.committed(), 0);
    let (_, mut late) = reserved(None);
    assert!(late.start("job", identity(), 30_001).is_err());
    let (_, mut late) = reserved(Some(100));
    assert!(late.start("job", identity(), 101).is_err());
}
