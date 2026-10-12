//! Bounded manager observations run outside the single broker accept loop.
use crate::host::{HostLedger, Identity};
use anyhow::{Result, ensure};
use std::{
    thread,
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 256;
const MAX_LIVE: usize = 8;
// One UID can probe the policy's maximum eight simultaneous bursts without
// accumulating enough failed/non-live identities to exhaust the shared cache.
const MAX_PROBES_PER_UID: usize = MAX_LIVE;

#[derive(Clone)]
pub(crate) struct Observation {
    identity: Identity,
    runtime: Option<u64>,
    completed: Option<Instant>,
    requested: Instant,
    live: bool,
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot(Vec<Observation>);

impl Snapshot {
    pub(crate) fn runtime(&self, identity: &Identity) -> Option<u64> {
        self.0
            .iter()
            .find(|entry| &entry.identity == identity)
            .filter(|entry| {
                entry
                    .completed
                    .is_some_and(|time| time.elapsed() <= Duration::from_secs(1))
            })
            .and_then(|entry| entry.runtime)
    }
}

struct Pending {
    identity: Identity,
    worker: thread::JoinHandle<(Option<u64>, Instant)>,
}

#[derive(Default)]
pub(crate) struct Cache {
    entries: Vec<Observation>,
    pending: Vec<Pending>,
}

impl Cache {
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot(self.entries.clone())
    }

    pub(crate) fn runtime(&self, identity: &Identity) -> Option<u64> {
        self.snapshot().runtime(identity)
    }

    pub(crate) fn register(&mut self, identity: &Identity) -> Result<()> {
        self.register_with(identity, false)
    }

    fn register_with(&mut self, identity: &Identity, live: bool) -> Result<()> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.identity == identity)
        {
            entry.requested = Instant::now();
            entry.live |= live;
            return Ok(());
        }
        ensure!(
            live || self
                .entries
                .iter()
                .filter(|entry| !entry.live && entry.identity.uid == identity.uid)
                .count()
                < MAX_PROBES_PER_UID,
            "burst manager per-user observation queue is full"
        );
        ensure!(
            self.entries.len()
                < if live {
                    MAX_ENTRIES
                } else {
                    MAX_ENTRIES - MAX_LIVE
                },
            "burst manager observation queue is full"
        );
        self.entries.push(Observation {
            identity: identity.clone(),
            runtime: None,
            completed: None,
            requested: Instant::now(),
            live,
        });
        Ok(())
    }

    pub(crate) fn refresh(&mut self, ledger: &HostLedger) -> Result<()> {
        self.entries.retain(|entry| {
            entry.requested.elapsed() < Duration::from_secs(60)
                || ledger
                    .reservations
                    .iter()
                    .any(|r| r.burst && r.identity == entry.identity)
        });
        for entry in &mut self.entries {
            entry.live = false;
        }
        for r in ledger.reservations.iter().filter(|r| r.burst && r.granted) {
            self.register_with(&r.identity, true)?;
        }
        self.poll_with(|identity| crate::host_native::burst_runtime(&identity).ok())
    }

    fn settle(&mut self, identity: &Identity, runtime: Option<u64>, completed: Instant) {
        // Failed queue-only probes confer no admission evidence. Drop them
        // promptly; never evict a live grant whose accounting still exists.
        self.entries
            .retain(|entry| &entry.identity != identity || entry.live || runtime.is_some());
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.identity == identity)
        {
            entry.runtime = runtime;
            entry.completed = Some(completed);
        }
    }

    fn poll_with(
        &mut self,
        observe: impl Fn(Identity) -> Option<u64> + Send + Sync + 'static,
    ) -> Result<()> {
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].worker.is_finished() {
                let task = self.pending.swap_remove(index);
                let (runtime, completed) = task.worker.join().unwrap_or((None, Instant::now()));
                self.settle(&task.identity, runtime, completed);
            } else {
                index += 1;
            }
        }
        // Eight is the policy's maximum live burst count. Refresh those peers
        // concurrently instead of serializing a >1s cycle behind stale evidence.
        // Queue-only registrations use spare slots and cannot outrank live work.
        let mut candidates: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                entry
                    .completed
                    .is_none_or(|time| time.elapsed() >= Duration::from_millis(250))
            })
            .filter(|entry| {
                !self
                    .pending
                    .iter()
                    .any(|task| task.identity == entry.identity)
            })
            .collect();
        candidates.sort_by_key(|entry| (!entry.live, entry.completed));
        let identities: Vec<_> = candidates
            .into_iter()
            .take(MAX_LIVE - self.pending.len())
            .map(|entry| entry.identity.clone())
            .collect();
        let observe = std::sync::Arc::new(observe);
        for identity in identities {
            let target = identity.clone();
            let observe = observe.clone();
            let worker = thread::Builder::new()
                .name("amc-burst-manager".into())
                .spawn(move || (observe(target), Instant::now()))?;
            self.pending.push(Pending { identity, worker });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn one_users_failed_probes_cannot_exhaust_another_users_registration() {
        let mut cache = Cache::default();
        let mut identity = Identity {
            cgroup: "/burst.service".into(),
            inode: 1,
            uid: 1000,
            pid: 1,
            start_ticks: 7,
        };
        for pid in 1..=256 {
            identity.pid = pid;
            let _ = cache.register(&identity);
        }
        identity.uid = 1001;
        identity.pid = 1000;
        cache
            .register(&identity)
            .expect("one UID must not fill the shared observation cache");
        assert!(cache.entries.len() <= MAX_PROBES_PER_UID + 1);
    }

    #[test]
    fn failed_probes_release_slots_but_live_grants_survive_failed_queries() {
        let mut cache = Cache::default();
        let mut identity = Identity {
            cgroup: "/burst.service".into(),
            inode: 1,
            uid: 1000,
            pid: 1,
            start_ticks: 7,
        };
        cache.register(&identity).unwrap();
        cache.settle(&identity, None, Instant::now());
        assert!(cache.entries.is_empty());
        cache.register_with(&identity, true).unwrap();
        cache.settle(&identity, None, Instant::now());
        assert!(cache.entries[0].live);
        for pid in 2..=9 {
            identity.pid = pid;
            cache.register(&identity).unwrap();
        }
        identity.pid = 10;
        assert!(cache.register(&identity).is_err());
        cache.register_with(&identity, true).unwrap();
        assert_eq!(cache.entries.iter().filter(|entry| entry.live).count(), 2);
    }

    #[test]
    fn queued_burst_grants_require_exact_fresh_evidence_after_native_capacity_validation() {
        let policy: crate::host::HostPolicy = serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":80,"reserve_bytes":20,"swap_reserve_bytes":0,
            "max_memory_full_psi":1.0,"max_io_full_psi":20.0,"resume_ms":250,"aging_ms":1000,"queue_limit":32,
            "burst":{"budget_bytes":20,"max_job_bytes":10,"max_running":1,"max_runtime_ms":5000,"min_interval_ms":0},
            "domains":[{"name":"burst","uid":1000,"cgroup":"/burst","ceiling_bytes":10,"swap_bytes":0,"fair_share_bytes":80,"burst":true}]
        })).unwrap();
        let request: crate::host::Reservation = serde_json::from_value(serde_json::json!({
            "id":"queued","domain":"burst","identity":{"cgroup":"/burst/queued","inode":1,"uid":1000,"pid":42,"start_ticks":7},
            "memory_bytes":10,"swap_bytes":0,"requested_ms":0,"deadline_ms":30000,"granted":false,"owners":[],"burst":true,"runtime_max_ms":5000
        })).unwrap();
        let mut ledger = HostLedger::new("boot".into());
        ledger.request(request.clone(), &policy).unwrap();
        for (runtime, age, replaced) in [
            (None, 0, false),
            (Some(4000), 0, false),
            (Some(5000), 2, false),
            (Some(5000), 0, true),
            (Some(5000), 0, false),
        ] {
            let mut identity = request.identity.clone();
            if replaced {
                identity.start_ticks += 1;
            }
            let snapshot = Snapshot(vec![Observation {
                identity,
                runtime,
                completed: Some(Instant::now() - Duration::from_secs(age)),
                requested: Instant::now(),
                live: false,
            }]);
            let checked = std::cell::Cell::new(false);
            ledger.advance_with_burst_runtime(
                250,
                &policy,
                Some(crate::host::Capacity {
                    available_bytes: 100,
                    swap_free_bytes: 100,
                    memory_full_psi: 0.0,
                    io_full_psi: 0.0,
                }),
                &mut std::collections::BTreeMap::from([("burst".into(), 0)]),
                |_, _| {
                    checked.set(true);
                    Some(100)
                },
                |identity| {
                    assert!(
                        checked.get(),
                        "manager freshness is checked after native headroom validation"
                    );
                    snapshot.runtime(identity)
                },
            );
            assert_eq!(
                ledger.reservations[0].granted,
                runtime == Some(5000) && age == 0 && !replaced
            );
        }
    }

    #[test]
    fn slow_manager_queries_leave_broker_polling_nonblocking_and_replaced_peers_unproven() {
        let identity = Identity {
            cgroup: "/burst.service".into(),
            inode: 1,
            uid: 1000,
            pid: 42,
            start_ticks: 7,
        };
        let mut cache = Cache::default();
        cache.register(&identity).unwrap();
        let (entered, started) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = std::sync::Mutex::new(blocked);
        cache
            .poll_with(move |_| {
                entered.send(()).unwrap();
                blocked.lock().unwrap().recv().unwrap();
                Some(5000)
            })
            .unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        cache
            .poll_with(|_| panic!("a second manager query must not run concurrently"))
            .unwrap();
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(cache.runtime(&identity), None);
        release.send(()).unwrap();
        let task = cache.pending.pop().unwrap();
        let (runtime, completed) = task.worker.join().unwrap();
        cache.settle(&task.identity, runtime, completed);
        assert_eq!(cache.runtime(&identity), Some(5000));
        let replaced = Identity {
            start_ticks: 8,
            ..identity.clone()
        };
        assert_eq!(cache.runtime(&replaced), None);
        cache.entries[0].completed = Some(Instant::now() - Duration::from_secs(2));
        assert_eq!(cache.runtime(&identity), None);
        cache.settle(&identity, None, Instant::now());
        assert_eq!(cache.runtime(&identity), None);
    }

    #[test]
    fn eight_slow_live_bursts_refresh_concurrently_without_expanding_freshness() {
        let mut cache = Cache::default();
        for pid in 1..=8 {
            let identity = Identity {
                cgroup: format!("/burst-{pid}.service"),
                inode: pid as u64,
                uid: 1000,
                pid,
                start_ticks: 7,
            };
            cache.register(&identity).unwrap();
            cache.entries.last_mut().unwrap().live = true;
        }
        let (entered, started) = mpsc::channel();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let gate = barrier.clone();
        cache
            .poll_with(move |identity| {
                entered.send(identity.pid).unwrap();
                gate.wait();
                Some(5000)
            })
            .unwrap();
        let mut pids = std::collections::BTreeSet::new();
        for _ in 0..8 {
            pids.insert(started.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        assert_eq!(pids.len(), 8);
        assert_eq!(cache.pending.len(), 8);
        cache
            .poll_with(|_| panic!("pending queries cannot spawn duplicate work"))
            .unwrap();
        barrier.wait();
        for task in std::mem::take(&mut cache.pending) {
            let (runtime, completed) = task.worker.join().unwrap();
            cache.settle(&task.identity, runtime, completed);
        }
        let snapshot = cache.snapshot();
        assert!(
            cache
                .entries
                .iter()
                .all(|entry| snapshot.runtime(&entry.identity) == Some(5000))
        );
        for entry in &mut cache.entries {
            entry.completed = Some(Instant::now() - Duration::from_secs(2));
        }
        let expired = cache.snapshot();
        assert!(
            cache
                .entries
                .iter()
                .all(|entry| expired.runtime(&entry.identity).is_none())
        );
    }
}
