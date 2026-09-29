//! Real-path observer evidence tests.
//!
//! These drive the built `amc watch` / `amc diff` binaries against a fake
//! `systemctl` (property-driven shell script on PATH) and the test
//! process's own readable cgroup. No fixture is started, stopped, or
//! reconfigured: the manager is fake, the kernel files are read-only,
//! and every mutation targets files the test itself owns.
//!
//! Synthetic cgroup directories are deliberately not used: watch resolves
//! its target under `/sys/fs/cgroup`, which unprivileged tests cannot
//! populate. Replacement and disappearance mechanics are covered by unit
//! tests over the same summary path; here the wiring is exercised.

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ObserveFixture {
    root: PathBuf,
}

impl ObserveFixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "amc-observe-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let this = Self { root };
        this.script(
            "systemctl",
            r#"
echo "$*" >> "$FAKE_STATE/calls"
n=$(ls "$FAKE_STATE" | grep -c '^call-')
touch "$FAKE_STATE/call-$n"
case "$*" in
  *--version*) echo "systemd 255 (fake observer build)"; exit 0 ;;
esac
if [ "$FAKE_FAIL" = all ]; then exit 1; fi
case "$*" in
  *Result*)
    # Only the terminal query asks for Result past startup (call 0):
    # polls and attachment checks never carry it.
    if [ "$FAKE_FAIL_FINAL" = 1 ] && [ "$n" -ge 1 ]; then exit 1; fi
    # A final-only invocation override simulates a restart in the final
    # gap: sampling saw one lifetime, the independent endpoint query sees
    # the next. Polls never carry Result, so they keep the attach ID.
    if [ -n "$FAKE_FINAL_INVOCATION" ] && [ "$n" -ge 1 ]; then
      printf 'ControlGroup=%s\nInvocationID=%s\nResult=%s\n' \
        "$FAKE_CGROUP" "$FAKE_FINAL_INVOCATION" "${FAKE_RESULT:-success}"
      exit 0
    fi
    ;;
esac
inv="$FAKE_INVOCATION"
if [ -n "$FAKE_INVOCATION_ALT" ] && [ "$n" -ge "${FAKE_FLIP_AT:-999999}" ]; then
  inv="$FAKE_INVOCATION_ALT"
fi
printf 'ControlGroup=%s\nInvocationID=%s\nResult=%s\n' \
  "$FAKE_CGROUP" "$inv" "${FAKE_RESULT:-success}"
exit 0
"#,
        );
        this
    }

    fn script(&self, name: &str, text: &str) {
        let shell = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .expect("pinned test environment needs sh");
        let path = self.root.join(name);
        fs::write(&path, format!("#!{}\n{text}\n", shell.display())).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_amc"));
        let mut paths = vec![self.root.clone()];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
        command
            .env("PATH", env::join_paths(paths).unwrap())
            .env("FAKE_STATE", &self.root)
            .env("FAKE_INVOCATION", "abcdef0123456789abcdef0123456789");
        command
    }

    /// Watch command bound to this process's own cgroup, or `None` with a
    /// SKIP note when the sandbox exposes no readable cgroup telemetry
    /// (the nix package sandbox has no `/sys`). Skipping keeps restricted
    /// builds green without pretending the path was exercised.
    fn watch_own_cgroup(&self, seconds: u64, interval_ms: u64, output: &Path) -> Option<Command> {
        let Some(group) = readable_own_cgroup() else {
            eprintln!("SKIP: no readable cgroup telemetry in this environment");
            return None;
        };
        let mut command = self.watch(seconds, interval_ms, output);
        command.env("FAKE_CGROUP", group);
        Some(command)
    }

    fn watch(&self, seconds: u64, interval_ms: u64, output: &Path) -> Command {
        let mut command = self.command();
        command.args([
            "watch",
            "test.service",
            "--seconds",
            &seconds.to_string(),
            "--interval-ms",
            &interval_ms.to_string(),
            "--output",
        ]);
        command.arg(output);
        command
    }

    fn summary(&self, output: &Path) -> serde_json::Value {
        let text = fs::read_to_string(output.join("summary.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

impl Drop for ObserveFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// This test process's own cgroup when its telemetry files are actually
/// readable. `None` in restricted sandboxes (no `/sys`), where callers
/// skip instead of failing on the environment.
fn readable_own_cgroup() -> Option<String> {
    let group = fs::read_to_string("/proc/self/cgroup")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|line| line.starts_with('/'))
        .map(str::to_string)?;
    let dir = Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
    for file in ["memory.events", "cgroup.events"] {
        if fs::read_to_string(dir.join(file)).is_err() {
            return None;
        }
    }
    Some(group)
}

fn wait_for_lines(path: &Path, count: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let lines = fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0);
        if lines >= count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "only {lines} sample lines in {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_exit(mut child: Child, timeout: Duration) -> std::process::Output {
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("observed child hung");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn missing_unit_reports_observer_not_ready() {
    let f = ObserveFixture::new();
    let out = f.root.join("out");
    let mut command = f.watch(2, 100, &out);
    command.env("FAKE_CGROUP", "");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let summary = f.summary(&out);
    assert_eq!(summary["reason"], "observer-not-ready");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["coverage"]["validBaseline"], false);
    assert!(!out.join("samples.jsonl").exists());
    assert!(!out.join("done").exists());
}

#[test]
fn startup_failure_preserves_the_primary_error() {
    let f = ObserveFixture::new();
    let out = f.root.join("out");
    let mut command = f.watch(2, 100, &out);
    command.env("FAKE_FAIL", "all");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("amc:"));
    // No summary was fabricated around a failed startup query.
    assert!(!out.join("summary.json").exists());
}

#[test]
fn attachment_identity_change_aborts_before_sampling() {
    let f = ObserveFixture::new();
    let out = f.root.join("out");
    let Some(mut command) = f.watch_own_cgroup(2, 100, &out) else {
        return;
    };
    // Startup (call 0) and version (call 1) report the attach invocation;
    // the attachment re-check (call 2) reports a different one.
    command
        .env("FAKE_INVOCATION_ALT", "1234567890abcdef1234567890abcdef")
        .env("FAKE_FLIP_AT", "2");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("identity changed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!out.join("summary.json").exists());
}

#[test]
fn cancellation_leaves_explicit_incomplete_evidence() {
    let f = ObserveFixture::new();
    let out = f.root.join("out2");
    let Some(mut cmd) = f.watch_own_cgroup(30, 20, &out) else {
        return;
    };
    let child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_lines(&out.join("samples.jsonl"), 3, Duration::from_secs(10));
    assert!(
        Command::new("sh")
            .args(["-c", &format!("kill -TERM {}", child.id())])
            .status()
            .unwrap()
            .success()
    );
    let output = wait_exit(child, Duration::from_secs(10));
    assert!(output.status.success(), "cancelled watch exits cleanly");
    let summary = f.summary(&out);
    assert_eq!(summary["reason"], "cancelled");
    assert_eq!(summary["complete"], false);
    assert!(out.join("done").exists(), "summary persisted, then marked");
}

#[test]
fn final_query_failure_keeps_prefix_marks_endpoint_unknown() {
    let f = ObserveFixture::new();
    let out = f.root.join("out");
    let Some(mut command) = f.watch_own_cgroup(2, 100, &out) else {
        return;
    };
    command.env("FAKE_FAIL_FINAL", "1");
    let output = command.output().unwrap();
    assert!(output.status.success());
    let summary = f.summary(&out);
    // Collection finished but the endpoint could not be confirmed: the
    // verified prefix stands, the terminal state does not borrow
    // `deadline`, and final counters stay unavailable.
    assert_eq!(summary["reason"], "endpoint-query-failed");
    assert_eq!(summary["complete"], false);
    assert!(summary["eventDeltas"].is_object());
    assert_eq!(summary["coverage"]["finalCountersUnavailable"], true);
    assert!(summary.get("managerResult").is_none_or(|v| v.is_null()));
}

#[test]
fn final_gap_restart_rewrites_deadline_through_real_watch() {
    // Sampling saw one lifetime; the independent final endpoint query
    // sees the next. The real watch binary must rewrite `deadline`,
    // suppress deltas to the terminal reason, and mark final counters
    // unavailable — never bridge two lifetimes even when counters
    // increased (which monotonicity alone cannot catch).
    let f = ObserveFixture::new();
    let out = f.root.join("out");
    let Some(mut command) = f.watch_own_cgroup(2, 100, &out) else {
        return;
    };
    command.env("FAKE_FINAL_INVOCATION", "1234567890abcdef1234567890abcdef");
    let output = command.output().unwrap();
    assert!(output.status.success());
    let summary = f.summary(&out);
    assert_eq!(summary["reason"], "invocation-changed");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["coverage"]["finalCountersUnavailable"], true);
    // A later restart must not erase independently verified earlier
    // evidence: with a successful pre-change poll the verified prefix
    // yields deltas; without one the terminal reason stands. Either way
    // no cross-lifetime bridge is reported and final stays unavailable.
    let has_deltas = summary.get("eventDeltas").is_some_and(|v| v.is_object());
    let reason = summary.get("deltaUnsupportedReason");
    if has_deltas {
        assert!(reason.is_none_or(|v| v.is_null()));
    } else {
        assert_eq!(
            reason,
            Some(&serde_json::Value::String("restart-detected".into()))
        );
    }
    // The collection itself is attempting vs persisted, not just readable:
    // both counters exist and persisted never exceeds attempted.
    let collection = &summary["collection"];
    assert!(
        collection["attemptedSamples"].as_u64().unwrap()
            >= collection["persistedSamples"].as_u64().unwrap()
    );
}

#[test]
fn write_exhaustion_finalizes_incomplete_evidence() {
    let f = ObserveFixture::new();
    let out = f.root.join("out");
    let Some(group) = readable_own_cgroup() else {
        eprintln!("SKIP: no readable cgroup telemetry in this environment");
        return;
    };
    // 80 x 512-byte blocks: the manifest fits, the sample stream does
    // not. Ignored SIGXFSZ turns the overflow into EFBIG write errors
    // instead of a fatal signal, exercising the real failure recording.
    let bin = env!("CARGO_BIN_EXE_amc");
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "trap '' XFSZ; ulimit -f 80; exec \"$@\"",
        "sh",
        bin,
        "watch",
        "test.service",
        "--seconds",
        "10",
        "--interval-ms",
        "20",
        "--output",
    ]);
    command.arg(&out);
    let mut paths = vec![f.root.clone()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
    command
        .env("PATH", env::join_paths(paths).unwrap())
        .env("FAKE_STATE", &f.root)
        .env("FAKE_CGROUP", group)
        .env("FAKE_INVOCATION", "abcdef0123456789abcdef0123456789");
    let output = command.output().unwrap();
    assert!(output.status.success());
    let summary = f.summary(&out);
    assert_eq!(summary["reason"], "output-write-failed");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["coverage"]["incompletePersistence"], true);
    assert_eq!(summary["coverage"]["validBaseline"], true);
    assert!(out.join("ready").exists(), "baseline persisted first");
    let collection = &summary["collection"];
    assert!(
        collection["persistedSamples"].as_u64().unwrap()
            < collection["attemptedSamples"].as_u64().unwrap()
    );
    assert_eq!(collection["storageDurable"], false);
}

#[test]
fn frozen_state_is_neither_pressure_nor_termination() {
    // An ancestor freezing mid-interval: the comparison path must report
    // the frozen/populated transitions as state, keep pressure and
    // counters on their own evidence, and never claim termination.
    let dir = env::temp_dir().join(format!(
        "amc-observe-frozen-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir).unwrap();
    let snapshot = |frozen: u64| {
        serde_json::json!({
            "schemaVersion": 1,
            "path": "/sys/fs/cgroup/app.slice/app-amc-proof@1.service",
            "observedUnixMs": 1,
            "observedMonotonicMs": 20,
            "sequence": 0,
            "invocationId": "inv-frozen",
            "bootId": "boot-1",
            "inode": 77,
            "files": {
                "memory.events": {"value": {"oom_kill": 0, "max": 1}, "unknown": null},
                "memory.events.local": {"value": {"oom_kill": 0, "max": 1}, "unknown": null},
                "cgroup.events": {"value": {"populated": 1, "frozen": frozen}, "unknown": null},
                "memory.current": {"value": 4096, "unknown": null},
                "memory.peak": {"value": 8192, "unknown": null},
                "memory.pressure": {"value": {
                    "some": {"avg10": 1.5, "avg60": 0.5, "avg300": 0.1, "total": 1000},
                    "full": {"avg10": 0.0, "avg60": 0.0, "avg300": 0.0, "total": 0}
                }, "unknown": null},
                "memory.swap.current": {"value": null, "unknown": "not-collected"},
                "memory.swap.peak": {"value": null, "unknown": "not-collected"},
                "memory.min": {"value": null, "unknown": "not-collected"},
                "memory.low": {"value": null, "unknown": "not-collected"},
                "memory.high": {"value": null, "unknown": "not-collected"},
                "memory.max": {"value": null, "unknown": "not-collected"},
                "memory.swap.max": {"value": null, "unknown": "not-collected"},
                "memory.oom.group": {"value": null, "unknown": "not-collected"}
            }
        })
    };
    fs::write(
        dir.join("before.json"),
        serde_json::to_string(&snapshot(0)).unwrap(),
    )
    .unwrap();
    fs::write(
        dir.join("after.json"),
        serde_json::to_string(&snapshot(1)).unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_amc"))
        .args(["diff"])
        .arg(dir.join("before.json"))
        .arg(dir.join("after.json"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let compared: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // Frozen is a state transition on the same evidence, not a counter.
    assert_eq!(compared["cgroup.events"]["frozen"]["kind"], "state");
    assert_eq!(
        compared["cgroup.events"]["frozen"]["reason"],
        "state-transition"
    );
    assert_eq!(compared["cgroup.events"]["frozen"]["delta"], 1);
    // Populated did not move: no termination signal anywhere.
    assert_eq!(
        compared["cgroup.events"]["populated"]["reason"],
        "unchanged"
    );
    assert!(!stdout.contains("terminat"));
    // Pressure averages are retained verbatim; the freeze adds no stall
    // claim and no counter delta.
    assert_eq!(compared["memory.pressure"]["some"]["reason"], "compatible");
    assert_eq!(compared["memory.events"]["oom_kill"]["delta"], 0);
    assert_eq!(compared["memory.events"]["max"]["delta"], 0);
    let _ = fs::remove_dir_all(&dir);
}
