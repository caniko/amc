//! Bounded, non-interactive manager queries. Workload forwarding does not use this path.
use std::{
    io::{ErrorKind, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};

/// Default bound for one manager query.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
pub const OUTPUT_LIMIT: usize = 64 * 1024;

/// Run a bounded manager query, never including subprocess output in errors.
pub fn capture(command: &mut Command) -> Result<String> {
    capture_with_limit(command, QUERY_TIMEOUT, OUTPUT_LIMIT)
}

/// Run a bounded manager query with a caller-supplied timeout, clamped to
/// at `QUERY_TIMEOUT`, never extending the caller's budget. A zero timeout fails immediately without
/// spawning: the caller's deadline is already exhausted.
pub fn capture_with_timeout(command: &mut Command, timeout: Duration) -> Result<String> {
    if timeout.is_zero() {
        bail!("diagnostic deadline exhausted; state unknown");
    }
    capture_with_limit(command, timeout.min(QUERY_TIMEOUT), OUTPUT_LIMIT)
}

fn capture_with_limit(command: &mut Command, timeout: Duration, limit: usize) -> Result<String> {
    let deadline = Instant::now() + timeout;
    // A nonblocking socket avoids both pipe-reader threads and a descendant
    // holding stdout open indefinitely after the queried process has exited.
    //
    // Spawning can fail transiently (fork EAGAIN) under a process storm, which
    // must not be reported as an unobservable unit. Retry briefly; permanent
    // failures surface after ~100ms with the OS error preserved.
    let mut attempts = 0;
    let (mut reader, mut child) = loop {
        if Instant::now() >= deadline {
            bail!("diagnostic deadline exhausted; state unknown");
        }
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::null());
        let spawned = {
            #[cfg(test)]
            let _guard = crate::test_support::executable_fixture_guard();
            command.spawn()
        };
        match spawned {
            Ok(child) => break (reader, child),
            Err(_) if attempts < 20 => {
                attempts += 1;
                thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "diagnostic client could not be spawned: {error}"
                ));
            }
        }
    };
    command.stdout(Stdio::null());
    let mut bytes = Vec::new();
    let result = (|| {
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                bail!("diagnostic query timed out; state unknown");
            }
            let mut buffer = [0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    if bytes.len() + n > limit {
                        bail!("diagnostic output exceeded byte limit; state unknown");
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(_) => bail!("diagnostic output unreadable; state unknown"),
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("diagnostic query failed; state unknown");
                }
                if eof {
                    return String::from_utf8(bytes).map_err(|_| {
                        anyhow::anyhow!("diagnostic output malformed; state unknown")
                    });
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        // Do not wait indefinitely for an uninterruptible process.
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_never_include_subprocess_text() {
        let error = capture(Command::new("sh").args([
            "-c",
            "printf SENSITIVE_STDOUT; printf SENSITIVE_STDERR >&2; exit 1",
        ]))
        .unwrap_err();
        assert!(!format!("{error:#}").contains("SENSITIVE"));
    }

    #[test]
    fn diagnostic_time_and_bytes_are_bounded() {
        let started = Instant::now();
        assert!(
            capture_with_limit(
                Command::new("sh").args(["-c", "exec sleep 10"]),
                Duration::from_millis(30),
                64,
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            capture_with_limit(
                Command::new("sh").args(["-c", "printf SENSITIVE_TOO_LONG"]),
                Duration::from_secs(1),
                4,
            )
            .unwrap_err()
            .to_string()
            .contains("byte limit")
        );
    }
}
