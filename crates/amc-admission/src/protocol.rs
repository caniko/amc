//! Versioned, bounded local protocol. Requests carry no commands or secrets.
use crate::ledger::{Decision, Entry};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
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
    Enter { id: String },
    Cancel { id: String },
    Status,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Response {
    pub version: u32,
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
    let mut buffer = Vec::new();
    BufReader::new(stream.take(MAX_FRAME + 1)).read_until(b'\n', &mut buffer)?;
    ensure!(
        buffer.len() as u64 <= MAX_FRAME && buffer.last() == Some(&b'\n'),
        "invalid admission frame"
    );
    Ok(serde_json::from_slice(&buffer)?)
}

pub fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        (bytes.len() as u64) < MAX_FRAME,
        "admission response exceeds size bound"
    );
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
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
