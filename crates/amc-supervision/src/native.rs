//! Native identity and pidfd actuation. Signals target the pinned main process;
//! systemd owns descendant cleanup. No unit-name stop/restart race and no raw
//! cgroup mutation. A replacement process can never inherit an old pidfd.
use crate::{
    policy::{Domain, Lifecycle, Mode, Policy},
    recovery::Identity,
};
use amc_admission::{
    ledger::{Ledger, Phase},
    native::cgroup_directory,
};
use amc_runner::systemd::capture;
use amc_telemetry::{PinnedReader, Snapshot};
use anyhow::{Result, ensure};
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Read,
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

pub fn bounded_json<T: serde::de::DeserializeOwned>(path: &Path, owner: Option<u32>) -> Result<T> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.mode() & 0o022 == 0
            && owner.is_none_or(|uid| metadata.uid() == uid),
        "invalid JSON source owner/type/mode"
    );
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "JSON source exceeds bound");
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Clone)]
pub struct Manager {
    pub systemctl: PathBuf,
}
impl Manager {
    fn command(&self, domain: &Domain) -> Result<Command> {
        let mut command = Command::new(&self.systemctl);
        command.arg("--no-ask-password");
        if let Some(uid) = domain.uid {
            let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))?
                .ok_or_else(|| anyhow::anyhow!("supervised account missing"))?;
            command.args(["--user", &format!("--machine={}@.host", user.name)]);
        }
        Ok(command)
    }

    pub fn show(&self, domain: &Domain) -> Result<BTreeMap<String, String>> {
        let text = capture(self.command(domain)?.args(["show", "--no-pager",
            "--property=LoadState,ActiveState,ControlGroup,InvocationID,MainPID,Restart,KillMode,OOMPolicy,TimeoutStopUSec,MemoryMax,MemorySwapMax,Result", "--", &domain.unit]))?;
        Ok(text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.into(), v.into()))
            .collect())
    }

    /// Only issued after original termination, cooldown, current healthy
    /// headroom, durable accounting, and fresh native inactive verification.
    pub fn start(&self, domain: &Domain, may_start: impl Fn() -> bool) -> Result<()> {
        ensure!(
            domain.lifecycle == Lifecycle::Restart,
            "only backends may start"
        );
        let shown = self.show(domain)?;
        ensure!(
            matches!(field(&shown, "ActiveState"), "inactive" | "failed"),
            "replacement is already active; refusing start"
        );
        ensure!(
            field(&shown, "Restart") == "no"
                && field(&shown, "KillMode") == "control-group"
                && field(&shown, "OOMPolicy") == "kill",
            "backend lifecycle changed; refusing start"
        );
        ensure!(
            limits_match(&shown, domain),
            "backend hard ceilings changed; refusing start"
        );
        ensure!(
            may_start(),
            "restart headroom or recovery authority changed"
        );
        capture(
            self.command(domain)?
                .args(["start", "--no-block", "--", &domain.unit]),
        )?;
        Ok(())
    }

    pub fn attach(&self, domain: Domain, policy: &Policy) -> Result<Session> {
        let shown = self.show(&domain)?;
        ensure!(
            field(&shown, "LoadState") == "loaded" && field(&shown, "ActiveState") == "active",
            "domain not active"
        );
        if domain.lifecycle != Lifecycle::Observe && policy.mode == Mode::Enforce {
            ensure!(
                field(&shown, "Restart") == "no"
                    && field(&shown, "KillMode") == "control-group"
                    && field(&shown, "OOMPolicy") == "kill",
                "native lifecycle lacks exclusive supervision authority"
            );
            // Manager displays time spans, not necessarily integer microseconds.
            ensure!(
                cleanup_bounded(&shown, policy),
                "native descendant cleanup exceeds recovery deadline"
            );
        }
        let pid: i32 = field(&shown, "MainPID").parse()?;
        let start_ticks = process_start(pid)?;
        let fd = pidfd_open(
            Pid::from_raw(pid).ok_or_else(|| anyhow::anyhow!("invalid native PID"))?,
            PidfdFlags::empty(),
        )?;
        let cgroup = field(&shown, "ControlGroup").to_owned();
        let directory = cgroup_directory(&cgroup)?;
        let reader = PinnedReader::open(&directory)?;
        let invocation = field(&shown, "InvocationID").to_owned();
        ensure!(
            invocation.len() == 32 && invocation.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid invocation"
        );
        let identity = Identity {
            invocation,
            cgroup,
            inode: reader.inode()?,
            pid,
            start_ticks,
        };
        if let Some(expected) = &domain.expected {
            ensure!(
                expected.invocation == identity.invocation
                    && expected.cgroup == identity.cgroup
                    && expected.inode == identity.inode,
                "admission identity differs from native invocation"
            );
        }
        let mut ancestors = Vec::new();
        let mut parent = directory.parent();
        while let Some(path) = parent {
            if !path.starts_with("/sys/fs/cgroup") {
                break;
            }
            if path.join("memory.max").exists() {
                ancestors.push((path.to_owned(), PinnedReader::open(path)?));
            }
            parent = path.parent();
        }
        let session = Session {
            domain,
            identity,
            reader,
            ancestors,
            pidfd: Arc::new(fd),
        };
        session.verify_process()?;
        // A second manager query binds the pidfd and cgroup handles to exactly
        // the invocation sampled before attach, including same-directory reuse.
        session.verify_manager(self, policy.mode == Mode::Enforce)?;
        Ok(session)
    }
}

pub fn field<'a>(map: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    map.get(key).map_or("", String::as_str)
}

fn duration_ms(text: &str) -> Option<u64> {
    let mut sum = 0.0;
    for part in text.split_whitespace() {
        let (digits, scale) = [
            ("min", 60_000.0),
            ("ms", 1.0),
            ("us", 0.001),
            ("s", 1000.0),
            ("h", 3_600_000.0),
        ]
        .into_iter()
        .find_map(|(unit, scale)| part.strip_suffix(unit).map(|n| (n, scale)))?;
        sum += digits.parse::<f64>().ok()? * scale;
    }
    (sum.is_finite() && sum > 0.0 && sum <= u64::MAX as f64).then_some(sum.ceil() as u64)
}

pub fn process_start(pid: i32) -> Result<u64> {
    ensure!(pid > 0, "invalid process identity");
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .ok_or_else(|| anyhow::anyhow!("missing process identity"))?
        .1
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow::anyhow!("missing process start time"))?
        .parse()?)
}

pub fn cleanup_bounded(shown: &BTreeMap<String, String>, policy: &Policy) -> bool {
    duration_ms(field(shown, "TimeoutStopUSec")).is_some_and(|ms| ms <= policy.kill_ms)
}

pub fn limits_match(shown: &BTreeMap<String, String>, domain: &Domain) -> bool {
    domain.memory_max > 0
        && field(shown, "MemoryMax").parse::<u64>().ok() == Some(domain.memory_max)
        && field(shown, "MemorySwapMax").parse::<u64>().ok() == Some(domain.memory_swap_max)
}

fn original_gone(identity: &Identity) -> Option<bool> {
    match process_start(identity.pid) {
        Ok(start) => Some(start != identity.start_ticks),
        Err(_) => match fs::metadata(format!("/proc/{}", identity.pid)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
            _ => None,
        },
    }
}

pub struct Session {
    pub domain: Domain,
    pub identity: Identity,
    pub reader: PinnedReader,
    ancestors: Vec<(PathBuf, PinnedReader)>,
    pidfd: Arc<OwnedFd>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    pub key: String,
    pub cgroup: String,
    pub inode: u64,
    pub current_bytes: u64,
    pub limit_bytes: u64,
}

impl Boundary {
    pub fn normalized(&self) -> Result<f64> {
        ensure!(
            self.limit_bytes > 0 && self.key.len() <= 64 && self.cgroup.len() <= 4096,
            "invalid trajectory boundary"
        );
        let fraction = if self.key.starts_with("host.") {
            1.0
        } else {
            0.9
        };
        Ok(self.current_bytes as f64 / (self.limit_bytes as f64 * fraction))
    }
}

/// Include capacities and column identities: policy/ancestry changes cannot
/// silently repurpose a calibrated trajectory column of the same vector length.
pub fn forecast_input(identity: &Identity, boundaries: &[Boundary]) -> Result<(String, Vec<f64>)> {
    let mut signature = format!(
        "{}:{}:{}",
        identity.invocation, identity.inode, identity.start_ticks
    );
    let mut values = Vec::new();
    for boundary in boundaries {
        signature.push_str(&format!(
            ":{}:{}:{}",
            boundary.key, boundary.inode, boundary.limit_bytes
        ));
        values.push(boundary.normalized()?);
    }
    Ok((signature, values))
}

#[derive(Clone)]
pub struct Target {
    pub domain: Domain,
    pub identity: Identity,
    pidfd: Arc<OwnedFd>,
}

impl Target {
    pub fn verify_process(&self) -> Result<()> {
        verify_process(&self.domain, &self.identity)
    }
    pub fn signal(&self, signal: Signal) -> Result<()> {
        ensure!(
            self.domain.lifecycle != Lifecycle::Observe,
            "no cancellation authority"
        );
        self.verify_process()?;
        pidfd_send_signal(&*self.pidfd, signal)?;
        Ok(())
    }
}

fn verify_process(domain: &Domain, identity: &Identity) -> Result<()> {
    ensure!(
        process_start(identity.pid)? == identity.start_ticks,
        "process identity replaced"
    );
    if let Some(uid) = domain.uid {
        ensure!(
            fs::metadata(format!("/proc/{}", identity.pid))?.uid() == uid,
            "native process belongs to wrong account"
        );
    }
    let process = fs::read_to_string(format!("/proc/{}/cgroup", identity.pid))?;
    ensure!(
        process
            .lines()
            .any(|l| l.strip_prefix("0::") == Some(&identity.cgroup)),
        "native process moved cgroup"
    );
    ensure!(
        fs::metadata(cgroup_directory(&identity.cgroup)?)?.ino() == identity.inode,
        "cgroup replaced"
    );
    Ok(())
}

impl Session {
    pub fn target(&self) -> Target {
        Target {
            domain: self.domain.clone(),
            identity: self.identity.clone(),
            pidfd: self.pidfd.clone(),
        }
    }
    pub fn verify_process(&self) -> Result<()> {
        verify_process(&self.domain, &self.identity)
    }

    pub fn verify_manager(&self, manager: &Manager, enforce: bool) -> Result<()> {
        let shown = manager.show(&self.domain)?;
        ensure!(
            field(&shown, "InvocationID") == self.identity.invocation
                && field(&shown, "ControlGroup") == self.identity.cgroup
                && field(&shown, "MainPID") == self.identity.pid.to_string(),
            "native invocation replaced"
        );
        if self.domain.lifecycle != Lifecycle::Observe && enforce {
            ensure!(
                field(&shown, "Restart") == "no"
                    && field(&shown, "KillMode") == "control-group"
                    && field(&shown, "OOMPolicy") == "kill",
                "native lifecycle changed"
            );
        }
        Ok(())
    }

    /// Only the main PID is signalled. systemd handles the rest through its
    /// existing KillMode; stop/start by fixed name is never used for stopping.
    pub fn signal(&self, signal: Signal) -> Result<()> {
        self.target().signal(signal)
    }

    pub fn observations(&mut self, ms: u64) -> Result<(Snapshot, Result<Vec<Boundary>>)> {
        self.verify_process()?;
        let mut leaf = self.reader.snapshot(&self.identity.cgroup, None, Some(ms));
        leaf.invocation_id = Some(self.identity.invocation.clone());
        leaf.inode = Some(self.identity.inode);
        if self.domain.memory_max > 0 {
            ensure!(
                number(&leaf, "memory.max")? == self.domain.memory_max
                    && number(&leaf, "memory.swap.max")? == self.domain.memory_swap_max,
                "hard ceiling differs from enrolled policy"
            );
        }
        let mut values = Vec::new();
        for (key, max) in [
            ("memory.current", "memory.max"),
            ("memory.swap.current", "memory.swap.max"),
        ] {
            let limit = leaf
                .files
                .get(max)
                .and_then(|m| m.value.as_ref())
                .ok_or_else(|| anyhow::anyhow!("unknown leaf capacity"))?;
            let current = number(&leaf, key)?;
            if let Some(limit) = limit.as_u64() {
                if limit > 0 {
                    values.push(Boundary {
                        key: key.into(),
                        cgroup: self.identity.cgroup.clone(),
                        inode: self.identity.inode,
                        current_bytes: current,
                        limit_bytes: limit,
                    });
                } else {
                    ensure!(current == 0, "zero-swap policy exceeded");
                }
            } else {
                ensure!(
                    self.domain.lifecycle == Lifecycle::Observe && limit.as_str() == Some("max"),
                    "unbounded enrolled leaf"
                );
            }
        }
        let boundaries = (|| -> Result<Vec<Boundary>> {
            for (path, reader) in &mut self.ancestors {
                ensure!(
                    fs::metadata(&*path)?.ino() == reader.inode()?,
                    "ancestor replaced"
                );
                let ancestor = reader.snapshot(path.to_str().unwrap_or(""), None, Some(ms));
                for (key, max) in [
                    ("memory.current", "memory.max"),
                    ("memory.swap.current", "memory.swap.max"),
                ] {
                    let limit = ancestor
                        .files
                        .get(max)
                        .and_then(|m| m.value.as_ref())
                        .ok_or_else(|| anyhow::anyhow!("unknown ancestor ceiling"))?;
                    if let Some(limit) = limit.as_u64() {
                        let current = number(&ancestor, key)?;
                        if limit > 0 {
                            values.push(Boundary {
                                key: key.into(),
                                cgroup: path.to_string_lossy().into(),
                                inode: reader.inode()?,
                                current_bytes: current,
                                limit_bytes: limit,
                            });
                        } else {
                            ensure!(current == 0, "ancestor zero ceiling exceeded");
                        }
                    } else {
                        ensure!(limit.as_str() == Some("max"), "unknown ancestor capacity");
                    }
                }
            }
            ensure!(values.len() <= 30, "too many capacity boundaries");
            Ok(values)
        })();
        Ok((leaf, boundaries))
    }

    pub fn empty(&mut self) -> Option<bool> {
        // systemd may remove an empty cgroup before the next sample. Absence
        // needs the original process to be gone, too; replacement/permission
        // errors remain unknown. The server also requires native inactivity.
        if !original_gone(&self.identity)? {
            return Some(false);
        }
        if let Err(error) = fs::metadata(cgroup_directory(&self.identity.cgroup).ok()?) {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Some(true);
            }
            return None;
        }
        if fs::metadata(cgroup_directory(&self.identity.cgroup).ok()?)
            .ok()?
            .ino()
            != self.identity.inode
        {
            return None;
        }
        let snapshot = self.reader.snapshot(&self.identity.cgroup, None, None);
        snapshot
            .files
            .get("cgroup.events")?
            .value
            .as_ref()?
            .get("populated")?
            .as_u64()
            .map(|v| v == 0)
    }
}

pub fn number(snapshot: &Snapshot, file: &str) -> Result<u64> {
    snapshot
        .files
        .get(file)
        .and_then(|m| m.value.as_ref())
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("unknown native measurement: {file}"))
}

pub fn require_root() -> Result<()> {
    ensure!(
        nix::unistd::geteuid().is_root(),
        "host supervision requires root"
    );
    Ok(())
}

pub fn terminated(manager: &Manager, domain: &Domain, identity: &Identity) -> Result<bool> {
    let shown = manager.show(domain)?;
    ensure!(
        matches!(field(&shown, "ActiveState"), "inactive" | "failed")
            && field(&shown, "MainPID") == "0",
        "native unit is active or ambiguous"
    );
    let gone = original_gone(identity)
        .ok_or_else(|| anyhow::anyhow!("original process termination is unknown"))?;
    if !gone {
        return Ok(false);
    }
    let directory = cgroup_directory(&identity.cgroup)?;
    match fs::metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(metadata) if metadata.ino() == identity.inode => {
            let mut reader = PinnedReader::open(&directory)?;
            let snapshot = reader.snapshot(&identity.cgroup, None, None);
            Ok(snapshot
                .files
                .get("cgroup.events")
                .and_then(|m| m.value.as_ref())
                .and_then(|v| v.get("populated"))
                .and_then(|v| v.as_u64())
                == Some(0))
        }
        _ => anyhow::bail!("original cgroup is replaced or unreadable"),
    }
}

/// Ledger entries supply immutable, full-ceiling native contracts. Reading a
/// ledger does not borrow a grant for a sibling domain or release accounting.
pub fn job_domains(policy: &Policy, boot: &str) -> Result<Vec<Domain>> {
    let mut domains = Vec::new();
    for pool in &policy.job_pools {
        let ledger: Ledger = match bounded_json(&pool.state.join("ledger.json"), Some(pool.uid)) {
            Ok(ledger) => ledger,
            Err(_) if !pool.state.exists() => continue,
            Err(error) => return Err(error),
        };
        ledger.validate()?;
        ensure!(
            ledger.boot_id == boot,
            "job pool ledger belongs to previous boot"
        );
        for entry in ledger
            .entries
            .iter()
            .filter(|e| e.phase == Phase::Running && pool.contracts.contains(&e.name))
        {
            let domain = Domain {
                id: format!("{}-{}", pool.id, entry.id),
                uid: Some(pool.uid),
                unit: entry.unit(),
                lifecycle: pool.lifecycle,
                memory_max: entry.contract.memory_max,
                memory_swap_max: entry.contract.memory_swap_max,
                priority: pool.priority,
                expected: entry.identity.clone(),
            };
            domain.validate()?;
            domains.push(domain);
        }
    }
    ensure!(
        domains.len() <= 64,
        "active supervision domain bound exceeded"
    );
    Ok(domains)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn restart_requires_current_authorization_and_matching_native_ceilings() {
        let root = std::env::temp_dir().join(format!(
            "amc-start-{}",
            amc_admission::store::fresh_id().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let executable = root.join("systemctl");
        let marker = root.join("started");
        let script = |limit| {
            format!(
                "#!/bin/sh\nif [ \"$2\" = show ]; then\ncat <<'EOF'\nActiveState=inactive\nRestart=no\nKillMode=control-group\nOOMPolicy=kill\nMemoryMax={limit}\nMemorySwapMax=0\nEOF\nelse\n: > '{}'\nfi\n",
                marker.display()
            )
        };
        fs::write(&executable, script(1024)).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = Manager {
            systemctl: executable.clone(),
        };
        let domain = Domain {
            id: "fixture".into(),
            uid: None,
            unit: "fixture.service".into(),
            lifecycle: Lifecycle::Restart,
            memory_max: 1024,
            memory_swap_max: 0,
            priority: 0,
            expected: None,
        };
        assert!(manager.start(&domain, || false).is_err());
        assert!(!marker.exists());
        fs::write(&executable, script(2048)).unwrap();
        assert!(manager.start(&domain, || true).is_err());
        assert!(!marker.exists());
        fs::write(&executable, script(1024)).unwrap();
        manager.start(&domain, || true).unwrap();
        assert!(marker.exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_stale_pidfd_or_changed_identity_cannot_signal_another_process() {
        let mut original = Command::new("sleep").arg("60").spawn().unwrap();
        let mut other = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = original.id() as i32;
        let fd = pidfd_open(Pid::from_raw(pid).unwrap(), PidfdFlags::empty()).unwrap();
        let start = process_start(pid).unwrap();
        let mut target = Target {
            domain: Domain {
                id: "fixture".into(),
                uid: Some(nix::unistd::geteuid().as_raw()),
                unit: "fixture.service".into(),
                lifecycle: Lifecycle::Terminate,
                memory_max: 1024,
                memory_swap_max: 0,
                priority: 0,
                expected: None,
            },
            identity: Identity {
                invocation: "a".repeat(32),
                // Identity rejection precedes cgroup access. A Nix sandbox
                // need not mount the host cgroup hierarchy to prove it.
                cgroup: "/amc-pidfd-identity-fixture".into(),
                inode: 1,
                pid,
                start_ticks: start,
            },
            pidfd: Arc::new(fd),
        };
        target.identity.start_ticks += 1;
        let error = target.signal(Signal::KILL).unwrap_err();
        assert!(error.to_string().contains("process identity replaced"));
        assert!(original.try_wait().unwrap().is_none());
        assert_eq!(
            original_gone(&Identity {
                start_ticks: start,
                ..target.identity.clone()
            }),
            Some(false)
        );
        original.kill().unwrap();
        original.wait().unwrap();
        target.identity.start_ticks = start;
        assert_eq!(original_gone(&target.identity), Some(true));
        assert!(target.signal(Signal::KILL).is_err());
        assert!(pidfd_send_signal(&*target.pidfd, Signal::KILL).is_err());
        assert!(other.try_wait().unwrap().is_none());
        other.kill().unwrap();
        other.wait().unwrap();
    }
    #[test]
    fn capacity_changes_cannot_repurpose_a_calibrated_column() {
        let i = Identity {
            invocation: "a".repeat(32),
            cgroup: "/fixture".into(),
            inode: 1,
            pid: 1,
            start_ticks: 1,
        };
        let mut b = Boundary {
            key: "memory.current".into(),
            cgroup: "/fixture".into(),
            inode: 1,
            current_bytes: 50,
            limit_bytes: 100,
        };
        let before = forecast_input(&i, &[b.clone()]).unwrap().0;
        b.limit_bytes = 200;
        assert_ne!(before, forecast_input(&i, &[b]).unwrap().0);
    }
}
