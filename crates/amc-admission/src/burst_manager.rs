//! Bounded manager observations run outside the single broker accept loop.
use crate::host::{HostLedger, Identity};
use anyhow::{Result, ensure};
use std::{
    thread,
    time::{Duration, Instant},
};

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
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| &entry.identity == identity)
        {
            entry.requested = Instant::now();
            return Ok(());
        }
        ensure!(
            self.entries.len() < 256,
            "burst manager observation queue is full"
        );
        self.entries.push(Observation {
            identity: identity.clone(),
            runtime: None,
            completed: None,
            requested: Instant::now(),
            live: false,
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
            self.register(&r.identity)?;
            if let Some(entry) = self.entries.iter_mut().find(|e| e.identity == r.identity) {
                entry.live = true;
            }
        }
        self.poll_with(|identity| crate::host_native::burst_runtime(&identity).ok())
    }

    fn settle(&mut self, identity: &Identity, runtime: Option<u64>, completed: Instant) {
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
            .take(8 - self.pending.len())
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
