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
    pending: Option<Pending>,
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
        for r in ledger.reservations.iter().filter(|r| r.burst) {
            self.register(&r.identity)?;
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
        observe: impl FnOnce(Identity) -> Option<u64> + Send + 'static,
    ) -> Result<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|task| task.worker.is_finished())
        {
            let task = self.pending.take().expect("finished manager task");
            let (runtime, completed) = task.worker.join().unwrap_or((None, Instant::now()));
            self.settle(&task.identity, runtime, completed);
        }
        if self.pending.is_some() {
            return Ok(());
        }
        // Unknown entries first, then oldest observations. A slow manager never
        // holds the accept loop or starts one subprocess per concurrent peer.
        if let Some(entry) = self
            .entries
            .iter()
            .filter(|entry| {
                entry
                    .completed
                    .is_none_or(|time| time.elapsed() >= Duration::from_millis(250))
            })
            .min_by_key(|entry| entry.completed)
        {
            let identity = entry.identity.clone();
            let target = identity.clone();
            let worker = thread::Builder::new()
                .name("amc-burst-manager".into())
                .spawn(move || (observe(target), Instant::now()))?;
            self.pending = Some(Pending { identity, worker });
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
        cache
            .poll_with(move |_| {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
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
        let task = cache.pending.take().unwrap();
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
}
