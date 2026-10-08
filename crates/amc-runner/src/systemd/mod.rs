//! Linux systemd client execution. No signal handlers or terminal reporting.
//!
//! The low-level [`execute`] runs a caller-constructed systemd-run command and
//! verifies it targets the reserved identity in the matching wait mode before
//! spawning. Native resource properties remain the caller's responsibility:
//! this backend does not certify them and does not provide a sandbox.
//! Detached acknowledgment is not workload termination.

mod control;
mod managed;
mod startup;
pub use control::{QUERY_TIMEOUT, capture, capture_with_timeout};
pub use managed::{LaunchRequest, RunError, Runner};
pub use startup::{StartupBarrier, wait_for_startup};

use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
        process::ExitStatusExt,
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Evidence obtained while observing or stopping an owned unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// A passive reconciliation observed a running workload; capacity stays held.
    Running,
    /// A successful stop followed by a loaded, inactive/failed unit.
    Stopped,
    /// Absence, collection, or manager failure left termination unconfirmed.
    Unknown,
}

/// Why no systemd client was submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotSubmitted {
    /// The supplied identity was not a safe AMC service name.
    InvalidUnit,
    /// The prepared command does not target the reserved identity in the
    /// matching wait mode, or requests a remote/scope context.
    CommandMismatch,
    /// The client could not be spawned.
    SpawnFailed,
    /// The application cancelled before spawning the client.
    Cancelled(i32),
}

/// Execution outcome, independently of CLI formatting or exit-code mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No submission was attempted.
    NotSubmitted(NotSubmitted),
    /// The detached client acknowledged submission; the workload can still run.
    Acknowledged,
    /// The waited workload completed, or failure was confirmed by the manager.
    Completed(i32),
    /// Application cancellation after a client was spawned.
    Cancelled {
        /// Application-supplied cancellation signal.
        signal: i32,
        /// Whether a workload start was observed.
        started: bool,
        /// Cleanup evidence.
        cleanup: Cleanup,
    },
    /// The startup deadline expired, not a workload runtime deadline.
    TimedOut {
        /// Whether a workload start was observed.
        started: bool,
        /// Cleanup evidence.
        cleanup: Cleanup,
    },
    /// The client failed without a confirmed workload result; never replay.
    Unknown {
        /// Client exit code, or 1 when waiting failed.
        exit_code: i32,
        /// Cleanup evidence, which may still establish termination.
        cleanup: Cleanup,
    },
}

impl Outcome {
    /// Whether releasing a workload reservation is justified by this outcome.
    pub fn releases_reservation(self) -> bool {
        matches!(self, Self::NotSubmitted(_) | Self::Completed(_))
            || matches!(
                self,
                Self::Cancelled {
                    cleanup: Cleanup::Stopped,
                    ..
                } | Self::TimedOut {
                    cleanup: Cleanup::Stopped,
                    ..
                } | Self::Unknown {
                    cleanup: Cleanup::Stopped,
                    ..
                }
            )
    }
}

/// Whether `name` is a safe environment variable name for `--setenv=`.
pub fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_unit(unit: &str) -> bool {
    unit.starts_with("app-amc-")
        && unit.ends_with(".service")
        && unit.len() <= 255
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
}

fn startup_wait_required(detached: bool, started: bool) -> bool {
    !started || detached
}

/// Manager-observed unit state. `None` (from [`state`]) means the unit is
/// absent, collected, or unobservable — never evidence of termination.
pub(crate) struct UnitState {
    loaded: bool,
    invocation: Option<String>,
    started: bool,
    stopped: bool,
    control_group: Option<String>,
    main_pid: Option<u32>,
    /// Workload exit status from `ExecMainCode`/`ExecMainStatus`, when the
    /// manager reports one. Distinct from the submission client's exit code.
    workload_code: Option<i32>,
}

fn state(manager: &Path, unit: &str) -> Option<UnitState> {
    query_state(manager, unit).filter(|state| state.loaded)
}

fn query_state(manager: &Path, unit: &str) -> Option<UnitState> {
    let text = capture(Command::new(manager).args([
        "--user",
        "show",
        "--no-pager",
        "--property=LoadState,ActiveState,InvocationID,MainPID,ExecMainCode,ExecMainStatus,ExecMainStartTimestampMonotonic,ControlGroup",
        "--",
        unit,
    ]))
    .ok()?;
    let field = |name: &str| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(key, value)| (key == name).then_some(value))
    };
    let started = field("ExecMainStartTimestampMonotonic")
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > 0);
    let loaded = match field("LoadState") {
        Some("loaded") => true,
        Some("not-found") => false,
        _ => return None,
    };
    let stopped = match field("ActiveState") {
        Some("inactive" | "failed") => true,
        Some("active" | "activating" | "deactivating" | "reloading") => false,
        _ => return None,
    };
    // `systemctl show` reports numeric CLD_* codes: 1 = exited, 2 = killed,
    // 3 = dumped. Older managers may report the textual names; accept both.
    // Anything else (including out-of-range statuses) is unknown, never the
    // client exit code by default.
    let workload_code = match field("ExecMainCode") {
        Some("1" | "exited") => field("ExecMainStatus").and_then(|status| {
            status
                .parse::<i32>()
                .ok()
                .filter(|code| (0..=255).contains(code))
        }),
        Some("2" | "3" | "killed" | "dumped") => field("ExecMainStatus").and_then(|status| {
            status
                .parse::<i32>()
                .ok()
                .filter(|signal| (1..=64).contains(signal))
                .map(|signal| 128 + signal)
        }),
        _ => None,
    };
    Some(UnitState {
        loaded,
        invocation: field("InvocationID")
            .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned),
        started,
        stopped,
        control_group: field("ControlGroup").map(str::to_owned),
        main_pid: field("MainPID")
            .and_then(|pid| pid.parse().ok())
            .filter(|pid| *pid > 0),
        workload_code,
    })
}

/// Pin both directories while a native workload is observable. An absent leaf
/// proves collection only while its original parent remains visible. Reads via
/// the held directory descriptor cannot accidentally inspect a replacement.
struct ObservedDomain {
    path: PathBuf,
    directory: File,
    parent: File,
}

impl ObservedDomain {
    fn capture(path: &Path) -> Option<Self> {
        let open = |path| {
            OpenOptions::new()
                .read(true)
                // Linux O_DIRECTORY | O_NOFOLLOW (same ABI on supported targets).
                .custom_flags(0x10000 | 0x20000)
                .open(path)
                .ok()
        };
        let parent = open(path.parent()?)?;
        let directory = open(
            &PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(path.file_name()?),
        )?;
        let result = Self {
            path: path.into(),
            directory,
            parent,
        };
        (result.parent_visible()
            && fs::symlink_metadata(path)
                .is_ok_and(|metadata| Self::same(&result.directory, &metadata)))
        .then_some(result)
    }

    fn same(file: &File, metadata: &fs::Metadata) -> bool {
        file.metadata().is_ok_and(|held| {
            metadata.is_dir() && held.dev() == metadata.dev() && held.ino() == metadata.ino()
        })
    }

    fn parent_visible(&self) -> bool {
        self.path
            .parent()
            .and_then(|parent| fs::symlink_metadata(parent).ok())
            .is_some_and(|metadata| Self::same(&self.parent, &metadata))
    }

    fn empty(&self) -> Option<bool> {
        if !self.parent_visible() {
            return None;
        }
        let empty = match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Some(true),
            Ok(metadata) if Self::same(&self.directory, &metadata) => domain_empty(&PathBuf::from(
                format!("/proc/self/fd/{}", self.directory.as_raw_fd()),
            )),
            _ => None,
        };
        self.parent_visible().then_some(empty).flatten()
    }

    fn matches(&self, group: Option<&str>) -> bool {
        group.is_some_and(|group| {
            group.is_empty() || safe_cgroup_path(group).as_ref() == Some(&self.path)
        })
    }
}

fn confirmed_exit(state: &UnitState, observed: Option<&(String, ObservedDomain)>) -> bool {
    if !state.stopped {
        return false;
    }
    if let Some((invocation, domain)) = observed {
        (!state.loaded
            || (state.started
                && state.invocation.as_ref() == Some(invocation)
                && domain.matches(state.control_group.as_deref())))
            && domain.empty() == Some(true)
    } else {
        state.loaded && state.started && terminated(state.control_group.as_deref()) == Some(true)
    }
}

/// Resolve a manager-reported cgroup path under the v2 hierarchy, mirroring
/// the CLI convention. Rejects anything that is not an absolute path of
/// normal components.
fn safe_cgroup_path(control_group: &str) -> Option<PathBuf> {
    if !control_group.starts_with('/') || control_group == "/" || control_group.len() > 4096 {
        return None;
    }
    let relative = Path::new(control_group.trim_start_matches('/'));
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(Path::new("/sys/fs/cgroup").join(relative))
}

/// Whether the workload domain (including descendants) holds no processes,
/// via the native hierarchical `cgroup.events` `populated` key. A collected
/// (absent) domain also counts as empty: no process can reside in a directory
/// that does not exist. A present domain without a readable population key
/// is `None` (unknown), never empty.
fn domain_empty(dir: &Path) -> Option<bool> {
    let content = match fs::read_to_string(dir.join("cgroup.events")) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            // A collected domain holds no processes. Any other observation
            // failure (permissions, a present domain without the population
            // key) is unknown, never empty.
            return match fs::metadata(dir) {
                Err(error) if error.kind() == ErrorKind::NotFound => Some(true),
                _ => None,
            };
        }
        Err(_) => return None,
    };
    content
        .lines()
        .filter_map(|line| line.split_once(' '))
        .find_map(|(key, value)| {
            (key == "populated").then_some(match value.trim() {
                "0" => Some(true),
                "1" => Some(false),
                _ => None,
            })
        })
        .flatten()
}

/// Terminal unit state plus an empty workload domain. Stopped services whose
/// domain still holds processes (or cannot be observed) are not terminated:
/// descendants can survive the service under permissive kill settings.
fn terminated(control_group: Option<&str>) -> Option<bool> {
    let path = safe_cgroup_path(control_group?)?;
    domain_empty(&path)
}

/// Resolve a unit's workload domain directory from its manager-reported
/// `ControlGroup`. Returns `None` when the unit is absent, collected, or
/// unobservable, or when the reported path is unsafe — never a guess.
/// Pair with `CgroupV2Provider::for_dir` to observe a worker slice (plus
/// visible ancestors and host headroom) from a coordinator running
/// elsewhere. Fails closed: unresolvable placement is an error downstream,
/// not host-only accounting.
pub fn unit_cgroup_dir(manager: &Path, unit: &str) -> Option<PathBuf> {
    let group = state(manager, unit)?.control_group?;
    safe_cgroup_path(&group)
}

/// Stop exactly the supplied AMC identity and confirm workload termination.
pub fn cleanup(manager: &Path, unit: &str) -> Cleanup {
    if !valid_unit(unit)
        || capture(Command::new(manager).args(["--user", "stop", "--", unit])).is_err()
    {
        return Cleanup::Unknown;
    }
    match state(manager, unit) {
        Some(state)
            if state.stopped && terminated(state.control_group.as_deref()) == Some(true) =>
        {
            // Reset failure is housekeeping, not evidence that the workload stopped.
            let _ = capture(Command::new(manager).args(["--user", "reset-failed", "--", unit]));
            Cleanup::Stopped
        }
        _ => Cleanup::Unknown,
    }
}

/// Reap the submission client. Returns whether reaping was confirmed: an
/// unreaped client may still submit work, so its outcome must stay blocked.
fn reap_client(child: &mut Child) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(5));
    }
    matches!(child.try_wait(), Ok(Some(_)))
}

/// Whether the submission client was spawned and whether it is confirmed
/// gone. Panic recovery uses both: a never-submitted identity is safe to
/// forget, an uncertain submission with a possibly live client must stay
/// blocked until settlement is established by other means.
#[derive(Debug, Default)]
pub struct ClientRecord {
    submitted: std::cell::Cell<bool>,
    settled: std::cell::Cell<bool>,
    startup: Option<StartupBarrier>,
}

impl ClientRecord {
    /// Require startup synchronization before a waited job can complete. This
    /// does not grant admission or release capacity. Detached callers keep the
    /// ordinary acknowledgment-only path and must not supply a barrier.
    pub fn with_startup(startup: StartupBarrier) -> Self {
        Self {
            startup: Some(startup),
            ..Self::default()
        }
    }

    /// Whether the client was spawned.
    pub fn submitted(&self) -> bool {
        self.submitted.get()
    }

    /// Whether the client is confirmed reaped or exited.
    pub fn settled(&self) -> bool {
        self.settled.get()
    }

    pub(crate) fn mark_submitted(&self) {
        self.submitted.set(true);
    }

    pub(crate) fn mark_settled(&self, settled: bool) {
        self.settled.set(settled);
    }
}

/// Whether the prepared command targets `unit` in the matching wait mode and
/// stays in the local user-manager context. A reservation must never be
/// released for a workload that runs under a different identity.
fn command_binding(command: &Command, unit: &str, detached: bool) -> bool {
    let unit_arg = format!("--unit={unit}");
    let mut targets_unit = false;
    let mut waits = false;
    let mut user = false;
    let mut args = command.get_args();
    for arg in args.by_ref() {
        if arg == "--" {
            // Everything after this separator is literal workload argv.
            return targets_unit && user && waits != detached && args.next().is_some();
        }
        let arg = arg.to_string_lossy();
        if arg.as_ref() == unit_arg {
            if targets_unit {
                return false;
            }
            targets_unit = true;
        } else if arg.starts_with("--unit") || arg.starts_with("-u") {
            return false;
        }
        user |= arg.as_ref() == "--user";
        if arg.as_ref() == "--wait" {
            waits = true;
        }
        if matches!(arg.as_ref(), "--scope" | "--system")
            || arg.starts_with("--machine")
            || arg.starts_with("--host")
            || arg.starts_with("-H")
            || arg.starts_with("-M")
        {
            return false;
        }
    }
    false
}

/// Poll application cancellation after the client exists. A panicking
/// callback must not unwind while the submission client is unowned: reap it,
/// attempt cleanup, record whether reaping was confirmed, then resume the
/// panic. An unreaped client keeps the attempt blocked, not reconcilable.
fn poll_cancelled(
    child: &mut Child,
    manager: &Path,
    unit: &str,
    cancelled: &impl Fn() -> Option<i32>,
    record: &ClientRecord,
) -> Option<i32> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cancelled)) {
        Ok(cancellation) => cancellation,
        Err(payload) => {
            record.mark_settled(reap_client(child));
            let _ = cleanup(manager, unit);
            std::panic::resume_unwind(payload);
        }
    }
}

/// Execute a prepared systemd-run command using application-owned cancellation.
///
/// The caller must supply a fresh identity. The command must target `unit`
/// (via `--unit=`), use `--wait` unless
/// `detached`, stay local (no machine/host/scope escape), and disable
/// automatic restarts. Its resource properties remain the caller's
/// responsibility. Manager queries are bounded. Cancellation is polled
/// between those queries; provider I/O is not interruptible and a final
/// capacity probe may straddle its deadline.
///
/// `record` tracks client spawn and settlement, so panic recovery can
/// distinguish a never-submitted identity (safe to forget) from an uncertain
/// submission (retained, and blocked while the client is unsettled).
/// A waited unit collected after an observed start can complete only with a
/// successful final manager query and its pinned workload domain proven empty
/// beneath the original visible parent. Without that identity, collection is
/// unknown. The waited client's status is used when final manager status is gone.
pub fn execute(
    command: &mut Command,
    manager: &Path,
    unit: &str,
    detached: bool,
    retain: bool,
    cancelled: impl Fn() -> Option<i32>,
    record: &ClientRecord,
) -> Outcome {
    if !valid_unit(unit) {
        return Outcome::NotSubmitted(NotSubmitted::InvalidUnit);
    }
    if let Some(signal) = cancelled() {
        return Outcome::NotSubmitted(NotSubmitted::Cancelled(signal));
    }
    if !command_binding(command, unit, detached) {
        return Outcome::NotSubmitted(NotSubmitted::CommandMismatch);
    }
    if detached && record.startup.is_some() {
        return Outcome::NotSubmitted(NotSubmitted::CommandMismatch);
    }
    if detached {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let spawned = {
        #[cfg(test)]
        let _guard = crate::test_support::executable_fixture_guard();
        command.spawn()
    };
    let mut child = match spawned {
        Ok(child) => child,
        Err(_) => return Outcome::NotSubmitted(NotSubmitted::SpawnFailed),
    };
    record.mark_submitted();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut started = false;
    let mut observed: Option<(String, ObservedDomain)> = None;
    let mut startup_released = record.startup.is_none();
    let mut next_query = Instant::now();
    loop {
        let cancellation = poll_cancelled(&mut child, manager, unit, &cancelled, record);
        let expired = Instant::now() >= deadline && startup_wait_required(detached, started);
        if cancellation.is_some() || expired {
            record.mark_settled(reap_client(&mut child));
            let cleanup = cleanup(manager, unit);
            return match cancellation {
                Some(signal) => Outcome::Cancelled {
                    signal,
                    started,
                    cleanup,
                },
                None => Outcome::TimedOut { started, cleanup },
            };
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                record.mark_settled(true);
                let client_code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
                if client_code == 0 && detached {
                    return Outcome::Acknowledged;
                }
                // Every release path needs native termination evidence: a
                // stopped unit or collection after a pinned startup identity.
                // A waited client
                // reporting success while descendants survive (or while the
                // domain cannot be observed) is Unknown, not Completed.
                // Workload status comes from the manager; the client code is
                // only a fallback when the manager is silent.
                match query_state(manager, unit) {
                    Some(state)
                        if startup_released && confirmed_exit(&state, observed.as_ref()) =>
                    {
                        if !retain {
                            let _ = cleanup(manager, unit);
                        }
                        return Outcome::Completed(state.workload_code.unwrap_or(client_code));
                    }
                    _ => {
                        return Outcome::Unknown {
                            exit_code: client_code,
                            // A pinned invocation mismatch or failed final query
                            // must not be bypassed by stopping a replacement.
                            cleanup: if observed.is_some() {
                                Cleanup::Unknown
                            } else {
                                cleanup(manager, unit)
                            },
                        };
                    }
                }
            }
            Ok(None) => {}
            Err(_) => {
                record.mark_settled(reap_client(&mut child));
                return Outcome::Unknown {
                    exit_code: 1,
                    cleanup: cleanup(manager, unit),
                };
            }
        }
        if !started && Instant::now() >= next_query {
            if let Some(state) = state(manager, unit) {
                if record.startup.is_none() {
                    started = state.started;
                }
                if state.started && observed.is_none() {
                    observed = state
                        .invocation
                        .as_ref()
                        .zip(state.control_group.as_ref())
                        .and_then(|(invocation, group)| {
                            if !group.ends_with(&format!("/{unit}")) {
                                return None;
                            }
                            Some((
                                invocation.clone(),
                                ObservedDomain::capture(&safe_cgroup_path(group)?)?,
                            ))
                        });
                }
                if let (Some(barrier), Some((invocation, domain)), Some(main_pid), Some(group)) = (
                    record.startup.as_ref(),
                    observed.as_ref(),
                    state.main_pid,
                    state.control_group.as_deref(),
                ) {
                    // Pinning precedes release. Recheck cancellation after the
                    // bounded manager query, so cancellation during that query
                    // cannot authorize the payload.
                    if let Some(signal) =
                        poll_cancelled(&mut child, manager, unit, &cancelled, record)
                    {
                        record.mark_settled(reap_client(&mut child));
                        return Outcome::Cancelled {
                            signal,
                            started,
                            cleanup: cleanup(manager, unit),
                        };
                    }
                    if Instant::now() >= deadline {
                        continue;
                    }
                    let identity_matches = !state.stopped
                        && state.invocation.as_ref() == Some(invocation)
                        && domain.matches(Some(group))
                        && domain.empty() == Some(false);
                    match identity_matches.then(|| barrier.release(main_pid, group)) {
                        Some(Ok(true)) => {
                            startup_released = true;
                            started = true;
                        }
                        Some(Ok(false)) => {}
                        _ => {
                            record.mark_settled(reap_client(&mut child));
                            return Outcome::Unknown {
                                exit_code: 1,
                                cleanup: Cleanup::Unknown,
                            };
                        }
                    }
                }
            }
            next_query = Instant::now() + Duration::from_millis(100);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waited_workloads_have_no_startup_deadline_after_acknowledgment() {
        assert!(startup_wait_required(false, false));
        assert!(!startup_wait_required(false, true));
        assert!(startup_wait_required(true, true));
    }

    #[test]
    fn detached_acknowledgment_and_unknown_cleanup_retain_capacity() {
        assert!(!Outcome::Acknowledged.releases_reservation());
        assert!(
            !Outcome::Unknown {
                exit_code: 1,
                cleanup: Cleanup::Unknown
            }
            .releases_reservation()
        );
        assert!(
            Outcome::Unknown {
                exit_code: 1,
                cleanup: Cleanup::Stopped
            }
            .releases_reservation()
        );
        assert!(Outcome::Completed(42).releases_reservation());
        assert!(Outcome::NotSubmitted(NotSubmitted::CommandMismatch).releases_reservation());
    }

    fn bound_command(unit: &str, detached: bool) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0", "--user", &format!("--unit={unit}")]);
        if !detached {
            command.arg("--wait");
        }
        command.args(["--", "worker"]);
        command
    }

    #[test]
    fn workload_arguments_do_not_change_manager_binding() {
        let unit = "app-amc-arguments@1.service";
        for detached in [false, true] {
            let mut command = Command::new("systemd-run");
            command.args(["--user", &format!("--unit={unit}"), "--property=Restart=no"]);
            if !detached {
                command.arg("--wait");
            }
            command.args([
                "--",
                "worker",
                "--host=localhost",
                "--machine",
                "--scope",
                "-H",
                "--wait",
                "--unit=foreign.service",
            ]);
            assert!(command_binding(&command, unit, detached));
        }
    }

    #[test]
    fn payload_cannot_supply_missing_manager_identity_or_mode() {
        let unit = "app-amc-arguments@1.service";
        for arguments in [
            vec![
                "--user",
                "--",
                "worker",
                "--unit=app-amc-arguments@1.service",
            ],
            vec![
                "--unit=app-amc-arguments@1.service",
                "--",
                "worker",
                "--user",
            ],
            vec![
                "--user",
                "--unit=app-amc-arguments@1.service",
                "--unit=foreign.service",
                "--",
                "worker",
            ],
        ] {
            let mut command = Command::new("systemd-run");
            command.args(arguments);
            assert!(!command_binding(&command, unit, true));
        }
        let mut command = Command::new("systemd-run");
        command.args([
            "--user",
            &format!("--unit={unit}"),
            "--",
            "worker",
            "--wait",
        ]);
        assert!(!command_binding(&command, unit, false));
    }

    #[test]
    fn submission_requires_identity_and_mode_binding() {
        let unit = "app-amc-bound@1.service";
        let mut missing_unit = Command::new("sh");
        missing_unit.args(["-c", "exit 0"]);
        let record = ClientRecord::default();
        assert_eq!(
            execute(
                &mut missing_unit,
                Path::new("/nonexistent-manager"),
                unit,
                true,
                false,
                || None,
                &record,
            ),
            Outcome::NotSubmitted(NotSubmitted::CommandMismatch)
        );
        assert!(!record.submitted());

        let mut wrong_mode = bound_command(unit, false);
        let record = ClientRecord::default();
        assert_eq!(
            execute(
                &mut wrong_mode,
                Path::new("/nonexistent-manager"),
                unit,
                true,
                false,
                || None,
                &record,
            ),
            Outcome::NotSubmitted(NotSubmitted::CommandMismatch)
        );
        assert!(!record.submitted());

        let mut remote = Command::new("systemd-run");
        remote.args([
            "--user",
            &format!("--unit={unit}"),
            "--machine=elsewhere",
            "--",
            "worker",
        ]);
        let record = ClientRecord::default();
        assert_eq!(
            execute(
                &mut remote,
                Path::new("/nonexistent-manager"),
                unit,
                true,
                false,
                || None,
                &record,
            ),
            Outcome::NotSubmitted(NotSubmitted::CommandMismatch)
        );
        assert!(!record.submitted());
    }

    /// Minimal manager reporting a stopped workload in a collected domain,
    /// using numeric `CLD_*` codes exactly like `systemctl show`.
    #[test]
    fn waited_success_still_requires_termination_evidence() {
        use crate::test_support::{scratch_dir, show_manager};
        let dir = scratch_dir("amc-bound");
        let manager = show_manager(
            &dir,
            r#"
case " $* " in *' stop '*|*' reset-failed '*) exit 0;; esac
printf 'LoadState=loaded\nExecMainStartTimestampMonotonic=1\nActiveState=inactive\nExecMainCode=1\nExecMainStatus=0\nControlGroup=/amc-test-collected\n'
"#,
        );
        let record = ClientRecord::default();
        assert_eq!(
            execute(
                &mut bound_command("app-amc-bound@1.service", false),
                &manager,
                "app-amc-bound@1.service",
                false,
                false,
                || None,
                &record,
            ),
            Outcome::Completed(0)
        );
        assert!(record.submitted() && record.settled());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verified_workload_status_overrides_a_successful_wait_client() {
        use crate::test_support::{scratch_dir, show_manager};
        let dir = scratch_dir("amc-workload-status");
        for (code, status, expected) in [(1, 42, 42), (2, 9, 137), (1, 0, 0)] {
            let manager = show_manager(
                &dir,
                &format!(
                    "printf 'LoadState=loaded\\nExecMainStartTimestampMonotonic=1\\nActiveState=inactive\\nExecMainCode={code}\\nExecMainStatus={status}\\nControlGroup=/amc-test-collected\\n'"
                ),
            );
            assert_eq!(
                execute(
                    &mut bound_command("app-amc-status@1.service", false),
                    &manager,
                    "app-amc-status@1.service",
                    false,
                    true,
                    || None,
                    &ClientRecord::default(),
                ),
                Outcome::Completed(expected)
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_barrier_cannot_be_bypassed_by_a_successful_wait_client() {
        use crate::test_support::{scratch_dir, show_manager};
        let dir = scratch_dir("amc-startup-required");
        let manager = show_manager(
            &dir,
            "printf 'LoadState=loaded\\nExecMainStartTimestampMonotonic=1\\nActiveState=inactive\\nExecMainCode=1\\nExecMainStatus=0\\nControlGroup=/amc-test-collected\\n'",
        );
        let record = ClientRecord::with_startup(
            StartupBarrier::new(format!("amc-start-bypass-{}", std::process::id())).unwrap(),
        );
        let outcome = execute(
            &mut bound_command("app-amc-startup@1.service", false),
            &manager,
            "app-amc-startup@1.service",
            false,
            false,
            || None,
            &record,
        );
        assert!(matches!(outcome, Outcome::Unknown { exit_code: 0, .. }));
        assert!(record.submitted() && record.settled());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsafe_cgroup_paths_are_unknown() {
        assert!(safe_cgroup_path("relative/path").is_none());
        assert!(safe_cgroup_path("/has/../traversal").is_none());
        assert!(safe_cgroup_path("").is_none());
        assert_eq!(
            safe_cgroup_path("/user.slice/app.service"),
            Some(Path::new("/sys/fs/cgroup/user.slice/app.service").to_path_buf())
        );
    }

    #[test]
    fn unit_cgroup_dir_resolves_reported_groups() {
        use crate::test_support::{scratch_dir, show_manager};
        let dir = scratch_dir("amc-unit-cgroup");
        let manager = show_manager(
            &dir,
            "printf 'LoadState=loaded\\nActiveState=active\\nControlGroup=/amc-test-x\\n'",
        );
        assert_eq!(
            unit_cgroup_dir(&manager, "app-amc-x@1.service"),
            Some(Path::new("/sys/fs/cgroup/amc-test-x").to_path_buf())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unit_cgroup_dir_rejects_unobservable_units() {
        use crate::test_support::{scratch_dir, show_manager};
        let dir = scratch_dir("amc-unit-cgroup");
        let failing = show_manager(&dir, "exit 1");
        assert_eq!(unit_cgroup_dir(&failing, "app-amc-x@1.service"), None);
        let absent = show_manager(
            &dir,
            "printf 'LoadState=not-found\\nActiveState=inactive\\n'",
        );
        assert_eq!(unit_cgroup_dir(&absent, "app-amc-x@1.service"), None);
        let relative = show_manager(
            &dir,
            "printf 'LoadState=loaded\\nActiveState=active\\nControlGroup=relative\\n'",
        );
        assert_eq!(unit_cgroup_dir(&relative, "app-amc-x@1.service"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cgroup_population_decides_termination() {
        let root = std::env::temp_dir().join(format!(
            "amc-cgroup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let group = root.join("unit.service");
        std::fs::create_dir_all(&group).unwrap();
        // Absent domain (collected) counts as empty.
        assert_eq!(domain_empty(&root.join("missing.service")), Some(true));
        // Native hierarchical population key decides.
        std::fs::write(group.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert_eq!(domain_empty(&group), Some(true));
        std::fs::write(group.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert_eq!(domain_empty(&group), Some(false));
        // A present domain without the population key is unknown, not empty.
        std::fs::remove_file(group.join("cgroup.events")).unwrap();
        assert_eq!(domain_empty(&group), None);
        // Malformed population values are unknown, not empty.
        std::fs::write(group.join("cgroup.events"), "populated many\n").unwrap();
        assert_eq!(domain_empty(&group), None);
        assert_eq!(terminated(Some("/amc-test-collected")), Some(true));
        assert_eq!(terminated(Some("relative/path")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn collected_domain_requires_its_original_visible_parent() {
        let root = crate::test_support::scratch_dir("amc-collected-domain");
        let parent = root.join("slice");
        let group = parent.join("job.service");
        fs::create_dir_all(&group).unwrap();
        fs::write(group.join("cgroup.events"), "populated 1\n").unwrap();
        let observed = ObservedDomain::capture(&group).unwrap();
        assert_eq!(observed.empty(), Some(false));
        fs::write(group.join("cgroup.events"), "populated 0\n").unwrap();
        assert_eq!(observed.empty(), Some(true));
        fs::remove_dir_all(&group).unwrap();
        assert_eq!(observed.empty(), Some(true));
        fs::rename(&parent, root.join("hidden-slice")).unwrap();
        assert_eq!(observed.empty(), None);
        fs::create_dir(&parent).unwrap();
        assert_eq!(observed.empty(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaced_domain_or_unknown_population_is_not_termination() {
        let root = crate::test_support::scratch_dir("amc-replaced-domain");
        let group = root.join("job.service");
        fs::create_dir(&group).unwrap();
        let observed = ObservedDomain::capture(&group).unwrap();
        assert_eq!(observed.empty(), None);
        fs::write(group.join("cgroup.events"), "populated invalid\n").unwrap();
        assert_eq!(observed.empty(), None);
        fs::rename(&group, root.join("original.service")).unwrap();
        fs::create_dir(&group).unwrap();
        fs::write(group.join("cgroup.events"), "populated 0\n").unwrap();
        assert_eq!(observed.empty(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collected_exit_needs_startup_identity_and_no_native_contradiction() {
        let root = crate::test_support::scratch_dir("amc-collected-exit");
        let group = root.join("job.service");
        fs::create_dir(&group).unwrap();
        fs::write(group.join("cgroup.events"), "populated 1\n").unwrap();
        let observed = (
            "0123456789abcdef0123456789abcdef".into(),
            ObservedDomain::capture(&group).unwrap(),
        );
        let mut state = UnitState {
            loaded: false,
            invocation: None,
            started: false,
            stopped: true,
            control_group: Some(String::new()),
            main_pid: None,
            workload_code: None,
        };
        assert!(!confirmed_exit(&state, None));
        assert!(!confirmed_exit(&state, Some(&observed)));
        fs::remove_dir_all(&group).unwrap();
        assert!(confirmed_exit(&state, Some(&observed)));
        state.stopped = false;
        assert!(!confirmed_exit(&state, Some(&observed)));
        state.stopped = true;
        state.loaded = true;
        state.started = true;
        state.invocation = Some(observed.0.clone());
        assert!(confirmed_exit(&state, Some(&observed)));
        state.invocation = Some("fedcba9876543210fedcba9876543210".into());
        assert!(!confirmed_exit(&state, Some(&observed)));
        state.invocation = Some(observed.0.clone());
        state.control_group = Some("/other/job.service".into());
        assert!(!confirmed_exit(&state, Some(&observed)));
        fs::remove_dir_all(root).unwrap();
    }
}
