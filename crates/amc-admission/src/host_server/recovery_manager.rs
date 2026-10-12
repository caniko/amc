//! Manager replacement evidence never releases an unrelated recovery lease.
use crate::{host_native, recovery::RecoveryLease};
use anyhow::Result;
use std::{
    thread,
    time::{Duration, Instant},
};

struct Query {
    lease: RecoveryLease,
    worker: thread::JoinHandle<(Option<bool>, Instant)>,
}

struct Evidence {
    lease: RecoveryLease,
    result: Option<bool>,
    completed: Instant,
}

fn same_invocation(a: &RecoveryLease, b: &RecoveryLease) -> bool {
    a.identity == b.identity && a.invocation_id == b.invocation_id
}

fn needs_query(lease: &RecoveryLease) -> bool {
    lease.invocation_id.is_some()
        && host_native::owner_alive(&crate::ledger::ClientIdentity {
            pid: lease.identity.pid,
            start_ticks: lease.identity.start_ticks,
        }) == Some(false)
}

#[derive(Default)]
pub(super) struct Cache {
    pending: Option<Query>,
    evidence: Option<Evidence>,
}

impl Cache {
    pub fn refresh(&mut self, lease: Option<&RecoveryLease>) -> Result<()> {
        self.refresh_with(lease, host_native::recovery_replaced)
    }

    fn refresh_with(
        &mut self,
        lease: Option<&RecoveryLease>,
        query: impl FnOnce(&RecoveryLease) -> Option<bool> + Send + 'static,
    ) -> Result<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.worker.is_finished())
        {
            let pending = self.pending.take().expect("finished replacement query");
            let (result, completed) = pending.worker.join().unwrap_or((None, Instant::now()));
            self.evidence = Some(Evidence {
                lease: pending.lease,
                result,
                completed,
            });
        }
        let Some(lease) = lease.filter(|lease| needs_query(lease)) else {
            return Ok(());
        };
        if self.pending.is_some()
            || self.evidence.as_ref().is_some_and(|e| {
                same_invocation(&e.lease, lease)
                    && e.completed.elapsed() < Duration::from_millis(250)
            })
        {
            return Ok(());
        }
        let observed = lease.clone();
        let worker = thread::Builder::new()
            .name("amc-recovery-manager".into())
            .spawn(move || {
                let result = query(&observed);
                (result, Instant::now())
            })?;
        self.pending = Some(Query {
            lease: lease.clone(),
            worker,
        });
        Ok(())
    }

    pub fn replaced(&self, lease: &RecoveryLease) -> Option<bool> {
        self.evidence
            .as_ref()
            .filter(|e| {
                same_invocation(&e.lease, lease)
                    && e.completed.elapsed() <= Duration::from_secs(1)
                    && needs_query(lease)
            })
            .and_then(|e| e.result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    fn lease(invocation: &str) -> RecoveryLease {
        serde_json::from_value(serde_json::json!({
            "identity":{"cgroup":"/system.slice/recovery.service","inode":1,"uid":0,"pid":2147483647,"start_ticks":1},
            "invocation_id":invocation,
            "action":{"kind":"device","target":{"name":"swap","path":"/swap","priority":1},"before_used_bytes":4096},
            "return_bytes":4096,"helper_bytes":1024
        })).unwrap()
    }

    #[test]
    fn a_slow_inconclusive_replacement_query_does_not_occupy_the_broker_loop() {
        let lease = lease("11111111111111111111111111111111");
        let mut cache = Cache::default();
        let began = Instant::now();
        cache
            .refresh_with(Some(&lease), |_| {
                thread::sleep(Duration::from_millis(1100));
                None
            })
            .unwrap();
        assert!(
            began.elapsed() < Duration::from_millis(100),
            "a manager query occupied the broker loop"
        );
        assert_ne!(
            cache.replaced(&lease),
            Some(true),
            "inconclusive evidence must retain capacity"
        );
    }

    #[test]
    fn a_late_replacement_observation_cannot_release_a_new_invocation() {
        let old = lease("11111111111111111111111111111111");
        let new = lease("22222222222222222222222222222222");
        let mut cache = Cache::default();
        cache.refresh_with(Some(&old), |_| Some(true)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !cache.pending.as_ref().unwrap().worker.is_finished() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        cache.refresh_with(Some(&new), |_| None).unwrap();
        assert_eq!(cache.replaced(&old), Some(true));
        assert_ne!(cache.replaced(&new), Some(true));
        let mut missing = old.clone();
        missing.invocation_id = None;
        assert_ne!(cache.replaced(&missing), Some(true));
    }
}
