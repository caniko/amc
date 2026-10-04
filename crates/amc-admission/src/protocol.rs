//! Versioned, bounded local protocol. Entry carries a one-use capability;
//! commands stay client-owned and capabilities are never included in status.
use crate::ledger::{Decision, Entry};
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
    Enqueue { contract: String, wait_ms: u64 },
    Poll { id: String },
    Enter { id: String, key: String },
    Cancel { id: String },
    Status,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
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
    pub budget_bytes: u64,
    pub reserve_bytes: u64,
    pub committed_bytes: u64,
    pub entries: Vec<Entry>,
    pub waiting: std::collections::BTreeMap<String, Decision>,
    /// IDs whose last native termination query was inconclusive. Capacity held.
    pub unreconciled: Vec<String>,
}

pub fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let timeout = stream.read_timeout()?;
    let deadline = Instant::now()
        .checked_add(timeout.unwrap_or(Duration::from_secs(10)))
        .ok_or_else(|| anyhow::anyhow!("invalid admission frame deadline"))?;
    {
        let mut buffer = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            remaining(deadline)?;
            let bound = (MAX_FRAME + 1 - buffer.len() as u64).min(chunk.len() as u64) as usize;
            let count = match recv(
                stream.as_raw_fd(),
                &mut chunk[..bound],
                MsgFlags::MSG_DONTWAIT,
            ) {
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
                    wait_ready(stream, PollFlags::POLLIN, deadline)?;
                    continue;
                }
                result => result?,
            };
            ensure!(count > 0, "incomplete admission frame");
            let end = chunk[..count].iter().position(|byte| *byte == b'\n');
            buffer.extend_from_slice(&chunk[..end.map_or(count, |index| index + 1)]);
            ensure!(buffer.len() as u64 <= MAX_FRAME, "invalid admission frame");
            if end.is_some() {
                remaining(deadline)?;
                return Ok(serde_json::from_slice(&buffer)?);
            }
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "admission frame deadline exceeded");
    Ok(remaining)
}

// Per-call nonblocking flags preserve the caller's descriptor state. A socket's
// idle timeout cannot enforce a whole-frame deadline while bytes keep moving.
fn wait_ready(stream: &UnixStream, events: PollFlags, deadline: Instant) -> Result<()> {
    loop {
        let timeout = PollTimeout::try_from(remaining(deadline)?)?;
        let mut fds = [PollFd::new(stream.as_fd(), events)];
        match poll(&mut fds, timeout) {
            Err(Errno::EINTR) => continue,
            Ok(0) => continue,
            result => {
                result?;
                remaining(deadline)?;
                return Ok(());
            }
        }
    }
}

pub fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
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
    {
        let mut written = 0;
        while written < bytes.len() {
            remaining(deadline)?;
            let count = match send(
                stream.as_raw_fd(),
                &bytes[written..],
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
            ) {
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => {
                    wait_ready(stream, PollFlags::POLLOUT, deadline)?;
                    continue;
                }
                result => result?,
            };
            ensure!(count > 0, "incomplete admission response");
            written += count;
        }
        remaining(deadline)?;
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
    if let Some(error) = &response.error {
        anyhow::bail!("{error}");
    }
    Ok(response)
}
