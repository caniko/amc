use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Value, json};

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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    pub value: Option<Value>,
    pub unknown: Option<&'static str>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub path: String,
    pub observed_unix_ms: Option<u128>,
    pub files: BTreeMap<String, Measurement>,
}

pub fn now() -> Option<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|v| v.as_millis())
}

pub fn snapshot(directory: &Path, path: &str) -> Snapshot {
    Snapshot {
        path: path.into(),
        observed_unix_ms: now(),
        files: FILES
            .iter()
            .map(|name| (name.to_string(), read(directory, name)))
            .collect(),
    }
}

fn read(directory: &Path, name: &str) -> Measurement {
    let result = (|| {
        let file = File::open(directory.join(name)).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => "missing or disappeared",
            std::io::ErrorKind::PermissionDenied => "permission denied",
            _ => "unreadable",
        })?;
        let mut text = String::new();
        file.take(4097)
            .read_to_string(&mut text)
            .map_err(|_| "unreadable or invalid UTF-8")?;
        if text.len() > 4096 {
            return Err("byte limit exceeded");
        }
        parse(name, text.trim()).ok_or("malformed")
    })();
    match result {
        Ok(value) => Measurement {
            value: Some(value),
            unknown: None,
        },
        Err(reason) => Measurement {
            value: None,
            unknown: Some(reason),
        },
    }
}

fn parse(name: &str, text: &str) -> Option<Value> {
    if name.ends_with("events") || name == "memory.events.local" {
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
            let value = fields[1].parse::<u64>().ok()?;
            if result.insert(fields[0].into(), json!(value)).is_some() {
                return None;
            }
        }
        return (!result.is_empty()).then_some(Value::Object(result));
    }
    if name == "memory.pressure" {
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
                    "total" => json!(value.parse::<u64>().ok()?),
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
        return (result.len() == 2).then_some(Value::Object(result));
    }
    if text == "max" && matches!(name, "memory.max" | "memory.high" | "memory.swap.max") {
        return Some(json!("max"));
    }
    let value = text.parse::<u64>().ok()?;
    if name == "memory.oom.group" && value > 1 {
        return None;
    }
    Some(json!(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_not_zero_and_malformed_text_is_not_reprinted() {
        let directory = std::env::temp_dir().join(format!("amc-telemetry-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        assert!(read(&directory, "memory.swap.current").value.is_none());
        std::fs::write(directory.join("memory.swap.current"), "SENSITIVE").unwrap();
        let malformed = read(&directory, "memory.swap.current");
        assert_eq!(malformed.unknown, Some("malformed"));
        assert!(
            !serde_json::to_string(&malformed)
                .unwrap()
                .contains("SENSITIVE")
        );
        std::fs::write(directory.join("memory.swap.current"), "0").unwrap();
        assert_eq!(
            read(&directory, "memory.swap.current").value,
            Some(json!(0))
        );
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(read(&directory, "memory.swap.current").unknown.is_some());
        assert!(parse("memory.events", "oom 0\noom 1").is_none());
        assert!(parse("memory.pressure", "some avg10=NaN").is_none());
    }
}
