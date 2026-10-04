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
    fn headroom(&self, contract: &Contract, reserve: u64) -> Result<Headroom>;
    fn identify(&self, entry: &Entry, peer_pid: i32) -> Result<Identity>;
    fn terminated(&self, entry: &Entry) -> Option<bool>;
    fn stop(&self, entry: &Entry) -> Result<()>;
}

pub struct Systemd {
    pub systemctl: PathBuf,
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

impl Systemd {
    fn show(&self, unit: &str) -> Result<BTreeMap<String, String>> {
        let text = capture(Command::new(&self.systemctl).args([
            "--user", "show", "--no-pager",
            "--property=LoadState,ActiveState,ControlGroup,InvocationID,MainPID,Slice,Restart,KillMode,OOMPolicy",
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
