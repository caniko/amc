//! Bounded native observations. No cgroup writes or process-tree scanning.
use crate::ledger::{Contract, Entry, Headroom, Identity};
use amc_runner::{MemoryProvider, providers::CgroupV2Provider, systemd::capture};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::Command,
};

pub trait Native {
    fn can_enter(&self) -> bool {
        true
    }
    fn headroom(&self, contract: &Contract, reserve: u64) -> Result<Headroom>;
    fn identify(&self, entry: &Entry, peer_pid: i32) -> Result<Identity>;
    fn terminated(&self, entry: &Entry) -> Option<bool>;
    fn stop(&self, entry: &Entry) -> Result<()>;
}

pub struct Systemd {
    pub systemctl: PathBuf,
}

/// Optional host supervision without changing ordinary native admission.
pub struct Supervised<N> {
    pub native: N,
    pub health_file: Option<PathBuf>,
}
impl<N: Native> Native for Supervised<N> {
    fn can_enter(&self) -> bool {
        self.native.can_enter()
            && self
                .health_file
                .as_ref()
                .is_none_or(|path| crate::health::permits(path).unwrap_or(false))
    }
    fn headroom(&self, c: &Contract, reserve: u64) -> Result<Headroom> {
        let mut h = self.native.headroom(c, reserve)?;
        h.paused |= !self.can_enter();
        Ok(h)
    }
    fn identify(&self, e: &Entry, pid: i32) -> Result<Identity> {
        self.native.identify(e, pid)
    }
    fn terminated(&self, e: &Entry) -> Option<bool> {
        self.native.terminated(e)
    }
    fn stop(&self, e: &Entry) -> Result<()> {
        self.native.stop(e)
    }
}

pub fn cgroup_directory(path: &str) -> Result<PathBuf> {
    ensure!(
        path.starts_with('/') && path.len() <= 4096 && path != "/",
        "invalid workload cgroup"
    );
    let relative = Path::new(path.trim_start_matches('/'));
    ensure!(
        relative
            .components()
            .all(|p| matches!(p, Component::Normal(_))),
        "invalid workload cgroup"
    );
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

/// `systemctl show` renders USec properties as timespans (e.g. `5s 500ms`).
/// Parse exact integral microseconds, rejecting infinity, overflow and negatives.
pub fn duration_us(text: &str) -> Option<u64> {
    if text == "0" {
        return Some(0);
    }
    let mut total = 0u64;
    let mut present = false;
    for part in text.split_whitespace() {
        let (digits, scale) = [
            ("min", 60_000_000u64),
            ("ms", 1000),
            ("us", 1),
            ("µs", 1),
            ("s", 1_000_000),
            ("h", 3_600_000_000),
            ("d", 86_400_000_000),
        ]
        .into_iter()
        .find_map(|(suffix, scale)| part.strip_suffix(suffix).map(|n| (n, scale)))?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        total = total.checked_add(digits.parse::<u64>().ok()?.checked_mul(scale)?)?;
        present = true;
    }
    present.then_some(total)
}

impl Systemd {
    fn show(&self, unit: &str) -> Result<BTreeMap<String, String>> {
        let text = capture(Command::new(&self.systemctl).args([
            "--user", "show", "--no-pager",
            "--property=LoadState,ActiveState,ControlGroup,InvocationID,MainPID,Slice,Restart,KillMode,OOMPolicy,RuntimeMaxUSec,RuntimeRandomizedExtraUSec,TimeoutStopUSec,SendSIGKILL,FinalKillSignal",
            "--", unit,
        ]))?;
        Ok(text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.into(), v.into()))
            .collect())
    }
}

fn field<'a>(map: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    map.get(key).map_or("", String::as_str)
}

fn limit(directory: &Path, file: &str) -> Result<u64> {
    Ok(fs::read_to_string(directory.join(file))?.trim().parse()?)
}

impl Native for Systemd {
    fn headroom(&self, contract: &Contract, reserve: u64) -> Result<Headroom> {
        let mut paused = false;
        if let Some(marker) = &contract.pause_file {
            match fs::metadata(marker) {
                Ok(_) => paused = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        let unit = self.show(&contract.slice)?;
        ensure!(
            field(&unit, "LoadState") == "loaded" && field(&unit, "ActiveState") == "active",
            "workload slice unavailable"
        );
        let directory = cgroup_directory(field(&unit, "ControlGroup"))?;
        // Production admission requires an actual finite aggregate boundary.
        let memory_max = limit(&directory, "memory.max")?;
        let memory_swap_max = limit(&directory, "memory.swap.max")?;
        let stats = CgroupV2Provider::for_dir(directory)?.stats()?;
        let mut domains = stats.domains().iter();
        let host = domains
            .next()
            .ok_or_else(|| anyhow::anyhow!("host memory unavailable"))?;
        let slice = domains
            .next()
            .ok_or_else(|| anyhow::anyhow!("finite slice capacity unavailable"))?;
        Ok(Headroom {
            shared_bytes: domains.fold(host.available_bytes().saturating_sub(reserve), |a, d| {
                a.min(d.available_bytes())
            }),
            slice_bytes: slice.available_bytes(),
            memory_max,
            memory_swap_max,
            paused,
        })
    }

    fn identify(&self, entry: &Entry, peer_pid: i32) -> Result<Identity> {
        ensure!(peer_pid > 0, "invalid peer PID");
        let unit = self.show(&entry.unit())?;
        ensure!(field(&unit, "LoadState") == "loaded", "workload not loaded");
        ensure!(
            field(&unit, "MainPID") == peer_pid.to_string(),
            "only the native workload entry process can enter"
        );
        ensure!(
            field(&unit, "Slice") == entry.contract.slice,
            "wrong workload slice"
        );
        ensure!(
            field(&unit, "Restart") == "no"
                && field(&unit, "KillMode") == "control-group"
                && field(&unit, "OOMPolicy") == "kill",
            "wrong workload lifecycle"
        );
        if let Some(seconds) = entry.contract.runtime_max_sec {
            ensure!(
                duration_us(field(&unit, "RuntimeMaxUSec"))
                    .is_some_and(|runtime| runtime > 0 && runtime <= seconds * 1_000_000)
                    && duration_us(field(&unit, "RuntimeRandomizedExtraUSec")) == Some(0),
                "native runtime deadline mismatch"
            );
        }
        if entry.contract.burst {
            ensure!(
                duration_us(field(&unit, "TimeoutStopUSec"))
                    .is_some_and(|timeout| timeout > 0 && timeout <= 1_000_000)
                    && field(&unit, "SendSIGKILL") == "yes"
                    && field(&unit, "FinalKillSignal") == "9",
                "burst cleanup deadline mismatch"
            );
        }
        let cgroup = field(&unit, "ControlGroup");
        let process = fs::read_to_string(format!("/proc/{peer_pid}/cgroup"))?;
        ensure!(
            process
                .lines()
                .any(|line| line.strip_prefix("0::") == Some(cgroup)),
            "peer is outside workload cgroup"
        );
        let directory = cgroup_directory(cgroup)?;
        ensure!(
            limit(&directory, "memory.max")? == entry.contract.memory_max,
            "hard memory ceiling mismatch"
        );
        ensure!(
            limit(&directory, "memory.swap.max")? == entry.contract.memory_swap_max,
            "swap ceiling mismatch"
        );
        let invocation = field(&unit, "InvocationID");
        ensure!(
            invocation.len() == 32 && invocation.bytes().all(|b| b.is_ascii_hexdigit()),
            "workload invocation unavailable"
        );
        Ok(Identity {
            invocation: invocation.into(),
            cgroup: cgroup.into(),
            inode: fs::metadata(directory)?.ino(),
        })
    }

    fn terminated(&self, entry: &Entry) -> Option<bool> {
        let identity = entry.identity.as_ref()?;
        let unit = self.show(&entry.unit()).ok()?;
        let directory = cgroup_directory(&identity.cgroup).ok()?;
        let empty = match fs::metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Ok(metadata) if metadata.ino() == identity.inode => {
                let text = fs::read_to_string(directory.join("cgroup.events")).ok()?;
                if text.lines().any(|l| l == "populated 0") {
                    true
                } else if text.lines().any(|l| l == "populated 1") {
                    false
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        if !empty {
            return (field(&unit, "InvocationID") == identity.invocation).then_some(false);
        }
        match (field(&unit, "LoadState"), field(&unit, "ActiveState")) {
            ("not-found", _) => Some(true),
            ("loaded", "inactive" | "failed") => {
                let invocation = field(&unit, "InvocationID");
                (invocation.is_empty() || invocation == identity.invocation).then_some(true)
            }
            _ => None,
        }
    }

    fn stop(&self, entry: &Entry) -> Result<()> {
        let unit = self.show(&entry.unit())?;
        let identity = entry
            .identity
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workload has not entered"))?;
        ensure!(
            field(&unit, "InvocationID") == identity.invocation
                && field(&unit, "ControlGroup") == identity.cgroup,
            "workload identity changed"
        );
        capture(Command::new(&self.systemctl).args([
            "--user",
            "stop",
            "--no-block",
            "--",
            &entry.unit(),
        ]))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_timespans_are_exact_and_unknown_deadlines_fail_closed() {
        for (text, expected) in [
            ("0", 0),
            ("5s", 5_000_000),
            ("1s 500ms", 1_500_000),
            ("30s 1us", 30_000_001),
            ("1min 2s", 62_000_000),
            ("1µs", 1),
        ] {
            assert_eq!(duration_us(text), Some(expected), "{text}");
        }
        for text in [
            "",
            "infinity",
            "5000000",
            "-1s",
            "+1s",
            "NaNs",
            "1.5s",
            "1s unknown",
            "18446744073709551615s",
            "18446744073709551615us 1us",
        ] {
            assert_eq!(duration_us(text), None, "{text}");
        }
    }
}
