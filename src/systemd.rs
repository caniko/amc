use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Profile;
use crate::control::capture;
pub use amc_runner::systemd::inspect::{
    cgroup_directory, inspect, leaf_status_with_timeout, manager_version, parse_properties,
    parse_unified_cgroup, resolve_executable, validate_unit_public,
};

pub const SCOPE_STATEMENT: &str = "only processes executing inside the unit's cgroup are governed; work brokered by another daemon is not";
pub const UNIT_PATTERN: &str = "app-amc-<safe-app-id>-<stable-hash>@<random>.service";

const BASE_ENVIRONMENT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "COLORTERM",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "SSH_AUTH_SOCK",
    "PYTHONPATH",
    "LD_LIBRARY_PATH",
];

// Nix variables are listed individually so broad NIX_* inheritance cannot leak access tokens.
const NIX_ENVIRONMENT: &[&str] = &[
    "NIX_PATH",
    "NIX_PROFILES",
    "NIX_SSL_CERT_FILE",
    "NIX_USER_PROFILE_DIR",
    "NIX_REMOTE",
    "IN_NIX_SHELL",
];

#[derive(Debug, Clone, Copy)]
pub enum StartMode {
    Run { pty: bool },
    Launch,
}

pub struct StartSpec<'a> {
    pub id: &'a str,
    pub profile: &'a Profile,
    pub unit: &'a str,
    pub retain: bool,
    pub runtime_max_sec: Option<u64>,
    pub environment: &'a [String],
    pub argv: &'a [String],
}

pub fn start(spec: &StartSpec<'_>, mode: StartMode) -> Result<i32> {
    use amc_runner::systemd::{NotSubmitted, Outcome};
    eprintln!("AMC unit: {}", spec.unit);
    let systemd_run = resolve_executable("systemd-run")
        .map_err(|_| anyhow::anyhow!("systemd-run unavailable; submission was not attempted"))?;
    let signals = crate::control::Signals::install()?;
    let mut command = Command::new(systemd_run);
    command.args(systemd_arguments(spec, mode));
    let manager = resolve_executable("systemctl")?;
    let record = amc_runner::systemd::ClientRecord::default();
    let outcome = amc_runner::systemd::execute(
        &mut command,
        &manager,
        spec.unit,
        matches!(mode, StartMode::Launch),
        spec.retain,
        || signals.cancelled(),
        &record,
    );
    match outcome {
        Outcome::NotSubmitted(NotSubmitted::CommandMismatch) => {
            bail!("prepared command does not match managed identity/mode; nothing submitted")
        }
        Outcome::NotSubmitted(NotSubmitted::SpawnFailed) => {
            bail!("systemd-run client spawn failed; nothing submitted")
        }
        Outcome::NotSubmitted(NotSubmitted::InvalidUnit) => {
            bail!("invalid AMC service identity; nothing submitted")
        }
        Outcome::NotSubmitted(NotSubmitted::Cancelled(signal)) => {
            eprintln!("Cancelled before submission; nothing submitted");
            Ok(128 + signal)
        }
        Outcome::Acknowledged => {
            eprintln!("Submission acknowledged; detached unit remains owned by the manager");
            Ok(0)
        }
        Outcome::Completed(code) => {
            if code == 0 {
                eprintln!("Submission and workload completed successfully");
            } else {
                eprintln!(
                    "Submission started a workload that subsequently failed (client exit {code})"
                );
            }
            Ok(code)
        }
        Outcome::Cancelled {
            signal,
            started,
            cleanup,
        } => {
            report_interrupted(spec.unit, "cancelled", started, cleanup);
            Ok(128 + signal)
        }
        Outcome::TimedOut { started, cleanup } => {
            report_interrupted(spec.unit, "startup deadline exceeded", started, cleanup);
            Ok(124)
        }
        Outcome::Unknown { exit_code, cleanup } => {
            eprintln!(
                "Submission rejected or outcome UNKNOWN (client exit {exit_code}); {}",
                cleanup_report(spec.unit, cleanup)
            );
            Ok(exit_code)
        }
    }
}

fn report_interrupted(
    unit: &str,
    reason: &str,
    started: bool,
    cleanup: amc_runner::systemd::Cleanup,
) {
    eprintln!(
        "Launch {reason} {}; {}",
        if started {
            "after observed start"
        } else {
            "before acknowledgment (submission may have been accepted)"
        },
        cleanup_report(unit, cleanup)
    );
}

fn cleanup_report(unit: &str, cleanup: amc_runner::systemd::Cleanup) -> String {
    if cleanup == amc_runner::systemd::Cleanup::Stopped {
        format!("owned unit {unit} stopped; work may have run, never replay automatically")
    } else {
        format!(
            "UNKNOWN: cleanup not confirmed for {unit}; unit may be absent/collected or manager unavailable. Inspect this identity before any retry: systemctl --user show {unit}"
        )
    }
}

/// Disposable-fixture properties only (RFC 0.4 Appendix A shape).
/// Never applied to real application units: those use native drop-ins and
/// keep their own OOMPolicy/KillMode/Restart/Delegate/service-type.
pub fn systemd_arguments(spec: &StartSpec<'_>, mode: StartMode) -> Vec<String> {
    let mut args = vec![
        "--user".into(),
        "--service-type=exec".into(),
        "--expand-environment=no".into(),
        "--same-dir".into(),
        "--quiet".into(),
        format!("--unit={}", spec.unit),
        format!("--property=Description=AMC fixture: {}", spec.id),
        format!("--property=Slice={}", spec.profile.slice),
        "--property=MemoryAccounting=yes".into(),
        format!("--property=MemoryMax={}", spec.profile.memory_max_bytes),
        format!(
            "--property=MemorySwapMax={}",
            spec.profile.memory_swap_max_bytes
        ),
        "--property=OOMPolicy=kill".into(),
        // Fixtures must not acquire automatic restarts; unknown replay paths
        // must never restart agent actions or package operations.
        "--property=Restart=no".into(),
        "--property=TimeoutStartSec=30s".into(),
    ];
    // Optional fixture throttling threshold; native precedence is not inferred
    // from the submitted properties.
    if let Some(high) = spec.profile.memory_high_bytes {
        args.push(format!("--property=MemoryHigh={high}"));
    }
    if let Some(seconds) = spec.runtime_max_sec {
        args.push(format!("--property=RuntimeMaxSec={seconds}s"));
    }
    if !spec.retain {
        args.push("--collect".into());
    }
    match mode {
        StartMode::Run { pty } => {
            args.push("--wait".into());
            args.push(if pty { "--pty" } else { "--pipe" }.into());
        }
        StartMode::Launch => {}
    }
    args.extend(
        spec.environment
            .iter()
            .map(|name| format!("--setenv={name}")),
    );
    args.push("--".into());
    args.extend(spec.argv.iter().cloned());
    args
}

pub fn all_stdio_are_terminals() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

pub fn inherited_environment(extra: &[String]) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    for name in BASE_ENVIRONMENT.iter().chain(NIX_ENVIRONMENT) {
        if env::var_os(name).is_some() {
            names.insert((*name).to_owned());
        }
    }
    for (name, _) in env::vars_os() {
        if let Some(name) = name.to_str()
            && name.starts_with("LC_")
            && valid_environment_name(name)
        {
            names.insert(name.to_owned());
        }
    }
    for name in extra {
        if !valid_environment_name(name) {
            bail!("invalid environment variable name {name:?}");
        }
        if env::var_os(name).is_some() {
            names.insert(name.clone());
        } else {
            bail!("environment variable {name:?} is not set");
        }
    }
    Ok(names.into_iter().collect())
}

pub use amc_runner::systemd::valid_environment_name;

pub fn resolve_command(argv: &[String]) -> Result<Vec<String>> {
    let (program, rest) = argv
        .split_first()
        .context("a command is required after --")?;
    let executable = resolve_executable(program)?;
    let mut resolved = Vec::with_capacity(argv.len());
    resolved.push(executable.to_string_lossy().into_owned());
    resolved.extend(rest.iter().cloned());
    Ok(resolved)
}

pub fn unit_name(id: &str) -> Result<String> {
    let mut random = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .context("failed to open /dev/urandom")?
        .read_exact(&mut random)
        .context("failed to read /dev/urandom")?;
    Ok(unit_name_with_nonce(id, u64::from_ne_bytes(random)))
}

fn unit_name_with_nonce(id: &str, nonce: u64) -> String {
    let mut safe = String::with_capacity(48);
    let mut last_dash = false;
    for character in id.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_alphanumeric() {
            safe.push(character);
            last_dash = false;
        } else if !last_dash && !safe.is_empty() {
            safe.push('-');
            last_dash = true;
        }
        if safe.len() >= 48 {
            break;
        }
    }
    while safe.ends_with('-') {
        safe.pop();
    }
    if safe.is_empty() {
        safe.push_str("app");
    }
    format!(
        "app-amc-{safe}-{:08x}@{nonce:016x}.service",
        stable_hash(id) as u32
    )
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub available: bool,
    pub checks: Vec<DoctorCheck>,
    pub nix_boundary: NixBoundary,
    pub scope_statement: &'static str,
}

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub ok: bool,
    pub required: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NixBoundary {
    pub multi_user_daemon_detected: bool,
    pub detail: String,
}

pub fn doctor() -> DoctorReport {
    let mut checks = Vec::new();
    let controllers = fs::read_to_string("/sys/fs/cgroup/cgroup.controllers");
    checks.push(DoctorCheck {
        name: "cgroup-v2",
        ok: controllers.is_ok(),
        required: true,
        detail: match &controllers {
            Ok(_) => "/sys/fs/cgroup/cgroup.controllers is readable".into(),
            Err(error) => error.to_string(),
        },
    });
    checks.push(DoctorCheck {
        name: "memory-controller",
        ok: controllers.as_deref().is_ok_and(has_memory_controller),
        required: true,
        detail: controllers
            .as_deref()
            .map(str::trim)
            .unwrap_or("controller list unavailable")
            .to_owned(),
    });

    let systemctl = resolve_executable("systemctl");
    let systemd_run = resolve_executable("systemd-run");
    let sleep = resolve_executable("sleep");
    checks.push(DoctorCheck {
        name: "systemd-run",
        ok: systemd_run.is_ok(),
        required: true,
        detail: result_path_detail(&systemd_run),
    });
    let user_manager = systemctl.as_ref().ok().map(|program| {
        capture(Command::new(program).args(["--user", "show", "--property=Version"]))
    });
    checks.push(DoctorCheck {
        name: "user-manager",
        ok: user_manager.as_ref().is_some_and(Result::is_ok),
        required: true,
        detail: match user_manager.as_ref() {
            Some(Ok(_)) => "user manager responded".into(),
            Some(Err(_)) => "user manager query failed or timed out".into(),
            None => result_path_detail(&systemctl),
        },
    });

    if let (Ok(systemd_run), Ok(systemctl), Ok(sleep)) = (systemd_run, systemctl, sleep)
        && user_manager.is_some_and(|output| output.is_ok())
    {
        match probe_transient_service(&systemd_run, &systemctl, &sleep) {
            Ok(evidence) => {
                checks.push(DoctorCheck {
                    name: "transient-properties",
                    ok: evidence.properties_ok,
                    required: true,
                    detail: evidence.property_detail,
                });
                // Fixture-only: OOMPolicy=kill must produce memory.oom.group=1
                // in the kernel. This is not a MemoryOOMGroup property probe
                // (no such property exists); it verifies kernel state.
                checks.push(DoctorCheck {
                    name: "oom-policy-group",
                    ok: evidence.oom_group == "1",
                    required: true,
                    detail: format!(
                        "fixture cgroupfs memory.oom.group={} (set by OOMPolicy=kill, not a MemoryOOMGroup property)",
                        evidence.oom_group
                    ),
                });
            }
            Err(error) => checks.push(DoctorCheck {
                name: "transient-properties",
                ok: false,
                required: true,
                detail: format!("{error:#}"),
            }),
        }
    } else {
        checks.push(DoctorCheck {
            name: "transient-properties",
            ok: false,
            required: true,
            detail: "probe skipped because systemd-run, systemctl, sleep, or the user manager is unavailable"
                .into(),
        });
    }

    let multi_user_daemon_detected = Path::new("/nix/var/nix/daemon-socket/socket").exists();
    let nix_boundary = NixBoundary {
        multi_user_daemon_detected,
        detail: if multi_user_daemon_detected {
            "multi-user Nix detected: derivation builders run under nix-daemon and outside an AMC user service"
                .into()
        } else {
            "Nix daemon socket not detected; other broker configurations are unknown".into()
        },
    };
    let available = checks.iter().all(|check| !check.required || check.ok);
    DoctorReport {
        available,
        checks,
        nix_boundary,
        scope_statement: SCOPE_STATEMENT,
    }
}

fn result_path_detail(result: &Result<PathBuf>) -> String {
    match result {
        Ok(path) => path.display().to_string(),
        Err(error) => error.to_string(),
    }
}

fn has_memory_controller(controllers: &str) -> bool {
    controllers
        .split_ascii_whitespace()
        .any(|item| item == "memory")
}

struct ProbeEvidence {
    properties_ok: bool,
    property_detail: String,
    oom_group: String,
}

fn probe_transient_service(
    systemd_run: &Path,
    systemctl: &Path,
    sleep: &Path,
) -> Result<ProbeEvidence> {
    let unit = unit_name("doctor-probe")?;
    let expected_memory_max = 64_u64 << 20;
    let profile = Profile {
        slice: "app.slice".into(),
        memory_max_bytes: expected_memory_max,
        memory_swap_max_bytes: 0,
        memory_high_bytes: None,
    };
    let argv = vec![sleep.to_string_lossy().into_owned(), "20".into()];
    let spec = StartSpec {
        id: "doctor-probe",
        profile: &profile,
        unit: &unit,
        retain: false,
        runtime_max_sec: Some(25),
        environment: &[],
        argv: &argv,
    };
    let result = (|| {
        capture(Command::new(systemd_run).args(systemd_arguments(&spec, StartMode::Launch)))?;
        let property_names = [
            "MemoryAccounting",
            "MemoryMax",
            "MemorySwapMax",
            "OOMPolicy",
            "Restart",
            "Slice",
            "ControlGroup",
        ];
        let mut command = Command::new(systemctl);
        command.args(["--user", "show", "--no-pager"]);
        for property in property_names {
            command.arg(format!("--property={property}"));
        }
        let output = capture(command.arg("--").arg(&unit))?;
        let properties = parse_properties(&output);
        validate_probe_properties(&properties, expected_memory_max)?;
        let control_group = properties
            .get("ControlGroup")
            .context("systemctl did not report ControlGroup")?;
        let cgroup = cgroup_directory(control_group)?;
        let memory_max = crate::helpers::read_value(cgroup.join("memory.max"))?;
        let memory_swap_max = crate::helpers::read_value(cgroup.join("memory.swap.max"))?;
        let oom_group = crate::helpers::read_value(cgroup.join("memory.oom.group"))?;
        let cgroup_ok = memory_max == expected_memory_max.to_string() && memory_swap_max == "0";
        Ok(ProbeEvidence {
            properties_ok: cgroup_ok,
            property_detail: format!(
                "systemd properties accepted; cgroupfs memory.max={memory_max}, memory.swap.max={memory_swap_max}"
            ),
            oom_group,
        })
    })();

    let stopped = capture(
        Command::new(systemctl)
            .args(["--user", "stop", "--"])
            .arg(&unit),
    );
    let _ = capture(
        Command::new(systemctl)
            .args(["--user", "reset-failed", "--"])
            .arg(&unit),
    );
    if stopped.is_err() {
        bail!(
            "probe cleanup UNKNOWN for {unit}; inspect this identity (fixture runtime bound: 25 seconds)"
        );
    }
    result
}

fn validate_probe_properties(properties: &BTreeMap<String, String>, memory_max: u64) -> Result<()> {
    let expected = [
        ("MemoryAccounting", "yes".to_owned()),
        ("MemoryMax", memory_max.to_string()),
        ("MemorySwapMax", "0".to_owned()),
        ("OOMPolicy", "kill".to_owned()),
        ("Restart", "no".to_owned()),
        ("Slice", "app.slice".to_owned()),
    ];
    for (name, value) in expected {
        match properties.get(name) {
            Some(actual) if actual == &value => {}
            Some(_) => bail!("property {name} does not match the requested fixture value"),
            None => bail!("property {name} is missing"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use amc_runner::systemd::inspect::validate_unit;
    use std::time::Duration;

    #[test]
    fn diagnostic_units_accept_scopes_without_accepting_paths_or_arguments() {
        for unit in ["app-game.scope", "background-worker.service"] {
            assert!(validate_unit(unit).is_ok());
        }
        for unit in [
            "",
            "app.slice",
            "../app.scope",
            "a/b.service",
            "a.scope --system",
            "a.scope\n",
            "a.scope\0",
        ] {
            assert!(validate_unit(unit).is_err());
        }
    }

    #[test]
    fn diagnostic_properties_omit_free_form_text() {
        let properties = parse_properties(
            "Description=SENSITIVE_DESCRIPTION\nEnvironment=SENSITIVE_ENV\nExecStart=SENSITIVE_ARGV\nMemoryMax=SENSITIVE_INVALID\nResult=SENSITIVE_ERROR\n",
        );
        assert!(
            !serde_json::to_string(&properties)
                .unwrap()
                .contains("SENSITIVE")
        );
    }

    #[test]
    fn leaf_status_rejects_non_allowlisted_properties_without_spawning() {
        // Rejection happens before any subprocess: no manager interaction,
        // no attacker-controlled property name forwarded to systemctl.
        let program = Path::new("/nonexistent/systemctl");
        let error = leaf_status_with_timeout(
            program,
            "app-amc-test@1.service",
            false,
            &["ExecStart"],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("allowlist"));
        let error = leaf_status_with_timeout(
            program,
            "app-amc-test@1.service",
            false,
            &["Result"],
            Duration::from_secs(1),
        )
        .unwrap_err();
        // Allowlisted names pass validation and fail only on the missing
        // program, proving the check order.
        assert!(!format!("{error:#}").contains("allowlist"));
    }

    #[test]
    fn leaf_status_zero_timeout_fails_without_spawning() {
        // An exhausted deadline never spawns a client, even for an
        // allowlisted property on an existing program.
        let error = leaf_status_with_timeout(
            Path::new("/bin/true"),
            "app-amc-test@1.service",
            false,
            &["Result"],
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("deadline exhausted"));
    }

    #[test]
    fn manager_version_never_fails_the_caller() {
        // Best-effort metadata: an unreachable manager yields an explicit
        // unknown marker, never an error that aborts observation.
        let version = manager_version(Path::new("/nonexistent/systemctl"), false);
        assert!(version.starts_with("unknown"));
        assert!(!version.contains('\n'));
    }

    #[test]
    fn vm_assertions_use_properties_and_explicit_work_reports() {
        let vm = include_str!("../nix/vm-test.py");
        assert!(vm.contains("properties = json.dumps(explanation[\"systemdProperties\"])"));
        for arm in ["a", "b", "c"] {
            assert!(vm.contains(&format!(
                "report = json.loads(machine.succeed(\"cat /tmp/work-{arm}.json\"))"
            )));
        }
    }

    fn profile() -> Profile {
        Profile {
            slice: "app-amc.slice".into(),
            memory_max_bytes: 268_435_456,
            memory_swap_max_bytes: 33_554_432,
            memory_high_bytes: None,
        }
    }

    #[test]
    fn unit_names_are_safe_unique_and_stably_hashed() {
        let first = unit_name_with_nonce("AI weird/应用", 1);
        let second = unit_name_with_nonce("AI weird/应用", 2);
        assert_ne!(first, second);
        assert!(first.starts_with("app-amc-ai-weird-"));
        assert!(first.ends_with("@0000000000000001.service"));
        assert!(
            first
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'@' | b'.') })
        );
        assert_eq!(stable_hash("ai.opencode"), stable_hash("ai.opencode"));
        assert_ne!(stable_hash("a/b"), stable_hash("a b"));
    }

    #[test]
    fn environment_names_are_strict() {
        for valid in ["PATH", "LC_ALL", "_PRIVATE", "A1"] {
            assert!(valid_environment_name(valid));
        }
        for invalid in ["", "1A", "A=B", "A-B", "å"] {
            assert!(!valid_environment_name(invalid));
        }
    }

    #[test]
    fn command_construction_preserves_argv_and_omits_rejected_property() {
        let argv = vec![
            "/bin/example".into(),
            "space here".into(),
            "'quotes'".into(),
            "应用".into(),
            "$LITERAL".into(),
        ];
        let environment = vec!["PATH".into(), "LANG".into()];
        let profile = profile();
        let spec = StartSpec {
            id: "ai.opencode",
            profile: &profile,
            unit: "app-amc-ai-opencode-deadbeef@1.service",
            retain: false,
            runtime_max_sec: None,
            environment: &environment,
            argv: &argv,
        };
        let args = systemd_arguments(&spec, StartMode::Run { pty: false });
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(&args[separator + 1..], argv);
        assert!(args.contains(&"--pipe".to_owned()));
        assert!(args.contains(&"--collect".to_owned()));
        assert!(args.contains(&"--expand-environment=no".to_owned()));
        assert!(args.contains(&"--property=OOMPolicy=kill".to_owned()));
        // Fixtures must not restart; KillMode is shutdown handling, not OOM policy.
        assert!(args.contains(&"--property=Restart=no".to_owned()));
        assert!(!args.iter().any(|arg| arg.contains("KillMode")));
        assert!(!args.iter().any(|arg| arg.contains("MemoryOOMGroup")));
        assert!(!args.iter().any(|arg| arg.contains("MemoryHigh")));
        // Optional native throttling threshold is emitted only when configured.
        let high_profile = Profile {
            slice: profile.slice.clone(),
            memory_max_bytes: profile.memory_max_bytes,
            memory_swap_max_bytes: profile.memory_swap_max_bytes,
            memory_high_bytes: Some(134_217_728),
        };
        let high_spec = StartSpec {
            id: spec.id,
            profile: &high_profile,
            unit: spec.unit,
            retain: spec.retain,
            runtime_max_sec: spec.runtime_max_sec,
            environment: spec.environment,
            argv: spec.argv,
        };
        let high_args = systemd_arguments(&high_spec, StartMode::Run { pty: false });
        assert!(high_args.contains(&"--property=MemoryHigh=134217728".to_owned()));
    }

    #[test]
    fn doctor_helpers_detect_missing_capabilities() {
        assert!(has_memory_controller("cpu io memory pids"));
        assert!(!has_memory_controller("cpu io pids"));
        let mut properties = BTreeMap::new();
        properties.insert("MemoryAccounting".into(), "yes".into());
        assert!(validate_probe_properties(&properties, 1024).is_err());
    }

    #[test]
    fn parses_unified_cgroup_path() {
        assert_eq!(
            parse_unified_cgroup("0::/user.slice/user-1000.slice/app.slice/test.service\n")
                .unwrap(),
            "/user.slice/user-1000.slice/app.slice/test.service"
        );
        assert!(parse_unified_cgroup("1:name=/legacy\n").is_err());
        assert!(parse_unified_cgroup("0::relative\n").is_err());
    }
}
