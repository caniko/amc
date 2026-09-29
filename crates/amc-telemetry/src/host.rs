//! Passive, allowlisted context from the observer's procfs view, not a promise
//! of unrestricted host visibility. No process metadata or raw input is emitted.

use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use serde::Serialize;

use crate::{Measurement, measure::reason};

const MAX_PROC_BYTES: u64 = 64 * 1024;
const MEMORY_KEYS: &[&str] = &[
    "MemTotal",
    "MemAvailable",
    "SwapTotal",
    "SwapFree",
    "Buffers",
    "Cached",
    "Dirty",
    "Writeback",
];
const VM_KEYS: &[&str] = &["pswpin", "pswpout", "pgmajfault"];

/// Sequential observations: meminfo values are bytes; pswpin/out count pages,
/// pgmajfault counts faults, and PSI totals count microseconds since boot.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    pub observed_unix_ms: Option<u128>,
    pub observed_monotonic_ms: Option<u64>,
    pub files: BTreeMap<String, Measurement>,
}

fn read_bounded(path: &Path, limit: u64) -> Result<String, &'static str> {
    let mut bytes = Vec::new();
    let file = File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => reason::MISSING,
        std::io::ErrorKind::PermissionDenied => reason::PERMISSION,
        _ => reason::UNREADABLE,
    })?;
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| reason::UNREADABLE)?;
    if bytes.len() as u64 > limit {
        return Err(reason::BYTE_LIMIT);
    }
    String::from_utf8(bytes).map_err(|_| reason::INVALID_UTF8)
}

fn counter(text: Result<&str, &'static str>, key: &str, kib: bool) -> Measurement {
    let value = (|| {
        let text = text?;
        let mut rows = text.lines().filter_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(key)).then_some(fields)
        });
        let mut fields = rows.next().ok_or(reason::MISSING)?;
        if rows.next().is_some() {
            return Err(reason::MALFORMED);
        }
        let number = fields.next().ok_or(reason::MALFORMED)?;
        if !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(reason::MALFORMED);
        }
        let number = number.parse::<u64>().map_err(|_| reason::MALFORMED)?;
        if (kib && fields.next() != Some("kB")) || fields.next().is_some() {
            return Err(reason::MALFORMED);
        }
        number
            .checked_mul(if kib { 1024 } else { 1 })
            .ok_or(reason::MALFORMED)
    })();
    match value {
        Ok(value) => Measurement::known(value.into()),
        Err(reason) => Measurement::unknown(reason),
    }
}

/// Read aggregate memory, swap activity, and CPU/IO/memory pressure. The CLI
/// supplies `/proc`; a directory parameter permits deterministic local tests.
/// Missing fields are independently unknown, never synthesized as zero.
pub fn snapshot(proc_root: &Path, monotonic_ms: Option<u64>) -> HostSnapshot {
    let observed_unix_ms = crate::now_unix_ms();
    let mut files = BTreeMap::new();
    for (name, keys, kib) in [("meminfo", MEMORY_KEYS, true), ("vmstat", VM_KEYS, false)] {
        let text = read_bounded(&proc_root.join(name), MAX_PROC_BYTES);
        for key in keys {
            let source_key = if kib {
                format!("{key}:")
            } else {
                key.to_string()
            };
            files.insert(
                format!("{name}.{key}"),
                counter(text.as_deref().map_err(|reason| *reason), &source_key, kib),
            );
        }
    }
    for resource in ["memory", "cpu", "io"] {
        let value = read_bounded(
            &proc_root.join("pressure").join(resource),
            crate::measure::MAX_FILE_BYTES as u64,
        )
        .and_then(|text| {
            crate::parse(&format!("{resource}.pressure"), &text).ok_or(reason::MALFORMED)
        });
        files.insert(
            format!("pressure.{resource}"),
            match value {
                Ok(value) => Measurement::known(value),
                Err(reason) => Measurement::unknown(reason),
            },
        );
    }
    HostSnapshot {
        observed_unix_ms,
        observed_monotonic_ms: monotonic_ms,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_allowlisted_strict_and_independently_unknown() {
        assert_eq!(
            counter(Ok("MemTotal: 12 kB\nPRIVATE: secret"), "MemTotal:", true).value,
            Some(serde_json::json!(12288))
        );
        assert_eq!(
            counter(Ok("pswpin 0"), "pswpin", false).value,
            Some(0.into())
        );
        for text in [
            "MemTotal: -1 kB",
            "MemTotal: +1 kB",
            "MemTotal: 1 MB",
            "MemTotal: 1 kB extra",
            "MemTotal: 18446744073709551615 kB",
            "MemTotal: 1 kB\nMemTotal: 2 kB",
            "MemTotal: secret kB",
        ] {
            let measurement = counter(Ok(text), "MemTotal:", true);
            assert_eq!(
                measurement,
                Measurement::unknown(reason::MALFORMED),
                "{text}"
            );
        }
        assert_eq!(
            counter(Ok("MemTotal: 1 kB"), "SwapFree:", true),
            Measurement::unknown(reason::MISSING)
        );
        assert_eq!(
            counter(Err(reason::PERMISSION), "SwapFree:", true),
            Measurement::unknown(reason::PERMISSION)
        );
        assert_eq!(
            counter(Ok("pswpin 1 kB"), "pswpin", false),
            Measurement::unknown(reason::MALFORMED)
        );
    }

    #[test]
    fn cpu_pressure_can_lack_full_without_relaxing_memory_or_io() {
        let text = "some avg10=1.00 avg60=0.00 avg300=0.00 total=9\n";
        let cpu = crate::parse("cpu.pressure", text).unwrap();
        assert_eq!(cpu["some"]["total"], 9);
        assert!(cpu.get("full").is_none());
        assert!(crate::parse("memory.pressure", text).is_none());
        assert!(crate::parse("io.pressure", text).is_none());
        assert!(crate::parse("cpu.pressure", &text.replace("some", "full")).is_none());
        assert!(crate::parse("cpu.pressure", &text.replace("1.00", "NaN")).is_none());
        assert!(crate::parse("cpu.pressure", &format!("{text}{text}")).is_none());
    }

    #[test]
    fn host_snapshot_is_bounded_private_and_preserves_missing_fields() {
        let root = std::env::temp_dir().join(format!(
            "amc-host-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("pressure")).unwrap();
        std::fs::write(
            root.join("meminfo"),
            "MemTotal: 16 kB\nMemAvailable: 8 kB\nPRIVATE: secret",
        )
        .unwrap();
        std::fs::write(
            root.join("vmstat"),
            "pswpin 3\npswpout 4\npgmajfault 5\nPRIVATE secret",
        )
        .unwrap();
        let pressure = "some avg10=1.00 avg60=0.00 avg300=0.00 total=9\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        std::fs::write(root.join("pressure/memory"), pressure).unwrap();
        std::fs::write(root.join("pressure/cpu"), "SECRET").unwrap();
        let observed = snapshot(&root, Some(123));
        assert_eq!(observed.observed_monotonic_ms, Some(123));
        assert!(observed.observed_unix_ms.is_some());
        assert_eq!(observed.files.len(), 14);
        assert_eq!(
            observed.files["meminfo.MemAvailable"].value,
            Some(8192.into())
        );
        assert_eq!(observed.files["vmstat.pswpin"].value, Some(3.into()));
        assert_eq!(
            observed.files["pressure.memory"].value.as_ref().unwrap()["some"]["total"],
            9
        );
        assert_eq!(
            observed.files["pressure.cpu"].unknown.as_deref(),
            Some(reason::MALFORMED)
        );
        assert_eq!(
            observed.files["pressure.io"].unknown.as_deref(),
            Some(reason::MISSING)
        );
        assert!(observed.files.values().all(Measurement::is_coherent));
        assert!(!serde_json::to_string(&observed).unwrap().contains("secret"));
        std::fs::write(
            root.join("meminfo"),
            vec![b'x'; MAX_PROC_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            snapshot(&root, None).files["meminfo.MemTotal"]
                .unknown
                .as_deref(),
            Some(reason::BYTE_LIMIT)
        );
        std::fs::write(root.join("meminfo"), [0xff]).unwrap();
        assert_eq!(
            snapshot(&root, None).files["meminfo.MemTotal"]
                .unknown
                .as_deref(),
            Some(reason::INVALID_UTF8)
        );
        std::fs::write(
            root.join("pressure/memory"),
            vec![b'x'; crate::measure::MAX_FILE_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            snapshot(&root, None).files["pressure.memory"]
                .unknown
                .as_deref(),
            Some(reason::BYTE_LIMIT)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
