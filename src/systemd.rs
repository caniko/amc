use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{IsTerminal, Read},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Profile;

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
    pub environment: &'a [String],
    pub argv: &'a [String],
}

pub fn start(spec: &StartSpec<'_>, mode: StartMode) -> Result<i32> {
    let systemd_run = resolve_executable("systemd-run")?;
    eprintln!("AMC unit: {}", spec.unit);
    if spec.retain {
        eprintln!(
            "Cleanup retained unit: systemctl --user stop {}; systemctl --user reset-failed {}",
            spec.unit, spec.unit
        );
    }
    let status = Command::new(systemd_run)
        .args(systemd_arguments(spec, mode))
        .status()
        .context("failed to execute systemd-run")?;
    Ok(status.code().unwrap_or(1))
}

pub fn systemd_arguments(spec: &StartSpec<'_>, mode: StartMode) -> Vec<String> {
    let mut args = vec![
        "--user".into(),
        "--service-type=exec".into(),
        "--expand-environment=no".into(),
        "--same-dir".into(),
        format!("--unit={}", spec.unit),
        format!(
            "--property=Description=Application Memory Contract: {}",
            spec.id
        ),
        format!("--property=Slice={}", spec.profile.slice),
        "--property=MemoryAccounting=yes".into(),
        format!("--property=MemoryMax={}", spec.profile.memory_max_bytes),
        format!(
            "--property=MemorySwapMax={}",
            spec.profile.memory_swap_max_bytes
        ),
        "--property=OOMPolicy=kill".into(),
        "--property=KillMode=control-group".into(),
    ];
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

pub fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

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
    let user_manager = systemctl.as_ref().ok().and_then(|program| {
        Command::new(program)
            .args(["--user", "show-environment"])
            .output()
            .ok()
    });
    checks.push(DoctorCheck {
        name: "user-manager",
        ok: user_manager
            .as_ref()
            .is_some_and(|output| output.status.success()),
        required: true,
        detail: match user_manager.as_ref() {
            Some(output) if output.status.success() => "user manager responded".into(),
            Some(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            None => result_path_detail(&systemctl),
        },
    });

    if let (Ok(systemd_run), Ok(systemctl), Ok(sleep)) = (systemd_run, systemctl, sleep)
        && user_manager.is_some_and(|output| output.status.success())
    {
        match probe_transient_service(&systemd_run, &systemctl, &sleep) {
            Ok(evidence) => {
                checks.push(DoctorCheck {
                    name: "transient-properties",
                    ok: evidence.properties_ok,
                    required: true,
                    detail: evidence.property_detail,
                });
                checks.push(DoctorCheck {
                    name: "memory-oom-group",
                    ok: evidence.oom_group == "1",
                    required: true,
                    detail: format!(
                        "cgroupfs memory.oom.group={} (set by OOMPolicy=kill)",
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

    let multi_user_daemon_detected = Path::new("/nix/var/nix/daemon-socket/socket").exists()
        || Command::new("systemctl")
            .args(["is-active", "--quiet", "nix-daemon.service"])
            .status()
            .is_ok_and(|status| status.success());
    let nix_boundary = NixBoundary {
        multi_user_daemon_detected,
        detail: if multi_user_daemon_detected {
            "multi-user Nix detected: derivation builders run under nix-daemon and outside an AMC user service"
                .into()
        } else {
            "multi-user Nix daemon was not detected by socket or active service".into()
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

fn output_detail(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stdout.is_empty() {
        stdout
    } else if !stderr.is_empty() {
        stderr
    } else {
        format!("exit status {}", output.status)
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
    let unit = unit_name_with_nonce("doctor-probe", std::process::id().into());
    let expected_memory_max = 64_u64 << 20;
    let profile = Profile {
        slice: "app.slice".into(),
        memory_max_bytes: expected_memory_max,
        memory_swap_max_bytes: 0,
    };
    let argv = vec![sleep.to_string_lossy().into_owned(), "20".into()];
    let spec = StartSpec {
        id: "doctor-probe",
        profile: &profile,
        unit: &unit,
        retain: false,
        environment: &[],
        argv: &argv,
    };
    let start = Command::new(systemd_run)
        .args(systemd_arguments(&spec, StartMode::Launch))
        .output()
        .context("failed to execute transient-property probe")?;
    if !start.status.success() {
        bail!("systemd-run probe failed: {}", output_detail(&start));
    }

    let result = (|| {
        let property_names = [
            "MemoryAccounting",
            "MemoryMax",
            "MemorySwapMax",
            "OOMPolicy",
            "KillMode",
            "Slice",
            "ControlGroup",
        ];
        let mut command = Command::new(systemctl);
        command.args(["--user", "show", "--no-pager"]);
        for property in property_names {
            command.arg(format!("--property={property}"));
        }
        let output = command.arg("--").arg(&unit).output()?;
        if !output.status.success() {
            bail!(
                "systemctl probe inspection failed: {}",
                output_detail(&output)
            );
        }
        let properties = parse_properties(&String::from_utf8_lossy(&output.stdout));
        validate_probe_properties(&properties, expected_memory_max)?;
        let control_group = properties
            .get("ControlGroup")
            .context("systemctl did not report ControlGroup")?;
        let cgroup = cgroup_directory(control_group)?;
        let memory_max = read_trimmed(cgroup.join("memory.max"))?;
        let memory_swap_max = read_trimmed(cgroup.join("memory.swap.max"))?;
        let oom_group = read_trimmed(cgroup.join("memory.oom.group"))?;
        let cgroup_ok = memory_max == expected_memory_max.to_string() && memory_swap_max == "0";
        Ok(ProbeEvidence {
            properties_ok: cgroup_ok,
            property_detail: format!(
                "systemd properties accepted; cgroupfs memory.max={memory_max}, memory.swap.max={memory_swap_max}"
            ),
            oom_group,
        })
    })();

    let _ = Command::new(systemctl)
        .args(["--user", "stop", "--"])
        .arg(&unit)
        .output();
    let _ = Command::new(systemctl)
        .args(["--user", "reset-failed", "--"])
        .arg(&unit)
        .output();
    result
}

fn validate_probe_properties(properties: &BTreeMap<String, String>, memory_max: u64) -> Result<()> {
    let expected = [
        ("MemoryAccounting", "yes".to_owned()),
        ("MemoryMax", memory_max.to_string()),
        ("MemorySwapMax", "0".to_owned()),
        ("OOMPolicy", "kill".to_owned()),
        ("KillMode", "control-group".to_owned()),
        ("Slice", "app.slice".to_owned()),
    ];
    for (name, value) in expected {
        match properties.get(name) {
            Some(actual) if actual == &value => {}
            Some(actual) => bail!("property {name} is {actual:?}, expected {value:?}"),
            None => bail!("property {name} is missing"),
        }
    }
    Ok(())
}

fn parse_properties(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
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

fn read_trimmed(path: PathBuf) -> Result<String> {
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))
        .map(|value| value.trim().to_owned())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub unit: String,
    pub properties: BTreeMap<String, String>,
    pub cgroup_path: Option<String>,
    pub cgroup: BTreeMap<String, String>,
    pub memory_events: BTreeMap<String, u64>,
    pub journal: String,
}

pub fn inspect(unit: &str) -> Result<Inspection> {
    validate_unit(unit)?;
    let systemctl = resolve_executable("systemctl")?;
    let properties_to_read = [
        "Id",
        "Description",
        "LoadState",
        "ActiveState",
        "SubState",
        "Result",
        "ExecMainCode",
        "ExecMainStatus",
        "Slice",
        "MemoryAccounting",
        "MemoryCurrent",
        "MemoryPeak",
        "MemoryMax",
        "MemorySwapCurrent",
        "MemorySwapMax",
        "OOMPolicy",
        "KillMode",
        "ControlGroup",
    ];
    let mut command = Command::new(systemctl);
    command.args(["--user", "show", "--no-pager"]);
    for property in properties_to_read {
        command.arg(format!("--property={property}"));
    }
    let output = command.arg("--").arg(unit).output()?;
    if !output.status.success() {
        bail!("systemctl show failed: {}", output_detail(&output));
    }
    let properties = parse_properties(&String::from_utf8_lossy(&output.stdout));
    let cgroup_path = properties
        .get("ControlGroup")
        .filter(|path| !path.is_empty())
        .cloned();
    let mut cgroup = BTreeMap::new();
    let mut memory_events = BTreeMap::new();
    if let Some(path) = &cgroup_path {
        let directory = cgroup_directory(path)?;
        for file in [
            "memory.current",
            "memory.peak",
            "memory.swap.current",
            "memory.max",
            "memory.swap.max",
            "memory.oom.group",
        ] {
            if let Ok(value) = fs::read_to_string(directory.join(file)) {
                cgroup.insert(file.to_owned(), value.trim().to_owned());
            }
        }
        if let Ok(events) = fs::read_to_string(directory.join("memory.events")) {
            memory_events = events
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter_map(|(name, value)| {
                    value.parse().ok().map(|value| (name.to_owned(), value))
                })
                .collect();
        }
    }

    let journal = resolve_executable("journalctl")
        .and_then(|journalctl| {
            Command::new(journalctl)
                .args([
                    "--user-unit",
                    unit,
                    "--lines=50",
                    "--no-pager",
                    "--output=short-iso",
                ])
                .output()
                .context("failed to execute journalctl")
        })
        .map(|output| {
            if output.status.success() {
                String::from_utf8_lossy(&output.stdout).into_owned()
            } else {
                output_detail(&output)
            }
        })
        .unwrap_or_else(|error| format!("journal unavailable: {error:#}"));

    Ok(Inspection {
        unit: unit.to_owned(),
        properties,
        cgroup_path,
        cgroup,
        memory_events,
        journal,
    })
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

    fn profile() -> Profile {
        Profile {
            slice: "app-amc.slice".into(),
            memory_max_bytes: 268_435_456,
            memory_swap_max_bytes: 33_554_432,
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
        assert!(!args.iter().any(|arg| arg.contains("MemoryOOMGroup")));
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
