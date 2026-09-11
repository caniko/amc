use super::{
    Cleanup, ClientRecord, Outcome, cleanup, execute, state, valid_environment_name, valid_unit,
};
use crate::{
    AdmitError,
    provider::SharedMemoryProvider,
    weighted::{SyncWeightedAdmissionGate, WeightedConfig, WeightedConfigError, WeightedPermit},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Everything needed to submit one managed job. The runner constructs the
/// submission command from this request; callers never hand-craft
/// `systemd-run` arguments for managed jobs, so a reservation cannot be
/// bound to a differently-targeted command.
pub struct LaunchRequest<'a> {
    /// Resolved `systemd-run` executable.
    pub launcher: &'a Path,
    /// Fresh AMC service identity; never reuse an uncertain submission identity.
    pub unit: &'a str,
    /// Conservative peak allocation in bytes, strictly greater than zero.
    pub weight: u64,
    /// Maximum capacity-wait time, excluding provider I/O.
    pub admission_timeout: Duration,
    /// Return after acknowledgment instead of workload exit.
    pub detached: bool,
    /// Resolved workload executable followed by its arguments.
    pub argv: &'a [String],
    /// Validated environment names forwarded via `--setenv=`.
    pub environment: &'a [String],
    /// Extra `Name=Value` resource properties (memory limits and similar).
    /// Effective enforcement is verified separately; see the runner docs.
    pub properties: &'a [String],
}

/// Build the submission command for a validated request. The unit identity,
/// wait mode, and run-once lifecycle are imposed here: transient units are
/// always retained (never `--collect`, so post-exit state stays observable)
/// and never restarted, so a released reservation cannot cover a replay.
fn build_command(request: &LaunchRequest<'_>) -> Result<Command, RunError> {
    if !valid_unit(request.unit) || request.weight == 0 || request.argv.is_empty() {
        return Err(RunError::InvalidJob);
    }
    for name in request.environment {
        if !valid_environment_name(name) {
            return Err(RunError::InvalidJob);
        }
    }
    for property in request.properties {
        let mut parts = property.splitn(2, '=');
        match (parts.next(), parts.next()) {
            (Some(name), Some(_))
                if !name.is_empty()
                    && !name.starts_with('-')
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => {}
            _ => return Err(RunError::InvalidJob),
        }
    }
    let mut command = Command::new(request.launcher);
    command.args(["--user", "--quiet", "--expand-environment=no", "--same-dir"]);
    command.arg(format!("--unit={}", request.unit));
    command.args(["--property=Restart=no", "--service-type=exec"]);
    if !request.detached {
        command.args(["--wait", "--pipe"]);
    }
    for name in request.environment {
        command.arg(format!("--setenv={name}"));
    }
    for property in request.properties {
        command.arg(format!("--property={property}"));
    }
    command.arg("--");
    command.args(request.argv.iter());
    Ok(command)
}

/// Failure before submission or while requesting reconciliation.
#[derive(Debug)]
pub enum RunError {
    /// Invalid admission configuration.
    Config(WeightedConfigError),
    /// Managed reservations require enabled, fail-closed admission.
    AdvisoryAdmission,
    /// Admission refused the requested reservation.
    Admission(AdmitError),
    /// Invalid service identity or zero reservation.
    InvalidJob,
    /// Another submission has an uncertain outcome and must be reconciled first.
    Unreconciled,
    /// This identity is already tracked by the runner.
    DuplicateIdentity,
    /// No reservation is tracked for the requested identity.
    NotTracked,
    /// Launch is still executing; reconciliation must not race submission.
    Busy,
    /// Application cancellation while waiting for capacity.
    Cancelled(i32),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => write!(f, "{error}"),
            Self::Admission(error) => write!(f, "{error}"),
            Self::AdvisoryAdmission => f.write_str("managed jobs require fail-closed admission"),
            Self::InvalidJob => f.write_str("invalid AMC identity or zero job weight"),
            Self::Unreconciled => {
                f.write_str("reconcile uncertain submissions before admitting more jobs")
            }
            Self::DuplicateIdentity => f.write_str("unit identity already tracked"),
            Self::NotTracked => f.write_str("unit identity is not tracked"),
            Self::Busy => f.write_str("submission is still executing"),
            Self::Cancelled(signal) => write!(f, "cancelled before submission ({signal})"),
        }
    }
}
impl std::error::Error for RunError {}

struct Entry {
    permit: Option<WeightedPermit>,
    submitting: bool,
    unknown: bool,
}

impl Drop for Entry {
    fn drop(&mut self) {
        // Dropping a coordinator is not proof that a systemd workload died.
        // Explicit reconciliation takes the permit before this fallback runs.
        if let Some(permit) = self.permit.take() {
            std::mem::forget(permit);
        }
    }
}

/// Coordinates admission and execution through the shared systemd backend.
///
/// Supply a provider for the worker's resource domain, not necessarily the
/// calling process. Submission commands are constructed from each request,
/// so managed reservations cannot leak onto foreign identities; effective
/// native memory properties remain the caller's responsibility and are
/// verified separately. Share one runner between concurrent callers (for
/// example via `Arc`).
///
/// Reconcile detached/uncertain jobs before dropping the runner. Otherwise
/// their permits are deliberately leaked rather than incorrectly released.
/// There is no automatic restart or replay, and no process-global cancellation.
pub struct Runner {
    gate: SyncWeightedAdmissionGate,
    manager: PathBuf,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl Runner {
    /// Construct a strict reservation owner using an explicit manager executable.
    pub fn new(
        config: WeightedConfig,
        provider: SharedMemoryProvider,
        manager: PathBuf,
    ) -> Result<Self, RunError> {
        let config = config.validate().map_err(RunError::Config)?;
        if !config.base.memory_scheduler_enabled || config.base.fail_open_on_provider_error {
            return Err(RunError::AdvisoryAdmission);
        }
        Ok(Self {
            gate: SyncWeightedAdmissionGate::new(config, provider),
            manager,
            entries: Mutex::new(BTreeMap::new()),
        })
    }

    fn ready(entries: &BTreeMap<String, Entry>, unit: &str) -> Result<(), RunError> {
        if entries.values().any(|entry| entry.unknown) {
            return Err(RunError::Unreconciled);
        }
        if entries.contains_key(unit) {
            return Err(RunError::DuplicateIdentity);
        }
        Ok(())
    }

    /// Acquire capacity and submit once. Detached/unknown outcomes retain capacity.
    ///
    /// The submission command is constructed from the request, so the
    /// reservation cannot be bound to a differently-targeted command.
    /// Cancellation is checked between bounded capacity waits and manager
    /// queries. Provider I/O itself is not interruptible; expiry is rechecked
    /// immediately before each acquisition after the first, so a blocked
    /// retry that outlasts the deadline starts no new probe (the documented
    /// initial probe is the only exception). A panic before the client spawns
    /// forgets the identity without holding capacity; a panic afterwards
    /// retains the reservation, and an unsettled client keeps the attempt
    /// blocked rather than reconcilable.
    pub fn run(
        &self,
        request: LaunchRequest<'_>,
        cancelled: impl Fn() -> Option<i32>,
    ) -> Result<Outcome, RunError> {
        let mut command = build_command(&request)?;
        let deadline = Instant::now().checked_add(request.admission_timeout);
        let mut attempted = false;
        let permit = loop {
            Self::ready(
                &self.entries.lock().expect("runner registry poisoned"),
                request.unit,
            )?;
            if let Some(signal) = cancelled() {
                return Err(RunError::Cancelled(signal));
            }
            if attempted && deadline.is_some_and(|end| Instant::now() >= end) {
                return Err(RunError::Admission(AdmitError::TimedOut));
            }
            attempted = true;
            let wait = deadline.map_or(Duration::from_millis(100), |end| {
                end.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
            });
            match self.gate.acquire_timeout(request.weight, wait) {
                Ok(permit) => break permit,
                Err(AdmitError::TimedOut) => continue,
                Err(error) => return Err(RunError::Admission(error)),
            }
        };
        {
            let mut entries = self.entries.lock().expect("runner registry poisoned");
            Self::ready(&entries, request.unit)?;
            entries.insert(
                request.unit.to_owned(),
                Entry {
                    permit: Some(permit),
                    submitting: true,
                    unknown: false,
                },
            );
        }
        let record = ClientRecord::default();
        let mut attempt = Attempt {
            runner: self,
            unit: request.unit,
            record: &record,
            finished: false,
        };
        // Retain is forced: transient units stay observable for post-exit
        // state, and nothing is ever automatically restarted or replayed.
        let outcome = execute(
            &mut command,
            &self.manager,
            request.unit,
            request.detached,
            true,
            cancelled,
            &record,
        );
        let mut entries = self.entries.lock().expect("runner registry poisoned");
        if outcome.releases_reservation() {
            drop(
                entries
                    .remove(request.unit)
                    .expect("registered unit")
                    .permit
                    .take(),
            );
        } else {
            let entry = entries.get_mut(request.unit).expect("registered unit");
            entry.submitting = false;
            entry.unknown = outcome != Outcome::Acknowledged;
        }
        attempt.finished = true;
        Ok(outcome)
    }

    /// Observe a terminal unit, optionally requesting stop first. Only a
    /// stopped unit with an empty workload domain releases capacity; a
    /// stopped service that still holds processes (or cannot be observed)
    /// retains the reservation and blocks subsequent submissions.
    pub fn reconcile(&self, unit: &str, stop: bool) -> Result<Cleanup, RunError> {
        // ponytail: serialize bounded reconciliations; use per-unit locks if
        // concurrent manager-query throughput becomes important.
        let mut entries = self.entries.lock().expect("runner registry poisoned");
        let entry = entries.get_mut(unit).ok_or(RunError::NotTracked)?;
        if entry.submitting {
            return Err(RunError::Busy);
        }
        let result = if stop {
            cleanup(&self.manager, unit)
        } else {
            match state(&self.manager, unit) {
                Some(state) if state.started && !state.stopped => Cleanup::Running,
                Some(state)
                    if state.stopped
                        && super::terminated(state.control_group.as_deref()) == Some(true) =>
                {
                    Cleanup::Stopped
                }
                _ => Cleanup::Unknown,
            }
        };
        if result == Cleanup::Stopped {
            drop(entries.remove(unit).expect("tracked unit").permit.take());
        } else {
            entry.unknown = result == Cleanup::Unknown;
        }
        Ok(result)
    }

    /// Current byte reservations, including detached and uncertain workloads.
    pub fn committed_bytes(&self) -> u64 {
        self.gate.committed_bytes()
    }

    /// Identities requiring observation or cleanup before discarding the runner.
    pub fn tracked_units(&self) -> Vec<String> {
        self.entries
            .lock()
            .expect("runner registry poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

struct Attempt<'a> {
    runner: &'a Runner,
    unit: &'a str,
    record: &'a ClientRecord,
    finished: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut entries = self
            .runner
            .entries
            .lock()
            .expect("runner registry poisoned");
        if !self.record.submitted() {
            // Nothing was submitted: release the reservation instead of
            // leaking it behind a permanently uncertain identity.
            if let Some(mut entry) = entries.remove(self.unit) {
                drop(entry.permit.take());
            }
        } else if let Some(entry) = entries.get_mut(self.unit) {
            // The client exists, so the workload may too. Retain the
            // reservation in all cases; an unsettled client additionally
            // keeps the attempt blocked instead of reconcilable.
            entry.unknown = true;
            entry.submitting = !self.record.settled();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::FixedProvider;
    use crate::{MemoryStats, ProviderError};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn shell() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .unwrap()
    }

    fn script(path: &std::path::Path, text: &str) {
        fs::write(path, format!("#!{}\n{text}\n", shell().display())).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "amc-managed-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            // Fake launcher: logs the constructed submission, then runs the
            // workload after `--`, like a real `systemd-run` client would.
            script(
                &dir.join("systemd-run"),
                r#"
printf '%s\n' "$@" >> "$0.calls"
while test $# -gt 0 && test "$1" != "--"; do shift; done
test $# -gt 0 || exit 99
shift
exec "$@"
"#,
            );
            script(
                &dir.join("manager"),
                r#"
unit=""
for a in "$@"; do case "$a" in app-amc-*.service) unit="$a";; esac; done
if test -n "$unit" && test -f "$0.state.$unit"; then state=$(cat "$0.state.$unit");
else state=$(cat "$0.state"); fi
test "$state" != unknown || exit 1
case " $* " in
  *' stop '*)
    printf inactive > "$0.state"
    if test -n "$unit"; then printf inactive > "$0.state.$unit"; fi
    state=inactive ;;
esac
printf 'LoadState=loaded\nExecMainStartTimestampMonotonic=1\nActiveState=%s\n' "$state"
printf 'ControlGroup=%s\n' "$(cat "$0.cgpath")"
if test "$state" = inactive; then printf 'ExecMainCode=1\nExecMainStatus=7\n'; fi
"#,
            );
            let fixture = Self(dir);
            fixture.state("active");
            // The manager reports an absent group: nothing runs there, so the
            // domain counts as terminated once the unit itself is stopped.
            let cg = format!(
                "/amc-test-{}",
                fixture.0.file_name().unwrap().to_string_lossy()
            );
            fs::write(fixture.0.join("manager.cgpath"), &cg).unwrap();
            fixture
        }

        /// Point the fake manager at a live group holding this test process,
        /// so population checks observe a genuinely surviving process.
        fn live_group(&self) -> String {
            let placement = fs::read_to_string("/proc/self/cgroup").expect("cgroup v2 placement");
            let path = placement
                .lines()
                .find_map(|line| line.strip_prefix("0::"))
                .expect("cgroup v2 hierarchy");
            fs::write(self.0.join("manager.cgpath"), path).unwrap();
            path.to_owned()
        }
        fn state(&self, value: &str) {
            fs::write(self.0.join("manager.state"), value).unwrap();
        }
        fn unit_state(&self, unit: &str, value: &str) {
            fs::write(self.0.join(format!("manager.state.{unit}")), value).unwrap();
        }
        fn launcher(&self) -> PathBuf {
            self.0.join("systemd-run")
        }
        fn runner(&self) -> Runner {
            Runner::new(
                WeightedConfig {
                    safety_reserve_bytes: 0,
                    ..WeightedConfig::default()
                },
                FixedProvider::shared(0.0),
                self.0.join("manager"),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn workload(script: &str) -> Vec<String> {
        vec![
            shell().to_string_lossy().into_owned(),
            "-c".to_owned(),
            script.to_owned(),
        ]
    }

    fn request<'a>(
        launcher: &'a Path,
        argv: &'a [String],
        unit: &'a str,
        detached: bool,
    ) -> LaunchRequest<'a> {
        LaunchRequest {
            launcher,
            unit,
            weight: 100,
            admission_timeout: Duration::ZERO,
            detached,
            argv,
            environment: &[],
            properties: &[],
        }
    }

    #[test]
    fn managed_command_carries_identity_wait_mode_and_lifecycle() {
        let fixture = Fixture::new();
        let unit = "app-amc-build@1.service";
        fixture.unit_state(unit, "inactive");
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let argv = workload("exit 0");
        let environment = ["TEST_MARKER".to_owned()];
        let properties = ["MemoryMax=12345".to_owned()];
        let request = LaunchRequest {
            launcher: &launcher,
            unit,
            weight: 100,
            admission_timeout: Duration::ZERO,
            detached: false,
            argv: &argv,
            environment: &environment,
            properties: &properties,
        };
        assert_eq!(runner.run(request, || None).unwrap(), Outcome::Completed(0));
        let calls = fs::read_to_string(fixture.0.join("systemd-run.calls")).unwrap();
        let lines: Vec<_> = calls.lines().collect();
        assert!(lines.contains(&format!("--unit={unit}").as_str()));
        assert!(lines.contains(&"--wait"));
        assert!(lines.contains(&"--pipe"));
        assert!(lines.contains(&"--property=Restart=no"));
        assert!(lines.contains(&"--setenv=TEST_MARKER"));
        assert!(lines.contains(&"--property=MemoryMax=12345"));
        assert!(!lines.contains(&"--collect"));
        let dash = lines.iter().position(|line| *line == "--").unwrap();
        let workload: Vec<&str> = argv.iter().map(String::as_str).collect();
        assert_eq!(&lines[dash + 1..], workload.as_slice());
    }

    #[test]
    fn invalid_requests_never_spawn() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let argv = workload("exit 0");
        let unit = "app-amc-invalid@1.service";
        for request in [
            LaunchRequest {
                launcher: &launcher,
                unit,
                weight: 100,
                admission_timeout: Duration::ZERO,
                detached: true,
                argv: &[],
                environment: &[],
                properties: &[],
            },
            LaunchRequest {
                launcher: &launcher,
                unit,
                weight: 100,
                admission_timeout: Duration::ZERO,
                detached: true,
                argv: &argv,
                environment: &["1BAD".to_owned()],
                properties: &[],
            },
            LaunchRequest {
                launcher: &launcher,
                unit,
                weight: 100,
                admission_timeout: Duration::ZERO,
                detached: true,
                argv: &argv,
                environment: &[],
                properties: &["NoEquals".to_owned()],
            },
        ] {
            assert!(matches!(
                runner.run(request, || None),
                Err(RunError::InvalidJob)
            ));
        }
        assert!(!fixture.0.join("systemd-run.calls").exists());
        assert_eq!(runner.committed_bytes(), 0);
    }

    #[test]
    fn detached_reservation_lasts_until_confirmed_termination() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let unit = "app-amc-detached@1.service";
        let argv = workload("exit 0");
        assert_eq!(
            runner
                .run(request(&launcher, &argv, unit, true), || None)
                .unwrap(),
            Outcome::Acknowledged
        );
        assert_eq!(runner.committed_bytes(), 100);
        assert_eq!(runner.reconcile(unit, false).unwrap(), Cleanup::Running);
        assert_eq!(runner.committed_bytes(), 100);
        assert_eq!(runner.reconcile(unit, true).unwrap(), Cleanup::Stopped);
        assert_eq!(runner.committed_bytes(), 0);
        assert!(runner.tracked_units().is_empty());
    }

    #[test]
    fn cancellation_with_unknown_cleanup_blocks_until_reconciled() {
        let fixture = Fixture::new();
        fixture.state("unknown");
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let calls = AtomicUsize::new(0);
        let unit = "app-amc-cancelled@1.service";
        let argv = workload("exec sleep 60");
        let outcome = runner
            .run(request(&launcher, &argv, unit, true), || {
                (calls.fetch_add(1, Ordering::Relaxed) >= 2).then_some(15)
            })
            .unwrap();
        assert!(matches!(
            outcome,
            Outcome::Cancelled {
                cleanup: Cleanup::Unknown,
                ..
            }
        ));
        assert_eq!(runner.committed_bytes(), 100);
        let next_argv = workload("exit 0");
        assert!(matches!(
            runner.run(
                request(&launcher, &next_argv, "app-amc-next@2.service", true),
                || None,
            ),
            Err(RunError::Unreconciled)
        ));
        assert_eq!(runner.reconcile(unit, true).unwrap(), Cleanup::Unknown);
        assert_eq!(runner.committed_bytes(), 100);
        fixture.state("inactive");
        assert_eq!(runner.reconcile(unit, false).unwrap(), Cleanup::Stopped);
        assert_eq!(runner.committed_bytes(), 0);
    }

    #[test]
    fn concurrent_calls_do_not_share_cancellation() {
        let fixture = Fixture::new();
        fixture.unit_state("app-amc-other@2.service", "inactive");
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let cancelled = scope.spawn(|| {
                barrier.wait();
                let argv = workload("exit 0");
                runner.run(
                    request(&launcher, &argv, "app-amc-cancel@1.service", true),
                    || Some(2),
                )
            });
            let other = scope.spawn(|| {
                barrier.wait();
                let argv = workload("exit 0");
                runner.run(
                    request(&launcher, &argv, "app-amc-other@2.service", false),
                    || None,
                )
            });
            assert!(matches!(
                cancelled.join().unwrap(),
                Err(RunError::Cancelled(2))
            ));
            assert_eq!(other.join().unwrap().unwrap(), Outcome::Completed(0));
        });
        assert_eq!(runner.committed_bytes(), 0);
    }

    #[test]
    fn dropping_runner_does_not_claim_detached_workload_stopped() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let gate = runner.gate.clone();
        let argv = workload("exit 0");
        runner
            .run(
                request(&launcher, &argv, "app-amc-drop@1.service", true),
                || None,
            )
            .unwrap();
        drop(runner);
        assert_eq!(gate.committed_bytes(), 100);
    }

    #[test]
    fn fail_open_admission_is_rejected_for_managed_jobs() {
        let mut config = WeightedConfig::default();
        config.base.fail_open_on_provider_error = true;
        assert!(matches!(
            Runner::new(config, FixedProvider::shared(0.0), "unused".into()),
            Err(RunError::AdvisoryAdmission)
        ));
    }

    #[test]
    fn workload_status_comes_from_the_manager_not_the_client() {
        let fixture = Fixture::new();
        // The workload already stopped; the fake manager reports numeric
        // status 7 while the submission client exits 3.
        fixture.state("inactive");
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let unit = "app-amc-workfail@1.service";
        let argv = workload("exit 3");
        assert_eq!(
            runner
                .run(request(&launcher, &argv, unit, false), || None)
                .unwrap(),
            Outcome::Completed(7)
        );
        assert_eq!(runner.committed_bytes(), 0);
    }

    #[test]
    fn stopped_service_with_surviving_processes_stays_uncertain() {
        let fixture = Fixture::new();
        fixture.live_group();
        fixture.state("inactive");
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let unit = "app-amc-survivor@1.service";
        let argv = workload("exit 0");
        assert_eq!(
            runner
                .run(request(&launcher, &argv, unit, true), || None)
                .unwrap(),
            Outcome::Acknowledged
        );
        // The service is stopped but its domain still holds this process.
        assert_eq!(runner.reconcile(unit, false).unwrap(), Cleanup::Unknown);
        assert_eq!(runner.committed_bytes(), 100);
        let next_argv = workload("exit 0");
        assert!(matches!(
            runner.run(
                request(&launcher, &next_argv, "app-amc-next@2.service", true),
                || None,
            ),
            Err(RunError::Unreconciled)
        ));
    }

    #[test]
    fn unsettled_submission_stays_blocked() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        let unit = "app-amc-unsettled@1.service";
        let permit = runner.gate.acquire_timeout(100, Duration::ZERO).unwrap();
        runner
            .entries
            .lock()
            .expect("runner registry poisoned")
            .insert(
                unit.to_owned(),
                Entry {
                    permit: Some(permit),
                    submitting: true,
                    unknown: false,
                },
            );
        let record = ClientRecord::default();
        record.mark_submitted();
        // The client was spawned but never confirmed gone: the identity must
        // stay blocked, and the uncertainty must block other submissions too.
        drop(Attempt {
            runner: &runner,
            unit,
            record: &record,
            finished: false,
        });
        assert!(matches!(runner.reconcile(unit, false), Err(RunError::Busy)));
        let launcher = fixture.launcher();
        let argv = workload("exit 0");
        assert!(matches!(
            runner.run(
                request(&launcher, &argv, "app-amc-other@2.service", true),
                || None,
            ),
            Err(RunError::Unreconciled)
        ));
        assert_eq!(runner.committed_bytes(), 100);
        assert_eq!(
            runner.tracked_units(),
            vec![unit.to_owned()],
            "unsettled identity stays tracked"
        );
    }

    struct FlipFlop {
        calls: AtomicUsize,
    }

    impl crate::MemoryProvider for FlipFlop {
        fn used_fraction(&self) -> Result<f64, ProviderError> {
            Ok(self.stats()?.used_fraction())
        }

        fn stats(&self) -> Result<MemoryStats, ProviderError> {
            let calls = self.calls.fetch_add(1, Ordering::SeqCst);
            MemoryStats::new(1024, if calls == 0 { 100 } else { 1024 }, 0)
        }
    }

    #[test]
    fn expired_admission_deadline_starts_no_new_probe() {
        let fixture = Fixture::new();
        let provider = std::sync::Arc::new(FlipFlop {
            calls: AtomicUsize::new(0),
        });
        let runner = Runner::new(
            WeightedConfig {
                safety_reserve_bytes: 0,
                ..WeightedConfig::default()
            },
            provider.clone(),
            "unused".into(),
        )
        .unwrap();
        let launcher = fixture.launcher();
        let argv = workload("exit 0");
        // Capacity appears on the second probe, but the zero deadline already
        // expired: the runner must not spend it.
        assert!(matches!(
            runner.run(
                request(&launcher, &argv, "app-amc-late@1.service", true),
                || None,
            ),
            Err(RunError::Admission(AdmitError::TimedOut))
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runner.committed_bytes(), 0);
    }

    #[test]
    fn panic_before_spawn_forgets_the_identity() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let unit = "app-amc-prespawn@1.service";
        let argv = workload("exit 0");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runner.run(request(&launcher, &argv, unit, true), || {
                panic!("cancellation exploded")
            })
        }));
        assert!(result.is_err());
        assert_eq!(runner.committed_bytes(), 0);
        assert!(runner.tracked_units().is_empty());
    }

    #[test]
    fn panic_after_spawn_retains_the_reservation() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        let launcher = fixture.launcher();
        let unit = "app-amc-postspawn@1.service";
        let argv = workload("exec sleep 60");
        let calls = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runner.run(request(&launcher, &argv, unit, true), || {
                // Admission probe, pre-spawn check, then the polled check.
                if calls.fetch_add(1, Ordering::SeqCst) >= 2 {
                    panic!("cancellation exploded");
                }
                None
            })
        }));
        assert!(result.is_err());
        // The submission may have happened: capacity stays held and the
        // identity stays tracked until reconciliation proves termination.
        assert_eq!(runner.committed_bytes(), 100);
        assert_eq!(runner.tracked_units(), vec![unit.to_owned()]);
        assert_eq!(runner.reconcile(unit, true).unwrap(), Cleanup::Stopped);
        assert_eq!(runner.committed_bytes(), 0);
    }
}
