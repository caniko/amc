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
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    time::Duration,
};

/// A one-use rendezvous held by the runner until native startup is pinned.
#[derive(Debug)]
pub struct StartupBarrier {
    name: String,
    listener: UnixListener,
}

impl StartupBarrier {
    /// Bind a fresh, one-attempt name before creating the submission client.
    pub fn new(name: String) -> Result<Self> {
        let listener = UnixListener::bind_addr(&address(&name)?)?;
        listener.set_nonblocking(true)?;
        Ok(Self { name, listener })
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
