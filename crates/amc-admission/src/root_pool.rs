//! Root handlers retain capacity for one finite worker, independently of their
//! long-lived socket/PID. A release settles ownership, never native descendants.
use crate::host::{HostLedger, Identity};
use anyhow::{Result, ensure};

fn owner_key(domain: &str, identity: &Identity) -> String {
    format!(
        "pool-{domain}-owner-{}-{}",
        identity.pid, identity.start_ticks
    )
}

impl HostLedger {
    pub fn owns_pool_operation(
        &self,
        domain: &str,
        identity: &Identity,
        operation: Option<u64>,
    ) -> bool {
        self.reservations.iter().any(|r| {
            r.granted
                && r.domain == domain
                && r.identity.uid == 0
                && r.identity.cgroup == identity.cgroup
                && r.identity.inode == identity.inode
                && r.owners
                    .iter()
                    .any(|o| o.pid == identity.pid && o.start_ticks == identity.start_ticks)
        }) && operation.is_none_or(|operation| {
            self.pool_operations
                .get(&format!("{}-{operation}", owner_key(domain, identity)))
                == Some(&operation)
        })
    }

    pub fn bind_pool_operation(
        &mut self,
        domain: &str,
        identity: &Identity,
        operation: u64,
    ) -> Result<()> {
        let key = format!("{}-{operation}", owner_key(domain, identity));
        ensure!(
            identity.uid == 0 && operation > 0 && crate::ledger::valid_name(&key),
            "invalid root worker operation"
        );
        ensure!(
            self.pool_operations.contains_key(&key) || self.pool_operations.len() < 256,
            "root operation bound exceeded"
        );
        self.pool_operations.insert(key, operation);
        Ok(())
    }

    pub fn release_pool_operation(&mut self, domain: &str, identity: &Identity, operation: u64) {
        let owner = owner_key(domain, identity);
        let key = format!("{owner}-{operation}");
        if self.pool_operations.get(&key) != Some(&operation) {
            return;
        }
        self.pool_operations.remove(&key);
        // A nested Worker on the same root handler has its own serial. Ending
        // that child cannot relinquish its still-running outer operation.
        if self
            .pool_operations
            .keys()
            .any(|key| key.starts_with(&format!("{owner}-")))
        {
            return;
        }
        for r in self.reservations.iter_mut().filter(|r| {
            r.domain == domain
                && r.identity.uid == 0
                && r.identity.cgroup == identity.cgroup
                && r.identity.inode == identity.inode
        }) {
            r.owners
                .retain(|o| o.pid != identity.pid || o.start_ticks != identity.start_ticks);
            if r.owners.is_empty() {
                r.owners_finished = true;
            }
        }
    }

    pub(crate) fn retain_pool_operations(&mut self) {
        self.pool_operations.retain(|saved, _| {
            self.reservations.iter().any(|r| {
                r.identity.uid == 0
                    && r.owners.iter().any(|o| {
                        saved.starts_with(&format!(
                            "pool-{}-owner-{}-{}-",
                            r.domain, o.pid, o.start_ticks
                        ))
                    })
            })
        });
    }
}
