use amc_admission::ledger::{Contract, Headroom, Identity, Ledger, Phase, Policy, WaitReason};
use std::collections::BTreeMap;

fn policy() -> Policy {
    Policy {
        version: 1,
        budget_bytes: 1000,
        reserve_bytes: 100,
        queue_limit: 16,
        contracts: BTreeMap::from([(
            "tool".into(),
            Contract {
                slice: "agent-tools.slice".into(),
                memory_max: 600,
                memory_swap_max: 0,
                max_running: 2,
                pause_file: None,
            },
        )]),
    }
}

fn identity() -> Identity {
    Identity {
        invocation: "abc".into(),
        cgroup: "/user.slice/job".into(),
        inode: 123,
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
fn independent_requests_share_one_budget() {
    let p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue("a".into(), "tool", &p, 1000).unwrap();
    l.enqueue("b".into(), "tool", &p, 1000).unwrap();
    l.advance(0, &p, |_| capacity(2000));
    assert_eq!(l.get("a").unwrap().phase, Phase::Reserved);
    assert_eq!(l.get("b").unwrap().phase, Phase::Queued);
    assert_eq!(l.committed(), 600);
}

#[test]
fn restart_and_client_timeout_do_not_release_running_work() {
    let p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue("a".into(), "tool", &p, 1000).unwrap();
    l.advance(0, &p, |_| capacity(2000));
    l.enter("a", identity()).unwrap();
    let mut restored: Ledger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
    restored.advance(9999, &p, |_| capacity(2000));
    assert_eq!(restored.committed(), 600);
    assert!(!restored.cancel_pending("a"));
    assert_eq!(restored.get("a").unwrap().identity, Some(identity()));
    restored.reconcile(|_| false);
    assert_eq!(restored.committed(), 600);
    restored.reconcile(|_| true);
    assert_eq!(restored.committed(), 0);
}

#[test]
fn expired_reservation_cannot_execute_late_or_replay() {
    let p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue("a".into(), "tool", &p, 10).unwrap();
    l.advance(0, &p, |_| capacity(2000));
    l.advance(11, &p, |_| capacity(2000));
    assert!(l.enter("a", identity()).is_err());
    l.enqueue("b".into(), "tool", &p, 100).unwrap();
    l.advance(11, &p, |_| capacity(2000));
    l.enter("b", identity()).unwrap();
    assert!(l.enter("b", identity()).is_err());
}

#[test]
fn unknown_capacity_and_lowered_policy_refuse_new_work() {
    let mut p = policy();
    let mut l = Ledger::new("boot".into());
    l.enqueue("a".into(), "tool", &p, 1000).unwrap();
    l.advance(0, &p, |_| None);
    assert_eq!(l.committed(), 0);
    l.advance(1, &p, |_| capacity(599));
    assert_eq!(l.committed(), 0);
    l.advance(2, &p, |_| capacity(2000));
    l.enter("a", identity()).unwrap();
    p.budget_bytes = 500;
    l.advance(3, &p, |_| capacity(2000));
    assert_eq!(l.committed(), 600);
}

#[test]
fn queue_is_bounded_and_uses_fifo() {
    let mut p = policy();
    p.queue_limit = 2;
    let mut l = Ledger::new("boot".into());
    l.enqueue("z".into(), "tool", &p, 1000).unwrap();
    l.enqueue("a".into(), "tool", &p, 1000).unwrap();
    assert!(l.enqueue("c".into(), "tool", &p, 1000).is_err());
    l.advance(0, &p, |_| capacity(2000));
    assert_eq!(l.get("z").unwrap().phase, Phase::Reserved);
    assert_eq!(l.get("a").unwrap().phase, Phase::Queued);
}

#[test]
fn long_lived_pool_does_not_block_disposable_pool() {
    let mut p = policy();
    p.contracts.insert(
        "server".into(),
        Contract {
            slice: "agent-server.slice".into(),
            memory_max: 400,
            memory_swap_max: 0,
            max_running: 1,
            pause_file: None,
        },
    );
    let mut ledger = Ledger::new("boot".into());
    ledger
        .enqueue("server1".into(), "server", &p, 1000)
        .unwrap();
    ledger.advance(0, &p, |_| capacity(2000));
    ledger.enter("server1", identity()).unwrap();
    ledger
        .enqueue("server2".into(), "server", &p, 1000)
        .unwrap();
    ledger.enqueue("tool".into(), "tool", &p, 1000).unwrap();
    let waiting = ledger.advance(0, &p, |_| {
        Some(Headroom {
            shared_bytes: 2000,
            slice_bytes: 600,
            memory_max: 600,
            memory_swap_max: 0,
            paused: false,
        })
    });
    assert_eq!(waiting["server2"].reason, WaitReason::Concurrency);
    assert_eq!(ledger.get("tool").unwrap().phase, Phase::Reserved);
    assert_eq!(ledger.committed(), 1000);
}
