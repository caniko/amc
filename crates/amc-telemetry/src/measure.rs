//! Bounded readers and strict parsers for cgroup telemetry files.
//!
//! Core is transport-independent: callers supply a directory, we never spawn
//! subprocesses, install subscribers, or touch manager-owned state.
//! Unknown is never zero: every failure maps to a stable sanitized reason
//! code, and malformed bytes are never reprinted.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Files observed by [`crate::snapshot`]. Stable order for deterministic JSON.
pub const FILES: &[&str] = &[
    "memory.current",
    "memory.peak",
    "memory.swap.current",
    "memory.swap.peak",
    "memory.min",
    "memory.low",
    "memory.high",
    "memory.max",
    "memory.swap.max",
    "memory.oom.group",
    "memory.events",
    "memory.events.local",
    "memory.pressure",
    "cgroup.events",
];

/// Max bytes read per file (4 KiB + 1 probe byte to detect overflow).
pub const MAX_FILE_BYTES: usize = 4096;

/// Stable, sanitized unknown-reason codes. No provider/file text is echoed.
pub mod reason {
    pub const MISSING: &str = "missing-or-disappeared";
    pub const PERMISSION: &str = "permission-denied";
    pub const UNREADABLE: &str = "unreadable";
    pub const INVALID_UTF8: &str = "invalid-utf8";
    pub const BYTE_LIMIT: &str = "byte-limit-exceeded";
    pub const MALFORMED: &str = "malformed";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const NOT_COLLECTED: &str = "not-collected";
}

/// One file observation: exactly one of `value` / `unknown` is `Some`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    pub value: Option<Value>,
    pub unknown: Option<String>,
}

impl Measurement {
    pub fn known(value: Value) -> Self {
        Self {
            value: Some(value),
            unknown: None,
        }
    }

    pub fn unknown(reason: &'static str) -> Self {
        Self {
            value: None,
            unknown: Some(reason.to_string()),
        }
    }

    /// Invariant: never both set, never both unset.
    pub fn is_coherent(&self) -> bool {
        self.value.is_some() != self.unknown.is_some()
    }
}

fn io_reason(error: std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => reason::MISSING,
        std::io::ErrorKind::PermissionDenied => reason::PERMISSION,
        _ => reason::UNREADABLE,
    }
}

/// Read and parse one file with bounded I/O. Never reprints file bytes.
pub fn read_measurement(directory: &Path, name: &str) -> Measurement {
    let result = (|| {
        let mut file = File::open(directory.join(name)).map_err(io_reason)?;
        read_pinned_measurement(&mut file, name)
    })();
    match result {
        Ok(value) => Measurement::known(value),
        Err(reason) => Measurement::unknown(reason),
    }
}

/// Parse the contents of an already-open file with bounded I/O.
/// Seeks to the start first so one handle serves repeated samples.
fn read_pinned_measurement(file: &mut File, name: &str) -> Result<Value, &'static str> {
    file.seek(SeekFrom::Start(0)).map_err(io_reason)?;
    let mut text = String::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|_| reason::INVALID_UTF8)?;
    if text.len() > MAX_FILE_BYTES {
        return Err(reason::BYTE_LIMIT);
    }
    parse(name, text.trim()).ok_or(reason::MALFORMED)
}

/// A set of telemetry files held open across samples.
///
/// Opening once at attach time and re-reading via seek keeps every sample
/// anchored to the same file descriptions: a same-path replacement cannot
/// slip different objects into one interval through reopen races. Handles
/// that failed to open (absent controller, permissions) fall back to a
/// bounded path read on each tick, so a file appearing later is still
/// observed. Pair with a per-tick continuity check (inode, invocation):
/// the pinned handles prove *what* was read, the continuity check proves
/// it is still the same object.
#[derive(Debug)]
pub struct PinnedReader {
    directory: PathBuf,
    _directory_handle: File,
    handles: Vec<(String, Option<File>)>,
}

impl PinnedReader {
    /// Open every telemetry file under `directory`. Missing or
    /// unreadable files are recorded as `None` and retried via path.
    pub fn open(directory: &Path) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let directory_handle = File::open(directory)?;
        if !directory_handle.metadata()?.is_dir() {
            return Err(std::io::Error::other("telemetry source is not a directory"));
        }
        // Linux procfs resolves children relative to the held directory,
        // even if its original pathname is renamed or replaced.
        let directory = PathBuf::from(format!("/proc/self/fd/{}", directory_handle.as_raw_fd()));
        let handles = FILES
            .iter()
            .map(|name| (name.to_string(), File::open(directory.join(name)).ok()))
            .collect();
        Ok(Self {
            directory,
            _directory_handle: directory_handle,
            handles,
        })
    }

    /// Identity of the opened directory, not a later pathname lookup.
    pub fn inode(&self) -> std::io::Result<u64> {
        use std::os::unix::fs::MetadataExt;
        Ok(self._directory_handle.metadata()?.ino())
    }

    /// Snapshot all files: pinned handles where available, bounded path
    /// reads otherwise. Handles stay open across ticks, so every sample
    /// comes from the same file descriptions the continuity check
    /// validated: even a check-to-read race can only serve the original
    /// object, never a replacement. Callers attach identity separately.
    pub fn snapshot(
        &mut self,
        path: &str,
        sequence: Option<u64>,
        monotonic_ms: Option<u64>,
    ) -> Snapshot {
        let files = self
            .handles
            .iter_mut()
            .map(|(name, handle)| {
                if handle.is_none() {
                    match File::open(self.directory.join(&name[..])) {
                        Ok(file) => *handle = Some(file),
                        Err(error) => {
                            return (name.clone(), Measurement::unknown(io_reason(error)));
                        }
                    }
                }
                let measurement = match read_pinned_measurement(handle.as_mut().unwrap(), name) {
                    Ok(value) => Measurement::known(value),
                    Err(reason) => Measurement::unknown(reason),
                };
                (name.clone(), measurement)
            })
            .collect();
        Snapshot {
            schema_version: crate::SCHEMA_VERSION,
            path: path.to_string(),
            observed_unix_ms: crate::now_unix_ms(),
            observed_monotonic_ms: monotonic_ms,
            sequence,
            invocation_id: None,
            boot_id: None,
            inode: None,
            files,
        }
    }
}

/// Strict parser shared by CLI inspection, the Rust observer, and tests.
/// This is the single implementation; the legacy Python parser is deprecated
/// and must match this behavior byte-for-byte on the corpus in
/// `tests/parser_corpus` (see `compare::metric_kind` for scope notes).
pub fn parse(name: &str, text: &str) -> Option<Value> {
    if name.ends_with("events") || name == "memory.events.local" {
        return parse_events(text);
    }
    if name == "memory.pressure" {
        return parse_pressure(text);
    }
    if text == "max" && matches!(name, "memory.max" | "memory.high" | "memory.swap.max") {
        return Some(json!("max"));
    }
    // Strict digits-only: `str::parse::<u64>` accepts a leading `+`, which
    // must not appear in kernel telemetry. Empty is rejected here too.
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = text.parse::<u64>().ok()?;
    if name == "memory.oom.group" && value > 1 {
        return None;
    }
    Some(json!(value))
}

/// Strict `u64` for telemetry values: digits-only, rejecting the leading
/// `+` that `str::parse::<u64>` would otherwise accept.
fn parse_u64_strict(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok()
}

fn parse_events(text: &str) -> Option<Value> {
    let mut result = serde_json::Map::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2
            || !fields[0]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return None;
        }
        // Strict digits-only: negatives, plus signs, and overflow rejected.
        let value = parse_u64_strict(fields[1])?;
        if result.insert(fields[0].into(), json!(value)).is_some() {
            return None; // duplicate key
        }
    }
    (!result.is_empty()).then_some(Value::Object(result))
}

fn parse_pressure(text: &str) -> Option<Value> {
    let mut result = serde_json::Map::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next()?;
        if !matches!(kind, "some" | "full") {
            return None;
        }
        let mut values = serde_json::Map::new();
        for field in fields {
            let (key, value) = field.split_once('=')?;
            let value = match key {
                "total" => json!(parse_u64_strict(value)?),
                "avg10" | "avg60" | "avg300" => {
                    let value = value.parse::<f64>().ok()?;
                    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                        return None;
                    }
                    json!(value)
                }
                _ => return None,
            };
            if values.insert(key.into(), value).is_some() {
                return None;
            }
        }
        if values.len() != 4 || result.insert(kind.into(), Value::Object(values)).is_some() {
            return None;
        }
    }
    (result.len() == 2).then_some(Value::Object(result))
}

/// Snapshot of one cgroup directory. `schema_version` is additive: legacy
/// consumers ignore unknown fields; new consumers check it.
fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub path: String,
    pub observed_unix_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub observed_monotonic_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sequence: Option<u64>,
    /// Continuity evidence captured with the sample. All optional: files
    /// without these fields carry no identity and must compare as
    /// `missing-identity`, never guessed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub invocation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub boot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inode: Option<u64>,
    pub files: BTreeMap<String, Measurement>,
}

impl Snapshot {
    /// Validate externally loaded observations before deriving evidence.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != crate::SCHEMA_VERSION {
            return Err("unsupported-schema-version");
        }
        if !self.path.starts_with('/')
            || self.path.len() > 4096
            || self.path.chars().any(char::is_control)
        {
            return Err("invalid-source-path");
        }
        if [&self.invocation_id, &self.boot_id].iter().any(|id| {
            id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 64 || id.chars().any(char::is_control)
            })
        }) {
            return Err("invalid-source-identity");
        }
        for (name, measurement) in &self.files {
            if !FILES.contains(&name.as_str())
                || !measurement.is_coherent()
                || measurement
                    .unknown
                    .as_ref()
                    .is_some_and(|reason| reason.is_empty() || reason.len() > 128)
            {
                return Err("invalid-measurement");
            }
            if let Some(value) = &measurement.value {
                let text = match name.as_str() {
                    "memory.events" | "memory.events.local" | "cgroup.events" => {
                        let object = value.as_object().ok_or("invalid-measurement")?;
                        object
                            .iter()
                            .map(|(key, value)| format!("{key} {value}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    "memory.pressure" => {
                        let object = value.as_object().ok_or("invalid-measurement")?;
                        let mut lines = Vec::new();
                        for (row, fields) in object {
                            let fields = fields.as_object().ok_or("invalid-measurement")?;
                            lines.push(format!(
                                "{} {}",
                                row,
                                fields
                                    .iter()
                                    .map(|(key, value)| format!("{key}={value}"))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            ));
                        }
                        lines.join("\n")
                    }
                    _ => value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string()),
                };
                if parse(name, &text).as_ref() != Some(value) {
                    return Err("invalid-measurement");
                }
            }
        }
        Ok(())
    }

    /// Source identity carried by this snapshot, if any. A snapshot with
    /// no identity fields yields `None`: callers must pass that through as
    /// missing identity rather than fabricating one from the path.
    pub fn source_identity(&self) -> Option<crate::identity::SourceIdentity> {
        if self.invocation_id.is_none() && self.boot_id.is_none() && self.inode.is_none() {
            return None;
        }
        Some(crate::identity::SourceIdentity {
            cgroup_path: self.path.clone(),
            unit: None,
            manager_context: None,
            uid: None,
            boot_id: self.boot_id.clone(),
            invocation_id: self.invocation_id.clone(),
            inode: self.inode,
        })
    }
}

impl Snapshot {
    pub fn coherent(&self) -> bool {
        self.files.values().all(Measurement::is_coherent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_observations_reject_contradictory_evidence() {
        let mut snapshot: Snapshot = serde_json::from_value(json!({
            "path": "/test", "observedUnixMs": 1,
            "files": {"memory.current": {"value": 0, "unknown": null}}
        }))
        .unwrap();
        assert!(snapshot.validate().is_ok());
        snapshot.files.get_mut("memory.current").unwrap().unknown = Some("missing".into());
        assert_eq!(snapshot.validate(), Err("invalid-measurement"));
        snapshot.files.clear();
        snapshot.invocation_id = Some(String::new());
        assert_eq!(snapshot.validate(), Err("invalid-source-identity"));
        snapshot.invocation_id = None;
        snapshot.schema_version = 99;
        assert_eq!(snapshot.validate(), Err("unsupported-schema-version"));
    }

    #[test]
    fn unknown_is_not_zero_and_malformed_text_is_not_reprinted() {
        let directory = std::env::temp_dir().join(format!(
            "amc-telemetry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        assert!(
            read_measurement(&directory, "memory.swap.current")
                .value
                .is_none()
        );
        std::fs::write(directory.join("memory.swap.current"), "SENSITIVE").unwrap();
        let malformed = read_measurement(&directory, "memory.swap.current");
        assert_eq!(malformed.unknown.as_deref(), Some(reason::MALFORMED));
        assert!(
            !serde_json::to_string(&malformed)
                .unwrap()
                .contains("SENSITIVE")
        );
        std::fs::write(directory.join("memory.swap.current"), "0").unwrap();
        assert_eq!(
            read_measurement(&directory, "memory.swap.current").value,
            Some(json!(0))
        );
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(parse("memory.events", "oom 0\noom 1").is_none());
        assert!(parse("memory.pressure", "some avg10=NaN").is_none());
    }

    #[test]
    fn parser_corpus_zero_unknown_unlimited() {
        // zero is a value, never unknown
        assert_eq!(parse("memory.current", "0"), Some(json!(0)));
        // "max" only for limit files
        assert_eq!(parse("memory.max", "max"), Some(json!("max")));
        assert!(parse("memory.current", "max").is_none());
        // oom.group domain
        assert_eq!(parse("memory.oom.group", "0"), Some(json!(0)));
        assert_eq!(parse("memory.oom.group", "1"), Some(json!(1)));
        assert!(parse("memory.oom.group", "2").is_none());
        // events: duplicates, uppercase, negatives, overflow rejected
        assert!(parse("memory.events", "oom_kill 1\noom_kill 2").is_none());
        assert!(parse("memory.events", "OOM 1").is_none());
        assert!(parse("memory.events", "oom -1").is_none());
        assert!(parse("memory.events", "oom 18446744073709551616").is_none());
        assert!(parse("memory.events", "").is_none());
        // pressure: totals must be u64, avgs finite 0..=100, both rows required
        assert!(
            parse(
                "memory.pressure",
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=0"
            )
            .is_none()
        );
        assert!(
            parse(
                "memory.pressure",
                "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0"
            )
            .is_some()
        );
        assert!(
            parse(
                "memory.pressure",
                "some avg10=inf avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0"
            )
            .is_none()
        );
    }

    #[test]
    fn pinned_reader_replays_held_descriptions_across_ticks() {
        let directory = std::env::temp_dir().join(format!(
            "amc-telemetry-pinned-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("memory.current"), "100\n").unwrap();
        std::fs::write(
            directory.join("memory.events"),
            "oom_kill 1\nmax 0\nlow 0\nhigh 0\noom 0\n",
        )
        .unwrap();
        let mut reader = PinnedReader::open(&directory).unwrap();
        let first = reader.snapshot("/x.service", Some(0), Some(0));
        assert!(first.coherent());
        assert_eq!(first.files["memory.current"].value, Some(json!(100)));
        // Mutating the path between ticks is visible through the held
        // descriptions on the next sample.
        std::fs::write(directory.join("memory.current"), "200\n").unwrap();
        let second = reader.snapshot("/x.service", Some(1), Some(20));
        assert_eq!(second.files["memory.current"].value, Some(json!(200)));
        let retired = directory.with_extension("retired");
        std::fs::rename(&directory, &retired).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("memory.current"), "999\n").unwrap();
        std::fs::write(directory.join("memory.swap.current"), "999\n").unwrap();
        let third = reader.snapshot("/x.service", Some(2), Some(40));
        assert_eq!(third.files["memory.current"].value, Some(json!(200)));
        assert!(third.files["memory.swap.current"].value.is_none());
        std::fs::remove_dir_all(retired).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn invalid_utf8_and_oversize_are_unknown_not_panic() {
        let dir = std::env::temp_dir().join(format!(
            "amc-telemetry-bad-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("memory.current"), [0xff, 0xfe]).unwrap();
        let m = read_measurement(&dir, "memory.current");
        assert!(m.value.is_none() && m.unknown.is_some());
        std::fs::write(dir.join("memory.current"), vec![b'1'; MAX_FILE_BYTES + 2]).unwrap();
        let m = read_measurement(&dir, "memory.current");
        assert_eq!(m.unknown.as_deref(), Some(reason::BYTE_LIMIT));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_files_stay_missing_until_they_appear() {
        // A file absent at open is retried via path on each tick: missing
        // stays explicit unknown, and a later appearance becomes readable
        // without reopening the reader. Deletion after appearance keeps
        // serving the held description only while the handle lives; a
        // never-opened file reports missing, never zero.
        let dir = std::env::temp_dir().join(format!(
            "amc-telemetry-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("memory.current"), "10\n").unwrap();
        let mut reader = PinnedReader::open(&dir).unwrap();
        let first = reader.snapshot("/x.service", Some(0), Some(0));
        assert_eq!(
            first.files["memory.events"].unknown.as_deref(),
            Some(reason::MISSING)
        );
        assert!(first.files["memory.events"].value.is_none());
        std::fs::write(dir.join("memory.events"), "oom_kill 2\n").unwrap();
        let second = reader.snapshot("/x.service", Some(1), Some(20));
        assert_eq!(
            second.files["memory.events"].value,
            Some(json!({"oom_kill": 2}))
        );
        std::fs::remove_file(dir.join("memory.events")).unwrap();
        // The held handle still serves the original object after unlink.
        let third = reader.snapshot("/x.service", Some(2), Some(40));
        assert_eq!(
            third.files["memory.events"].value,
            Some(json!({"oom_kill": 2}))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
