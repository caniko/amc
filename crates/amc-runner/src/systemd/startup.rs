//! One-attempt startup synchronization, not admission or termination evidence.
//!
//! The exec helper waits before the payload can exit. The runner releases it
//! only after pinning the native invocation and execution domain. Abstract Unix
//! sockets leave no pathname behind after crashes; both peers authenticate PIDs
//! and UIDs, and the runner also verifies the helper's native cgroup placement.
use anyhow::{Result, ensure};
use nix::{
    sys::socket::{getsockopt, sockopt::PeerCredentials},
    unistd::geteuid,
};
use std::{
    io::{ErrorKind, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// A one-use rendezvous held by the runner until native startup is pinned.
#[derive(Debug)]
pub struct StartupBarrier {
    name: String,
    listener: UnixListener,
    host_path: Option<(PathBuf, u64, u64)>,
}

impl StartupBarrier {
    /// Bind a fresh, one-attempt name before creating the submission client.
    pub fn new(name: String) -> Result<Self> {
        let listener = UnixListener::bind_addr(&address(&name)?)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            name,
            listener,
            host_path: None,
        })
    }

    /// Hold a host-native *runner* before it starts the inner managed launch.
    /// Unlike the payload barrier, the host runner validates the manager's
    /// PID/cgroup challenge in its own host view. The inner payload retains its
    /// independent native identity and admission checks.
    pub fn host_runner(path: PathBuf) -> Result<Self> {
        let parent = std::fs::symlink_metadata(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("missing host rendezvous parent"))?,
        )?;
        ensure!(
            parent.is_dir() && parent.uid() == geteuid().as_raw() && parent.mode() & 0o077 == 0,
            "untrusted host runner rendezvous directory"
        );
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            name: String::new(),
            listener,
            host_path: Some((path, metadata.dev(), metadata.ino())),
        })
    }

    /// Literal name passed to the exec helper; peer credentials authorize use.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Called only with a pinned, live native identity. A pending connection is
    /// not readiness. An unexpected peer fails closed instead of starting work.
    pub(super) fn release(&self, main_pid: u32, group: &str) -> Result<bool> {
        let (mut stream, _) = match self.listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let peer = getsockopt(&stream, PeerCredentials)?;
        if self.host_path.is_some() {
            ensure!(
                main_pid > 0 && peer.uid() == geteuid().as_raw(),
                "host runner UID mismatch"
            );
            ensure!(
                group.starts_with('/') && group.len() <= 4096 && !group.contains('\n'),
                "invalid native runner group"
            );
            stream.set_write_timeout(Some(Duration::from_millis(100)))?;
            stream.set_read_timeout(Some(Duration::from_millis(100)))?;
            stream.write_all(format!("{main_pid}\n{group}\n").as_bytes())?;
            let mut response = [0];
            stream.read_exact(&mut response)?;
            ensure!(
                response == [1],
                "host runner rejected native startup identity"
            );
            return Ok(true);
        }
        ensure!(
            main_pid > 0 && peer.pid() as u32 == main_pid && peer.uid() == geteuid().as_raw(),
            "startup helper does not match native MainPID/UID"
        );
        let placement = std::fs::read_to_string(format!("/proc/{main_pid}/cgroup"))?;
        ensure!(
            placement
                .lines()
                .any(|line| line.strip_prefix("0::") == Some(group)),
            "startup helper does not match native cgroup"
        );
        stream.set_write_timeout(Some(Duration::from_millis(100)))?;
        stream.write_all(&[1])?;
        Ok(true)
    }
}

impl Drop for StartupBarrier {
    fn drop(&mut self) {
        if let Some((path, dev, ino)) = &self.host_path
            && std::fs::symlink_metadata(path).is_ok_and(|m| m.dev() == *dev && m.ino() == *ino)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Authenticate the kernel-reported host parent, then verify the runner's own
/// host PID and cgroup against the manager snapshot before doing any inner work.
pub fn wait_for_host_runner(path: &Path, parent_start: u64) -> Result<()> {
    let mut stream = UnixStream::connect(path)?;
    let peer = getsockopt(&stream, PeerCredentials)?;
    ensure!(
        peer.pid() > 0 && peer.uid() == geteuid().as_raw(),
        "host runner parent UID/PID mismatch"
    );
    let start = || -> Result<u64> {
        let text = std::fs::read_to_string(format!("/proc/{}/stat", peer.pid()))?;
        text.rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .ok_or_else(|| anyhow::anyhow!("missing host runner parent identity"))?
            .parse()
            .map_err(Into::into)
    };
    ensure!(
        start()? == parent_start,
        "host runner parent identity changed"
    );
    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut bytes = Vec::new();
    for _ in 0..4120 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "host runner startup deadline exhausted"
        );
        stream.set_read_timeout(Some(remaining))?;
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        bytes.push(byte[0]);
        if bytes.iter().filter(|b| **b == b'\n').count() == 2 {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes)?;
    let mut fields = text.split_terminator('\n');
    let pid: u32 = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing runner PID"))?
        .parse()?;
    let group = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing runner cgroup"))?;
    let placement = std::fs::read_to_string("/proc/self/cgroup")?;
    ensure!(
        fields.next().is_none()
            && text.ends_with('\n')
            && pid == std::process::id()
            && placement
                .lines()
                .any(|line| line.strip_prefix("0::") == Some(group))
            && start()? == parent_start,
        "host runner native identity changed"
    );
    stream.write_all(&[1])?;
    Ok(())
}

fn address(name: &str) -> Result<SocketAddr> {
    ensure!(
        name.starts_with("amc-start-")
            && name.len() <= 90
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid startup socket name"
    );
    Ok(SocketAddr::from_abstract_name(name)?)
}

/// Wait for the creating runner. Failure, disconnect or timeout prevents exec.
/// Exec preserves the helper's MainPID, streams and native execution domain.
pub fn wait_for_startup(name: &str, parent_pid: u32) -> Result<()> {
    wait_with_timeout(name, parent_pid, Duration::from_secs(30))
}

fn wait_with_timeout(name: &str, parent_pid: u32, timeout: Duration) -> Result<()> {
    let mut stream = UnixStream::connect_addr(&address(name)?)?;
    let peer = getsockopt(&stream, PeerCredentials)?;
    ensure!(
        parent_pid > 0 && peer.pid() as u32 == parent_pid && peer.uid() == geteuid().as_raw(),
        "startup runner does not match parent PID/UID"
    );
    stream.set_read_timeout(Some(timeout))?;
    let mut response = [0];
    stream.read_exact(&mut response)?;
    ensure!(response == [1], "startup runner did not authorize exec");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn barrier() -> StartupBarrier {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        StartupBarrier::new(format!(
            "amc-start-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap()
    }

    #[test]
    fn readiness_needs_the_native_pid_and_placement_not_just_a_connection() {
        let barrier = barrier();
        assert!(!barrier.release(std::process::id(), "/unrelated").unwrap());
        let mut helper = UnixStream::connect_addr(&address(barrier.name()).unwrap()).unwrap();
        assert!(barrier.release(std::process::id(), "/unrelated").is_err());
        assert!(helper.read_exact(&mut [0]).is_err());
        let mut helper = UnixStream::connect_addr(&address(barrier.name()).unwrap()).unwrap();
        assert!(
            barrier
                .release(std::process::id() + 1, "/unrelated")
                .is_err()
        );
        assert!(helper.read_exact(&mut [0]).is_err());
    }

    #[test]
    fn only_the_creating_runner_can_release_the_helper() {
        let barrier = barrier();
        assert!(wait_for_startup(barrier.name(), std::process::id() + 1).is_err());
    }

    #[test]
    fn missing_ack_times_out_without_authorizing_exec() {
        let barrier = barrier();
        assert!(
            wait_with_timeout(
                barrier.name(),
                std::process::id(),
                Duration::from_millis(20)
            )
            .is_err()
        );
    }

    #[test]
    fn matching_peer_receives_ack_and_disconnect_is_not_readiness() {
        let barrier = barrier();
        let placement = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let group = placement
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap();
        let mut helper = UnixStream::connect_addr(&address(barrier.name()).unwrap()).unwrap();
        assert!(barrier.release(std::process::id(), group).unwrap());
        let mut ack = [0];
        helper.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [1]);
        let helper = UnixStream::connect_addr(&address(barrier.name()).unwrap()).unwrap();
        drop(helper);
        assert!(barrier.release(std::process::id(), group).is_err());
    }
}
