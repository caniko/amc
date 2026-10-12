//! Preparation authority is distinct from the aggregate helper's resource caps.
use crate::{
    host::{HostLedger, HostPolicy},
    host_native,
    ledger::ClientIdentity,
};
use anyhow::{Result, ensure};
use std::{os::unix::net::UnixStream, path::Path};

#[derive(Clone, Debug)]
pub(crate) struct Authorization {
    uid: u32,
    helper: ClientIdentity,
    helper_cgroup_inode: u64,
    origin: ClientIdentity,
    profile: String,
    key: String,
    expires_ms: u64,
}

pub(super) fn authorize(
    ledger: &mut HostLedger,
    id: &str,
    profile: &str,
    peer: (u32, ClientIdentity),
    origin: ClientIdentity,
    now: u64,
    policy: &HostPolicy,
) -> Result<()> {
    let (uid, helper) = peer;
    ensure!(
        crate::ledger::valid_name(id)
            && helper.pid != origin.pid
            && policy.preparations.iter().any(|p| p.name == profile
                && policy
                    .domains
                    .iter()
                    .any(|d| d.name == p.domain && d.uid == uid)),
        "invalid preparation rendezvous selection"
    );
    host_native::prepared_origin(&origin, uid)?;
    let helper_cgroup_inode = helper_boundary(id, uid, &helper)?;
    ensure!(
        host_native::preparation_owner_alive(&origin) == Some(true)
            && host_native::preparation_owner_alive(&helper) == Some(true),
        "preparation rendezvous or helper exited"
    );
    ledger.preparation_authorizations.retain(|_, a| {
        now < a.expires_ms
            && host_native::preparation_owner_alive(&a.helper) == Some(true)
            && host_native::preparation_owner_alive(&a.origin) == Some(true)
    });
    let authorization = if let Some(previous) = ledger.preparation_authorizations.get(id) {
        ensure!(
            previous.uid == uid
                && previous.helper == helper
                && previous.helper_cgroup_inode == helper_cgroup_inode
                && previous.origin == origin
                && previous.profile == profile,
            "preparation rendezvous registration changed"
        );
        previous.clone()
    } else {
        ensure!(
            ledger.preparation_authorizations.len() < policy.queue_limit.min(256)
                && ledger
                    .preparation_authorizations
                    .values()
                    .filter(|a| a.uid == uid)
                    .count()
                    < 8
                && !ledger.preparations.iter().any(|p| p.id == id),
            "preparation rendezvous queue full or ticket exists"
        );
        Authorization {
            uid,
            helper,
            helper_cgroup_inode,
            origin,
            profile: profile.into(),
            key: crate::store::fresh_id()?,
            expires_ms: now.saturating_add(30_000),
        }
    };
    let callback = format!("/run/user/{uid}/amc-{id}/authorize");
    deliver(Path::new(&callback), &authorization)?;
    ledger
        .preparation_authorizations
        .insert(id.into(), authorization);
    Ok(())
}

pub(super) fn helper_boundary(id: &str, uid: u32, helper: &ClientIdentity) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        crate::ledger::valid_name(id),
        "invalid preparation rendezvous ID"
    );
    let expected = format!(
        "{}/app-amc-prepare-helper-{id}.service",
        crate::namespace_runner::cgroup(uid)
    );
    let placement = std::fs::read_to_string(format!("/proc/{}/cgroup", helper.pid))?;
    ensure!(
        placement
            .lines()
            .any(|line| line.strip_prefix("0::") == Some(expected.as_str()))
            && host_native::preparation_owner_alive(helper) == Some(true),
        "preparation helper invocation changed"
    );
    Ok(std::fs::metadata(crate::native::cgroup_directory(&expected)?)?.ino())
}

fn deliver(path: &Path, authorization: &Authorization) -> Result<()> {
    use nix::sys::socket::{
        AddressFamily, SockFlag, SockType, UnixAddr, connect, getsockopt, socket,
        sockopt::PeerCredentials,
    };
    use std::os::fd::AsRawFd;
    // A full caller-owned listen backlog or unread callback cannot occupy the
    // broker loop. No user-controlled response is read on this connection.
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    connect(fd.as_raw_fd(), &UnixAddr::new(path)?)?;
    let mut stream = UnixStream::from(fd);
    let peer = getsockopt(&stream, PeerCredentials)?;
    ensure!(
        peer.uid() == authorization.uid && peer.pid() == authorization.origin.pid,
        "preparation callback is not the nominated rendezvous process"
    );
    host_native::prepared_origin(&authorization.origin, authorization.uid)?;
    crate::protocol::write_frame(&mut stream, &authorization.key)?;
    host_native::prepared_origin(&authorization.origin, authorization.uid)?;
    Ok(())
}

pub(super) fn prepare(
    ledger: &mut HostLedger,
    id: &str,
    key: &str,
    peer: (u32, ClientIdentity),
    profile: &str,
    now: u64,
    policy: &HostPolicy,
) -> Result<()> {
    let (uid, helper) = peer;
    let (origin, helper_cgroup_inode) =
        if let Some(previous) = ledger.preparations.iter().find(|p| p.id == id) {
            ensure!(
                previous.uid == uid
                    && previous.key == key
                    && previous.owner.as_ref() == Some(&helper)
                    && previous.profile == profile,
                "preparation authorization replay changed"
            );
            (
                previous.origin.clone().ok_or_else(|| {
                    anyhow::anyhow!("preparation rendezvous authorization missing")
                })?,
                previous
                    .helper_cgroup_inode
                    .ok_or_else(|| anyhow::anyhow!("preparation helper invocation missing"))?,
            )
        } else {
            let authorization = ledger
                .preparation_authorizations
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("preparation rendezvous authorization missing"))?;
            ensure!(
                authorization.uid == uid
                    && authorization.helper == helper
                    && authorization.profile == profile
                    && authorization.key == key
                    && now < authorization.expires_ms,
                "preparation rendezvous authorization changed or expired"
            );
            (
                authorization.origin.clone(),
                authorization.helper_cgroup_inode,
            )
        };
    host_native::prepared_origin(&origin, uid)?;
    ensure!(
        host_native::preparation_owner_alive(&origin) == Some(true),
        "preparation rendezvous exited"
    );
    ledger.prepare(id.into(), key.into(), (uid, helper), profile, now, policy)?;
    let preparation = ledger
        .preparations
        .iter_mut()
        .find(|p| p.id == id)
        .expect("prepared intent");
    preparation.origin = Some(origin);
    preparation.helper_cgroup_inode = Some(helper_cgroup_inode);
    ledger.preparation_authorizations.remove(id);
    Ok(())
}

pub(super) fn verify_caller(
    ledger: &HostLedger,
    id: &str,
    key: &str,
    uid: u32,
    pid: i32,
) -> Result<()> {
    let preparation = ledger
        .preparations
        .iter()
        .find(|p| p.id == id && p.uid == uid && p.key == key)
        .ok_or_else(|| anyhow::anyhow!("unknown preparation capability"))?;
    ensure!(
        preparation
            .owner
            .as_ref()
            .is_some_and(|owner| owner.pid == pid
                && host_native::process_start(pid).ok() == Some(owner.start_ticks)),
        "preparation request is not from the authorized helper lifetime"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HostPolicy {
        serde_json::from_value(serde_json::json!({
            "version":1,"budget_bytes":1073741824,"reserve_bytes":67108864,"swap_reserve_bytes":0,
            "max_memory_full_psi":10.0,"max_io_full_psi":10.0,"resume_ms":250,"aging_ms":1000,"queue_limit":16,
            "namespace_runner_bytes":67108864,
            "domains":[{"name":"game","uid":1000,"cgroup":"/user.slice/user-1000.slice/user@1000.service/game.slice","ceiling_bytes":134217728,"swap_bytes":0,"fair_share_bytes":134217728}],
            "preparations":[{"name":"game","domain":"game","memory_bytes":134217728,"swap_bytes":0,
                "drain_domains":[],"wait_ms":3600000,"ready_ms":15000}]
        })).unwrap()
    }

    #[test]
    fn a_backed_helper_cannot_create_a_barrier_without_rendezvous_authorization() {
        // Native placement/backing is a separate prerequisite. This is the
        // admission boundary after those checks have succeeded for a helper.
        let mut ledger = HostLedger::new("boot".into());
        let helper = ClientIdentity {
            pid: 42,
            start_ticks: 7,
        };
        let result = prepare(
            &mut ledger,
            "forged-helper",
            "chosen-key",
            (1000, helper),
            "game",
            0,
            &policy(),
        );
        assert!(
            result.is_err(),
            "a caller-selected unit and key must not authorize a host-wide barrier"
        );
        assert!(!ledger.preparation_barrier());
    }

    #[test]
    fn callback_secret_is_bound_to_kernel_origin_and_one_helper_lifetime() {
        use std::{fs, os::unix::net::UnixListener};
        let uid = nix::unistd::geteuid().as_raw();
        let origin = ClientIdentity {
            pid: std::process::id() as i32,
            start_ticks: host_native::process_start(std::process::id() as i32).unwrap(),
        };
        let helper = ClientIdentity {
            pid: 42,
            start_ticks: 7,
        };
        let mut policy = policy();
        policy.domains[0].uid = uid;
        let root =
            std::env::temp_dir().join(format!("amc-auth-{}", crate::store::fresh_id().unwrap()));
        fs::create_dir(&root).unwrap();
        let listener = UnixListener::bind(root.join("authorize")).unwrap();
        let binding = Authorization {
            uid,
            helper: helper.clone(),
            helper_cgroup_inode: 1,
            origin: origin.clone(),
            profile: "game".into(),
            key: crate::store::fresh_id().unwrap(),
            expires_ms: 30_000,
        };
        deliver(&root.join("authorize"), &binding).unwrap();
        let (mut callback, _) = listener.accept().unwrap();
        let secret: String = crate::protocol::read_frame(&mut callback).unwrap();
        assert_eq!(secret, binding.key);

        let mut wrong_origin = binding.clone();
        wrong_origin.origin.start_ticks += 1;
        assert!(deliver(&root.join("authorize"), &wrong_origin).is_err());
        wrong_origin.origin = helper.clone();
        assert!(deliver(&root.join("authorize"), &wrong_origin).is_err());

        let mut ledger = HostLedger::new("boot".into());
        ledger
            .preparation_authorizations
            .insert("intent".into(), binding);
        for (key, owner, profile, now) in [
            ("chosen-key", helper.clone(), "game", 0),
            (
                secret.as_str(),
                ClientIdentity {
                    pid: helper.pid,
                    start_ticks: helper.start_ticks + 1,
                },
                "game",
                0,
            ),
            (secret.as_str(), helper.clone(), "different", 0),
            (secret.as_str(), helper.clone(), "game", 30_000),
        ] {
            assert!(
                prepare(
                    &mut ledger,
                    "intent",
                    key,
                    (uid, owner),
                    profile,
                    now,
                    &policy
                )
                .is_err()
            );
            assert!(!ledger.preparation_barrier());
        }
        let unconsumed: HostLedger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        assert!(
            unconsumed.preparation_authorizations.is_empty(),
            "restart must invalidate unconsumed secrets"
        );
        prepare(
            &mut ledger,
            "intent",
            &secret,
            (uid, helper.clone()),
            "game",
            0,
            &policy,
        )
        .unwrap();
        assert!(
            ledger.preparation_authorizations.is_empty(),
            "authorization must be consumed exactly once"
        );
        assert_eq!(ledger.preparations[0].origin, Some(origin));
        assert!(ledger.preparation_barrier());
        let mut restored: HostLedger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        prepare(
            &mut restored,
            "intent",
            &secret,
            (uid, helper.clone()),
            "game",
            1,
            &policy,
        )
        .unwrap();
        assert!(
            prepare(
                &mut restored,
                "new-ticket",
                &secret,
                (uid, helper),
                "game",
                1,
                &policy
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
