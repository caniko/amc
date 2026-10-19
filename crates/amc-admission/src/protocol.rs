//! Versioned, bounded local protocol. Entry carries a one-use capability;
//! commands stay client-owned and capabilities are never included in status.
use crate::ledger::{Backpressure, Decision, Entry, QueueRequest};
use anyhow::{Result, ensure};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{MsgFlags, recv, send},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    os::fd::{AsFd, AsRawFd},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_FRAME: u64 = 2_097_152;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub message: Message,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    /// Resource waiting has no implicit expiry. Older coordinators fail closed.
    EnqueueWaiting {
        contract: String,
        request: QueueRequest,
    },
    Enqueue {
        contract: String,
        wait_ms: u64,
    },
    /// Explicit bounded estimate. Older coordinators reject this operation.
    EnqueueSized {
        contract: String,
        wait_ms: u64,
        memory_max: u64,
    },
    /// Select policy-owned short-call backing before native submission.
    EnqueueBounded {
        contract: String,
        wait_ms: u64,
        memory_max: u64,
        runtime_max_sec: u64,
    },
    Poll {
        id: String,
    },
    Enter {
        id: String,
        key: String,
    },
    Cancel {
        id: String,
    },
    Status,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backpressure: Option<Backpressure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Decision>,
    /// Optional root broker; native helpers acquire host capacity before entry.
    #[serde(default)]
    pub host_socket: Option<std::path::PathBuf>,
    /// One-use capability returned only to the enqueue caller, never by Poll/Status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_key: Option<String>,
    pub entry: Option<Entry>,
    pub status: Option<Status>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Status {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub protected_pools: std::collections::BTreeMap<String, u64>,
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
    pub committed_bytes: u64,
    pub entries: Vec<Entry>,
    pub waiting: std::collections::BTreeMap<String, Decision>,
    /// IDs whose last native termination query was inconclusive. Capacity held.
    pub unreconciled: Vec<String>,
}

pub fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    read_frame_cancellable(stream, None)
}

pub(crate) fn read_frame_cancellable<T: DeserializeOwned>(
    stream: &mut UnixStream,
    control: Option<(Instant, &dyn Fn() -> bool)>,
) -> Result<T> {
    let timeout = stream.read_timeout()?;
    let deadline = Instant::now()
        .checked_add(timeout.unwrap_or(Duration::from_secs(10)))
        .ok_or_else(|| anyhow::anyhow!("invalid admission frame deadline"))?;
    let deadline = control.map_or(deadline, |(until, _)| until.min(deadline));
    let stopping = control.map(|(_, stopping)| stopping);
    {
        let mut buffer = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            remaining(deadline, stopping)?;
            let bound = (MAX_FRAME + 1 - buffer.len() as u64).min(chunk.len() as u64) as usize;
            let count = match recv(
                stream.as_raw_fd(),
                &mut chunk[..bound],
                MsgFlags::MSG_DONTWAIT,
            ) {
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
                    wait_ready(stream, PollFlags::POLLIN, deadline, stopping)?;
                    continue;
                }
                result => result?,
            };
            ensure!(count > 0, "incomplete admission frame");
            let end = chunk[..count].iter().position(|byte| *byte == b'\n');
            buffer.extend_from_slice(&chunk[..end.map_or(count, |index| index + 1)]);
            ensure!(buffer.len() as u64 <= MAX_FRAME, "invalid admission frame");
            if end.is_some() {
                remaining(deadline, stopping)?;
                let value = serde_json::from_slice(&buffer)?;
                if stopping.is_some() {
                    remaining(deadline, stopping)?;
                }
                return Ok(value);
            }
        }
    }
}

fn remaining(deadline: Instant, stopping: Option<&dyn Fn() -> bool>) -> Result<Duration> {
    ensure!(
        !stopping.is_some_and(|stop| stop()),
        "admission frame cancelled"
    );
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "admission frame deadline exceeded");
    Ok(remaining)
}

// Per-call nonblocking flags preserve the caller's descriptor state. A socket's
// idle timeout cannot enforce a whole-frame deadline while bytes keep moving.
fn wait_ready(
    stream: &UnixStream,
    events: PollFlags,
    deadline: Instant,
    stopping: Option<&dyn Fn() -> bool>,
) -> Result<()> {
    loop {
        let left = remaining(deadline, stopping)?;
        let timeout = PollTimeout::try_from(if stopping.is_some() {
            left.min(Duration::from_millis(50))
        } else {
            left
        })?;
        let mut fds = [PollFd::new(stream.as_fd(), events)];
        match poll(&mut fds, timeout) {
            Err(Errno::EINTR) => continue,
            Ok(0) => continue,
            result => {
                result?;
                remaining(deadline, stopping)?;
                return Ok(());
            }
        }
    }
}

pub fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    write_frame_cancellable(stream, value, None)
}

pub(crate) fn write_frame_cancellable<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    control: Option<(Instant, &dyn Fn() -> bool)>,
) -> Result<()> {
    if let Some((deadline, stopping)) = control {
        remaining(deadline, Some(stopping))?;
    }
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        (bytes.len() as u64) < MAX_FRAME,
        "admission response exceeds size bound"
    );
    bytes.push(b'\n');
    let timeout = stream.write_timeout()?;
    let deadline = Instant::now()
        .checked_add(timeout.unwrap_or(Duration::from_secs(2)))
        .ok_or_else(|| anyhow::anyhow!("invalid admission response deadline"))?;
    let deadline = control.map_or(deadline, |(until, _)| until.min(deadline));
    let stopping = control.map(|(_, stopping)| stopping);
    {
        let mut written = 0;
        while written < bytes.len() {
            remaining(deadline, stopping)?;
            let count = match send(
                stream.as_raw_fd(),
                &bytes[written..],
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
            ) {
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
                    wait_ready(stream, PollFlags::POLLOUT, deadline, stopping)?;
                    continue;
                }
                result => result?,
            };
            ensure!(count > 0, "incomplete admission response");
            written += count;
        }
        remaining(deadline, stopping)?;
        Ok(())
    }
}

pub fn call(socket: &Path, message: Message) -> Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write_frame(
        &mut stream,
        &Request {
            version: 1,
            message,
        },
    )?;
    let response: Response = read_frame(&mut stream)?;
    ensure!(
        response.version == 1,
        "unsupported admission response version"
    );
    if let Some(backpressure) = &response.backpressure {
        return Err(backpressure.clone().into());
    }
    if let Some(error) = &response.error {
        anyhow::bail!("{error}");
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, thread};

    #[test]
    fn cancellable_frames_interrupt_stalled_reads_and_writes() {
        for writing in [false, true] {
            let (mut client, _peer) = UnixStream::pair().unwrap();
            let start = Instant::now();
            let stopping = || start.elapsed() >= Duration::from_millis(50);
            let control = Some((
                start + Duration::from_secs(2),
                &stopping as &dyn Fn() -> bool,
            ));
            let result = if writing {
                write_frame_cancellable(&mut client, &"x".repeat(1_048_576), control)
            } else {
                read_frame_cancellable::<serde_json::Value>(&mut client, control).map(|_| ())
            };
            let elapsed = start.elapsed();
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            assert!(
                elapsed < Duration::from_millis(250),
                "{writing}: {elapsed:?}"
            );
        }
    }

    #[test]
    fn cancellable_frame_deadlines_are_not_extended_by_trickling_bytes() {
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        let sender = thread::spawn(move || {
            let _ = peer.write_all(b"{");
            for _ in 0..20 {
                if peer.write_all(b" ").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(25));
            }
            let _ = peer.write_all(b"\"granted\":true}\n");
        });
        let start = Instant::now();
        let result = read_frame_cancellable::<serde_json::Value>(
            &mut client,
            Some((start + Duration::from_millis(100), &|| false)),
        );
        let elapsed = start.elapsed();
        drop(client);
        sender.join().unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("deadline exceeded")
        );
        assert!(elapsed < Duration::from_millis(250), "{elapsed:?}");
    }

    #[test]
    fn cancellation_after_consuming_a_complete_grant_frame_is_not_success() {
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        write_frame(
            &mut peer,
            &serde_json::json!({"version": 1, "granted": true}),
        )
        .unwrap();
        let probe = client.try_clone().unwrap();
        // Cancel once the grant bytes have actually been consumed, not before recv.
        let stopping = || {
            matches!(
                recv(
                    probe.as_raw_fd(),
                    &mut [0],
                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_PEEK
                ),
                Err(Errno::EAGAIN)
            )
        };
        let result = read_frame_cancellable::<serde_json::Value>(
            &mut client,
            Some((Instant::now() + Duration::from_secs(2), &stopping)),
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn already_cancelled_writes_do_not_send_a_request() {
        let (mut client, peer) = UnixStream::pair().unwrap();
        let result = write_frame_cancellable(
            &mut client,
            &serde_json::json!({"version": 1}),
            Some((Instant::now() + Duration::from_secs(2), &|| true)),
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(
            recv(peer.as_raw_fd(), &mut [0], MsgFlags::MSG_DONTWAIT),
            Err(Errno::EAGAIN)
        );
    }
}
