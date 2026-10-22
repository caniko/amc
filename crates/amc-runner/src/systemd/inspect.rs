//! Read-only unit inspection: manager properties, cgroup kernel snapshots and
//! ancestry, shared by the `amc inspect` CLI and library callers.
use std::{
    collections::BTreeMap,
    env,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::capture;

/// Resolve `program` to an absolute executable file, searching `PATH` for bare names.
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

/// Parse `systemctl show` `Name=value` output.
pub fn parse_properties(text: &str) -> BTreeMap<String, String> {
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

/// Extract the unified-hierarchy path from `/proc/<pid>/cgroup` contents.
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

/// Map a unified cgroup path to its directory below `/sys/fs/cgroup`.
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

/// Read-only snapshot of one unit: manager properties, kernel cgroup files and ancestry.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    /// Inspection JSON schema version.
    pub schema_version: u32,
    /// The inspected unit name.
    pub unit: String,
    /// `user` or `system`.
    pub manager_context: &'static str,
    /// Observation time in Unix milliseconds.
    pub observed_unix_ms: Option<u128>,
    /// Kernel boot identity at observation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// The unit's current invocation identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    /// The requested inspection scope.
    pub requested: &'static str,
    /// Manager-reported (systemctl show) settings.
    pub properties: BTreeMap<String, String>,
    /// Kernel cgroup snapshots for the unit and its descendants.
    pub kernel: Vec<amc_telemetry::Snapshot>,
    /// Manager metadata for enclosing slices.
    pub ancestors: Vec<ManagerMetadata>,
    /// Notes about absent, stale, or disappeared evidence.
    pub evidence_notes: Vec<String>,
}

/// Manager-reported metadata for one ancestor unit or slice.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerMetadata {
    /// The ancestor unit or slice name.
    pub unit: String,
    /// `user` or `system`.
    pub context: &'static str,
    /// Observation time in Unix milliseconds.
    pub observed_unix_ms: Option<u128>,
    /// Manager-reported properties, when the query succeeded.
    pub properties: Option<BTreeMap<String, String>>,
    /// Why the properties are unknown, when the query failed.
    pub unknown: Option<&'static str>,
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
    let output = super::capture_with_timeout(command.arg("--").arg(unit), timeout)?;
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

/// Inspect `unit` through the user (or `system`) manager without changing it.
pub fn inspect(unit: &str, system: bool) -> Result<Inspection> {
    validate_unit(unit)?;
    let systemctl = resolve_executable("systemctl")?;
    let observed_unix_ms = amc_telemetry::now_unix_ms();
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
                kernel.push(amc_telemetry::snapshot_legacy(&directory, &name));
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
                    observed_unix_ms: amc_telemetry::now_unix_ms(),
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

/// Public alias of [`validate_unit`] kept for existing callers.
pub fn validate_unit_public(unit: &str) -> Result<()> {
    validate_unit(unit)
}

/// Accept only safe `.service`/`.scope` unit names, never paths or arguments.
pub fn validate_unit(unit: &str) -> Result<()> {
    if unit.is_empty()
        || unit.len() > 255
        || !(unit.ends_with(".service") || unit.ends_with(".scope"))
        || !unit.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@' | b':')
        })
    {
        bail!("UNIT must be a safe .service or .scope unit name");
    }
    Ok(())
}
