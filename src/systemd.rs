use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{IsTerminal, Read},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Profile;
use crate::control::capture;

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

pub fn resolve_executable(program: &str) -> Result<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            env::current_dir()
                .context("failed to read current directory")?
                .join(path)
        };
        return executable_file(absolute, program);
    }

    let path_var = env::var_os("PATH").context("PATH is not set")?;
    for directory in env::split_paths(&path_var) {
        let directory = if directory.as_os_str().is_empty() {
            env::current_dir().context("failed to read current directory")?
        } else if directory.is_absolute() {
            directory
        } else {
            env::current_dir()
                .context("failed to read current directory")?
                .join(directory)
        };
        let candidate = directory.join(program);
        if let Ok(metadata) = candidate.metadata()
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            return Ok(candidate);
        }
    }
    bail!("executable {program:?} was not found in PATH")
}

fn executable_file(path: PathBuf, original: &str) -> Result<PathBuf> {
    let metadata = path
        .metadata()
        .with_context(|| format!("executable {original:?} does not exist"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("path {original:?} is not an executable file");
    }
    Ok(path)
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

fn parse_properties(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter_map(|(name, value)| {
            if matches!(name, "MemoryCurrent" | "MemoryPeak" | "MemorySwapCurrent")
                && (value == "infinity" || value.parse::<u64>().ok() == Some(u64::MAX))
            {
                return Some((
                    name.to_owned(),
                    "unknown (unavailable manager counter)".into(),
                ));
            }
            let valid = match name {
                "MemoryAccounting" | "Delegate" | "RemainAfterExit" => {
                    matches!(value, "yes" | "no")
                }
                "MemoryCurrent"
                | "MemoryPeak"
                | "MemorySwapCurrent"
                | "MemoryMax"
                | "MemoryHigh"
                | "MemorySwapMax"
                | "MemoryLow"
                | "MemoryMin"
                | "ExecMainCode"
                | "ExecMainStatus"
                | "ExecMainStartTimestampMonotonic" => {
                    value == "infinity" || value.parse::<u64>().is_ok()
                }
                "LoadState" => matches!(
                    value,
                    "loaded" | "not-found" | "error" | "bad-setting" | "masked" | "merged" | "stub"
                ),
                "ActiveState" => matches!(
                    value,
                    "active" | "reloading" | "inactive" | "failed" | "activating" | "deactivating"
                ),
                "SubState" => matches!(
                    value,
                    "running"
                        | "exited"
                        | "dead"
                        | "failed"
                        | "start"
                        | "start-pre"
                        | "start-post"
                        | "stop"
                        | "stop-sigterm"
                        | "stop-sigkill"
                        | "auto-restart"
                ),
                "Result" => matches!(
                    value,
                    "success"
                        | "exit-code"
                        | "signal"
                        | "core-dump"
                        | "timeout"
                        | "oom-kill"
                        | "resources"
                        | "protocol"
                        | "start-limit-hit"
                ),
                "OOMPolicy" => matches!(value, "continue" | "stop" | "kill"),
                "ManagedOOMSwap" | "ManagedOOMMemoryPressure" => matches!(value, "auto" | "kill"),
                "MemoryPressureWatch" => matches!(value, "auto" | "on" | "off" | "skip"),
                "Restart" => matches!(
                    value,
                    "no" | "always"
                        | "on-success"
                        | "on-failure"
                        | "on-abnormal"
                        | "on-abort"
                        | "on-watchdog"
                ),
                "KillMode" => matches!(value, "control-group" | "mixed" | "process" | "none"),
                "Type" => matches!(
                    value,
                    "simple"
                        | "exec"
                        | "forking"
                        | "oneshot"
                        | "dbus"
                        | "notify"
                        | "notify-reload"
                        | "idle"
                ),
                "Id" | "Slice" | "Names" => value.split_whitespace().all(|name| {
                    name.len() <= 255
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-@:\\".contains(&b))
                }),
                "ControlGroup" | "FragmentPath" | "DropInPaths" => {
                    value.is_empty()
                        || (value.starts_with('/')
                            && value.len() <= 4096
                            && !value.chars().any(char::is_control))
                }
                "InvocationID" => {
                    // systemd InvocationID: 32 lowercase hex chars, empty when
                    // the unit never ran in this boot. Reject anything else
                    // rather than echoing it.
                    value.is_empty()
                        || (value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()))
                }
                _ => return None,
            };
            Some((
                name.to_owned(),
                if valid {
                    value.to_owned()
                } else {
                    "unknown (malformed or unsupported value)".into()
                },
            ))
        })
        .collect()
}

pub fn parse_unified_cgroup(text: &str) -> Result<String> {
    let paths: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ':');
            match (fields.next(), fields.next(), fields.next()) {
                (Some("0"), Some(""), Some(path)) => Some(path),
                _ => None,
            }
        })
        .collect();
    match paths.as_slice() {
        [path] if path.starts_with('/') => Ok((*path).to_owned()),
        [] => bail!("no unified cgroup v2 entry found"),
        _ => bail!("multiple or invalid unified cgroup v2 entries found"),
    }
}

pub fn cgroup_directory(control_group: &str) -> Result<PathBuf> {
    if !control_group.starts_with('/') {
        bail!("cgroup path is not absolute");
    }
    let relative = Path::new(control_group.trim_start_matches('/'));
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        && !relative.as_os_str().is_empty()
    {
        bail!("unsafe cgroup path {control_group:?}");
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub schema_version: u32,
    pub unit: String,
    pub manager_context: &'static str,
    pub observed_unix_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    pub requested: &'static str,
    /// Manager-reported (systemctl show) settings.
    pub properties: BTreeMap<String, String>,
    pub kernel: Vec<crate::telemetry::Snapshot>,
    pub ancestors: Vec<ManagerMetadata>,
    /// Notes about absent, stale, or disappeared evidence.
    pub evidence_notes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerMetadata {
    unit: String,
    context: &'static str,
    observed_unix_ms: Option<u128>,
    properties: Option<BTreeMap<String, String>>,
    unknown: Option<&'static str>,
}

const MANAGER_PROPERTIES: &[&str] = &[
    "Id",
    "Names",
    "FragmentPath",
    "DropInPaths",
    "LoadState",
    "ActiveState",
    "SubState",
    "Result",
    "InvocationID",
    "ExecMainCode",
    "ExecMainStatus",
    "Slice",
    "MemoryAccounting",
    "MemoryCurrent",
    "MemoryPeak",
    "MemoryMax",
    "MemoryHigh",
    "MemoryLow",
    "MemoryMin",
    "MemorySwapCurrent",
    "MemorySwapMax",
    "OOMPolicy",
    "KillMode",
    "Restart",
    "Type",
    "Delegate",
    "ControlGroup",
    "ManagedOOMSwap",
    "ManagedOOMMemoryPressure",
    "MemoryPressureWatch",
];

/// Query a bounded subset of manager properties for one unit. Every
/// requested name must be in the allowlist; anything else is rejected
/// rather than forwarded to the manager.
/// Leaf-only manager query bounded by the caller's remaining deadline.
/// `timeout` is clamped to the query bounds by the transport; a zero
/// timeout fails without spawning.
pub fn leaf_status_with_timeout(
    program: &Path,
    unit: &str,
    system: bool,
    properties: &[&str],
    timeout: Duration,
) -> Result<BTreeMap<String, String>> {
    validate_unit(unit)?;
    for name in properties {
        if !MANAGER_PROPERTIES.contains(name) {
            bail!("property {name} is not in the diagnostic allowlist");
        }
    }
    let mut command = Command::new(program);
    command.args([
        if system { "--system" } else { "--user" },
        "show",
        "--no-pager",
    ]);
    for property in properties {
        command.arg(format!("--property={property}"));
    }
    let output = crate::control::capture_with_timeout(command.arg("--").arg(unit), timeout)?;
    let mut parsed = parse_properties(&output);
    for property in properties {
        parsed
            .entry(property.to_string())
            .or_insert_with(|| "unknown (not reported)".into());
    }
    Ok(parsed)
}

/// Manager version from `systemctl --version` (first line, bounded).
/// Best-effort: returns `"unknown (...)"` rather than failing the caller.
pub fn manager_version(program: &Path, system: bool) -> String {
    let mut command = Command::new(program);
    command.args([
        if system { "--system" } else { "--user" },
        "--version",
        "--no-pager",
    ]);
    match capture(&mut command) {
        Ok(output) => {
            let first = output.lines().next().unwrap_or("").trim();
            if first.is_empty() || first.len() > 128 || first.chars().any(char::is_control) {
                "unknown (malformed version output)".to_string()
            } else {
                first.to_string()
            }
        }
        Err(_) => "unknown (manager version unavailable)".to_string(),
    }
}

fn metadata(program: &Path, unit: &str, system: bool) -> Result<BTreeMap<String, String>> {
    let mut command = Command::new(program);
    command.args([
        if system { "--system" } else { "--user" },
        "show",
        "--no-pager",
    ]);
    for property in MANAGER_PROPERTIES {
        command.arg(format!("--property={property}"));
    }
    let output = capture(command.arg("--").arg(unit))?;
    let mut properties = parse_properties(&output);
    let missing = properties.get("LoadState").map(String::as_str) == Some("not-found");
    for property in MANAGER_PROPERTIES {
        if missing && !matches!(*property, "Id" | "Names" | "LoadState") {
            properties.insert(
                property.to_string(),
                "unknown (unit absent or collected)".into(),
            );
        } else {
            properties
                .entry(property.to_string())
                .or_insert_with(|| "unknown (not reported)".into());
        }
    }
    Ok(properties)
}

pub fn inspect(unit: &str, system: bool) -> Result<Inspection> {
    validate_unit(unit)?;
    let systemctl = resolve_executable("systemctl")?;
    let observed_unix_ms = crate::telemetry::now();
    let deadline = Instant::now() + Duration::from_secs(10);
    let properties = metadata(&systemctl, unit, system)?;
    let cgroup_path = properties
        .get("ControlGroup")
        .filter(|path| path.starts_with('/'))
        .cloned();
    let mut kernel = Vec::new();
    let mut ancestors = Vec::new();
    let mut evidence_notes = vec![
        "Traversal ends at visible /sys/fs/cgroup root; namespace-hidden ancestry is unknown. Snapshots are sequential, not atomic.".into(),
        "Known external domains: separately launched OpenCode frontends/backends, Nix/Lix daemon builders, containers and remote work are not inferred from client placement.".into(),
    ];
    if let Some(path) = &cgroup_path {
        if cgroup_directory(path).is_ok() {
            for (index, ancestor) in Path::new(path).ancestors().enumerate() {
                if index >= 64 {
                    evidence_notes.push(
                        "Traversal limited to 64 visible cgroups; remaining ancestors unknown"
                            .into(),
                    );
                    break;
                }
                let name = ancestor.to_string_lossy();
                let directory = cgroup_directory(&name)?;
                kernel.push(crate::telemetry::snapshot(&directory, &name));
                if index == 0 || name == "/" {
                    continue;
                }
                let Some(unit) = ancestor.file_name().and_then(|v| v.to_str()) else {
                    continue;
                };
                if ![".slice", ".service", ".scope"]
                    .iter()
                    .any(|suffix| unit.ends_with(suffix))
                {
                    continue;
                }
                // Cgroups through user@UID.service belong to the system manager;
                // its descendants belong to the user manager used for the leaf.
                let inside_user = ancestor.parent().is_some_and(|parent| {
                    parent.components().any(|part| {
                        part.as_os_str()
                            .to_str()
                            .is_some_and(|v| v.starts_with("user@") && v.ends_with(".service"))
                    })
                });
                let context = if inside_user && !system {
                    "user"
                } else {
                    "system"
                };
                let mut entry = ManagerMetadata {
                    unit: unit.into(),
                    context,
                    observed_unix_ms: crate::telemetry::now(),
                    properties: None,
                    unknown: None,
                };
                if inside_user && system {
                    entry.unknown = Some("different user manager context not queried");
                } else if Instant::now() >= deadline {
                    entry.unknown = Some("inspection manager-query budget exhausted");
                } else {
                    match metadata(&systemctl, unit, context == "system") {
                        Ok(values) => entry.properties = Some(values),
                        Err(_) => entry.unknown = Some("manager metadata unavailable or timed out"),
                    }
                }
                ancestors.push(entry);
            }
        } else {
            evidence_notes.push(
                "manager reported an invalid cgroup path; kernel observations unknown".into(),
            );
        }
    } else {
        evidence_notes
            .push("no ControlGroup reported; kernel state unknown (unit may be gone)".into());
    }
    let invocation_id = properties
        .get("InvocationID")
        .filter(|v| !v.is_empty() && !v.starts_with("unknown"))
        .cloned();
    Ok(Inspection {
        schema_version: 1,
        unit: unit.to_owned(),
        manager_context: if system { "system" } else { "user" },
        observed_unix_ms,
        boot_id: amc_telemetry::read_boot_id(),
        invocation_id,
        requested: "unknown: no launch request supplied to inspect; native units/drop-ins remain authoritative",
        properties,
        kernel,
        ancestors,
        evidence_notes,
    })
}

pub fn validate_unit_public(unit: &str) -> Result<()> {
    validate_unit(unit)
}

fn validate_unit(unit: &str) -> Result<()> {
    if unit.is_empty()
        || unit.len() > 255
        || !unit.ends_with(".service")
        || !unit.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@' | b':')
        })
    {
        bail!("UNIT must be a safe .service unit name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
