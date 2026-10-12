use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "amc-cli-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let this = Self(root);
        this.script(
            "systemd-run",
            r#"
printf 'submit\n' >> "$FAKE_ROOT/calls"
printf SENSITIVE_CLIENT_STDOUT
printf SENSITIVE_CLIENT_STDERR >&2
case "$FAKE_SCENARIO" in
  reject) exit 1 ;;
  lost|cancel|hung) printf started > "$FAKE_ROOT/state" ;;
esac
case "$FAKE_SCENARIO" in
  lost) exit 1 ;;
  cancel|hung) exec sleep 60 ;;
  work-fail) printf failed > "$FAKE_ROOT/state"; exit 42 ;;
  detached) printf started > "$FAKE_ROOT/state"; exit 0 ;;
esac
"#,
        );
        this.script(
            "systemctl",
            r#"
printf '%s\n' "$*" >> "$FAKE_ROOT/queries"
case "$FAKE_SCENARIO" in
  hung) exec sleep 60 ;;
  diagnostic-fail) printf SENSITIVE_STDOUT; printf SENSITIVE_STDERR >&2; exit 1 ;;
esac
case " $* " in
  *' stop '*) printf stopped > "$FAKE_ROOT/state"; exit 0 ;;
  *' reset-failed '*) exit 0 ;;
esac
if test -f "$FAKE_ROOT/state"; then
  state=$(cat "$FAKE_ROOT/state")
  printf 'LoadState=loaded\nExecMainStartTimestampMonotonic=1\n'
  case "$state" in
    stopped) printf 'ActiveState=inactive\nResult=success\n' ;;
    failed) printf 'ActiveState=failed\nResult=exit-code\n' ;;
    *) printf 'ActiveState=active\nResult=success\n' ;;
  esac
  # Absent group: nothing runs there, so termination evidence succeeds.
  printf 'ControlGroup=/amc-cli-test-%s\n' "$FAKE_SCENARIO"
else
  printf 'LoadState=not-found\nActiveState=inactive\n'
fi
printf 'Description=SENSITIVE_DESCRIPTION\nEnvironment=SENSITIVE_ENV\nExecStart=SENSITIVE_ARGV\n'
"#,
        );
        this.script(
            "journalctl",
            "printf journal-called > \"$FAKE_ROOT/journal-called\"; printf SENSITIVE_JOURNAL",
        );
        fs::write(this.0.join("config.toml"), "version=1\n[profiles.test]\nslice='app.slice'\nmemory_max='64MiB'\nmemory_swap_max='0B'\n").unwrap();
        this
    }

    fn script(&self, name: &str, text: &str) {
        let shell = env::split_paths(&env::var_os("PATH").unwrap())
            .map(|p| p.join("sh"))
            .find(|p| p.is_file())
            .expect("pinned test environment needs sh");
        let path = self.0.join(name);
        fs::write(&path, format!("#!{}\n{text}\n", shell.display())).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self, scenario: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_amc"));
        let mut paths = vec![self.0.clone()];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap()));
        command
            .env("PATH", env::join_paths(paths).unwrap())
            .env("FAKE_ROOT", &self.0)
            .env("FAKE_SCENARIO", scenario);
        command
    }

    fn launch(&self, scenario: &str) -> Command {
        let mut command = self.command(scenario);
        command
            .arg("--config")
            .arg(self.0.join("config.toml"))
            .args(["launch", "--id", "test", "--profile", "test", "--unit-file"])
            .arg(self.0.join("unit"))
            .args(["--", "true"]);
        command
    }

    fn assert_owned_once(&self) {
        assert_eq!(
            fs::read_to_string(self.0.join("calls")).unwrap(),
            "submit\n"
        );
        let unit = fs::read_to_string(self.0.join("unit")).unwrap();
        for query in fs::read_to_string(self.0.join("queries"))
            .unwrap_or_default()
            .lines()
        {
            assert!(
                query.ends_with(unit.trim()),
                "query escaped attempt: {query}"
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bounded(mut command: Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("isolated CLI test exceeded deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn rejection_lost_reply_workload_failure_and_detach_do_not_resubmit() {
    for scenario in ["reject", "lost", "work-fail", "detached"] {
        let f = Fixture::new();
        let output = bounded(f.launch(scenario));
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(!text.contains("SENSITIVE"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SENSITIVE"));
        f.assert_owned_once();
        match scenario {
            "detached" => {
                assert!(output.status.success());
                assert_eq!(fs::read_to_string(f.0.join("state")).unwrap(), "started");
            }
            "work-fail" => {
                assert_eq!(output.status.code(), Some(42));
                assert!(text.contains("subsequently failed"));
            }
            _ => {
                assert!(!output.status.success());
                assert!(text.contains("UNKNOWN"));
            }
        }
    }
}

#[test]
fn cancellation_and_hung_reconciliation_are_bounded() {
    for (scenario, signal) in [("cancel", "INT"), ("cancel", "TERM"), ("hung", "TERM")] {
        let f = Fixture::new();
        let mut child = f
            .launch(scenario)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !f.0.join("state").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            Command::new("sh")
                .args(["-c", &format!("kill -{signal} {}", child.id())])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("cancellation hung");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.contains("cancelled"));
        if scenario == "hung" {
            assert!(text.contains("UNKNOWN"));
        }
        f.assert_owned_once();
    }
}

#[test]
fn inspection_never_reads_journals_or_reprints_error_streams() {
    for scenario in ["reject", "diagnostic-fail"] {
        let f = Fixture::new();
        let mut command = f.command(scenario);
        command.args(["inspect", "test.service", "--json"]);
        let output = bounded(command);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SENSITIVE"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SENSITIVE"));
        assert!(!f.0.join("journal-called").exists());
    }
}

#[test]
fn failed_client_spawn_publishes_identity_without_submission() {
    let f = Fixture::new();
    fs::write(f.0.join("systemd-run"), "#!/amc-nonexistent-interpreter\n").unwrap();
    let output = bounded(f.launch("reject"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("nothing submitted"));
    assert!(f.0.join("unit").exists());
    assert!(!f.0.join("calls").exists());
}

#[test]
fn admission_execution_deadline_rejects_invalid_values_before_contacting_server() {
    let f = Fixture::new();
    for value in ["0", "86401", "infinity", "-1"] {
        let mut command = f.command("reject");
        command.args([
            "admission",
            "exec",
            "--socket",
            "/amc-nonexistent/admission.sock",
            "--contract",
            "test",
            &format!("--runtime-max-sec={value}"),
            "--",
            "true",
        ]);
        let output = bounded(command);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("--runtime-max-sec"));
        assert!(!f.0.join("calls").exists());
    }
}

#[test]
fn delayed_admission_grants_cannot_submit_after_the_client_deadline() {
    use amc_admission::{
        ledger::{Contract, Entry, Phase},
        protocol::{Message, Request, Response, read_frame, write_frame},
    };
    use std::os::unix::net::UnixListener;

    for delayed_reply in ["enqueue", "poll"] {
        let f = Fixture::new();
        let socket = f.0.join("admission.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let mut entry = Entry {
                id: "deadline-ticket".into(),
                name: "test".into(),
                contract: Contract {
                    burst: false,
                    runtime_max_sec: None,
                    slice: "app.slice".into(),
                    memory_max: 1024,
                    memory_swap_max: 0,
                    max_running: 1,
                    pause_file: None,
                },
                phase: Phase::Queued,
                deadline_ms: amc_admission::server::now_ms().unwrap() + 30_000,
                identity: None,
                client: None,
            };
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: Request = read_frame(&mut stream).unwrap();
            assert!(matches!(
                request.message,
                Message::Enqueue { wait_ms: 1000, .. }
            ));
            if delayed_reply == "enqueue" {
                thread::sleep(Duration::from_millis(1200));
                entry.phase = Phase::Reserved;
            }
            write_frame(
                &mut stream,
                &Response {
                    version: 1,
                    entry_key: Some("private-key".into()),
                    entry: Some(entry.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
            if delayed_reply == "poll" {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request: Request = read_frame(&mut stream).unwrap();
                assert!(matches!(request.message, Message::Poll { id } if id == entry.id));
                thread::sleep(Duration::from_millis(1200));
                entry.phase = Phase::Reserved;
                write_frame(
                    &mut stream,
                    &Response {
                        version: 1,
                        entry: Some(entry.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: Request = read_frame(&mut stream).unwrap();
            assert!(matches!(request.message, Message::Cancel { id } if id == entry.id));
            write_frame(
                &mut stream,
                &Response {
                    version: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        });
        let mut command = f.command("reject");
        command
            .args(["admission", "exec", "--socket"])
            .arg(&socket)
            .args(["--contract", "test", "--timeout", "1", "--", "true"]);
        let output = bounded(command);
        server.join().unwrap();
        assert!(!output.status.success());
        assert!(
            !f.0.join("calls").exists(),
            "late {delayed_reply} reply submitted native work"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("cancelled or timed out before submission")
        );
    }
}

#[test]
fn report_renders_a_complete_capture_offline_and_rejects_corruption() {
    let f = Fixture::new();
    let capture = f.0.join("capture");
    fs::create_dir(&capture).unwrap();
    let target = serde_json::json!({
        "unit": "test.service", "cgroupPath": "/test.service", "invocationId": "inv-1"
    });
    let manifest = serde_json::json!({
        "schemaVersion": 1, "observationId": "obs-1", "bootId": "boot-1",
        "production": true, "seconds": 2, "intervalMs": 1000, "target": target
    });
    fs::write(capture.join("manifest.json"), manifest.to_string()).unwrap();
    let samples: Vec<String> = (0..2)
        .map(|seq| {
            serde_json::json!({
                "observationId": "obs-1", "clockDomain": "monotonic-clock",
                "target": target,
                "observation": {
                    "schemaVersion": 1, "sequence": seq, "observedMonotonicMs": seq * 1000,
                    "observedUnixMs": 1_700_000_000_000_u64 + seq * 1000,
                    "invocationId": "inv-1", "bootId": "boot-1", "inode": 10,
                    "path": "/test.service", "files": {
                        "memory.current": {"value": 1_073_741_824_u64 + seq * 1024, "unknown": null},
                        "memory.events": {"value": {"oom": 0, "max": seq}, "unknown": null},
                        "cgroup.events": {"value": {"populated": 1}, "unknown": null}
                    }
                },
                "host": {
                    "observedMonotonicMs": seq * 1000,
                    "observedUnixMs": 1_700_000_000_000_u64 + seq * 1000,
                    "files": {"meminfo.MemAvailable": {
                        "value": 8_000_000_000_u64 - seq * 1000, "unknown": null
                    }}
                },
                "scheduleLagMs": 0, "captureDurationUs": 100
            })
            .to_string()
        })
        .collect();
    let stream = format!("{}\n", samples.join("\n"));
    fs::write(capture.join("samples.jsonl"), &stream).unwrap();
    let summary = serde_json::json!({
        "schemaVersion": 1, "observationId": "obs-1", "target": target,
        "reason": "deadline", "complete": true,
        "coverage": {"validBaseline": true, "incompletePersistence": false},
        "eventDeltas": {"oom": 0, "max": 1},
        "collection": {"attemptedSamples": 2, "persistedSamples": 2,
                       "storageDurable": true, "bytesWritten": stream.len()}
    });
    fs::write(capture.join("summary.json"), summary.to_string()).unwrap();
    fs::write(capture.join("ready"), "").unwrap();
    fs::write(capture.join("done"), "").unwrap();

    let markdown = bounded({
        let mut command = f.command("reject");
        command.args(["report"]).arg(&capture);
        command
    });
    assert!(
        markdown.status.success(),
        "{}",
        String::from_utf8_lossy(&markdown.stderr)
    );
    let text = String::from_utf8_lossy(&markdown.stdout);
    assert!(text.contains("Target: test.service"), "{text}");
    assert!(
        text.contains("Collection: reason=deadline complete=True"),
        "{text}"
    );
    assert!(text.contains("Samples: 2 reconciled=True"), "{text}");
    assert!(text.contains("memory.current: 1.00 GiB"), "{text}");
    assert!(text.contains("oom=0"), "{text}");
    assert!(
        text.contains("Host MemAvailable (sampled): first"),
        "{text}"
    );
    assert!(text.contains("GiB"), "{text}");
    assert!(!text.contains("memAvailableBytes: {'first'"), "{text}");

    let json = bounded({
        let mut command = f.command("reject");
        command.args(["report"]).arg(&capture).arg("--json");
        command
    });
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["collection"]["complete"], true);
    assert_eq!(
        report["metrics"]["memAvailableBytes"]["min"],
        7_999_999_000_u64
    );
    assert!(!f.0.join("queries").exists());

    fs::write(capture.join("samples.jsonl"), "malformed\n").unwrap();
    let rejected = bounded({
        let mut command = f.command("reject");
        command.args(["report"]).arg(&capture);
        command
    });
    assert_eq!(rejected.status.code(), Some(2));
    assert!(!f.0.join("queries").exists());
}
