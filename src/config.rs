use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    profiles: BTreeMap<String, RawProfile>,
    #[serde(default)]
    applications: BTreeMap<String, Application>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    slice: String,
    memory_max: String,
    memory_swap_max: String,
    /// Optional fixture MemoryHigh; absent means do not submit this property.
    #[serde(default)]
    memory_high: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Application {
    profile: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub source: PathBuf,
    profiles: BTreeMap<String, Profile>,
    applications: BTreeMap<String, Application>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub slice: String,
    pub memory_max_bytes: u64,
    pub memory_swap_max_bytes: u64,
    /// None = do not emit MemoryHigh. Native precedence is measured separately.
    pub memory_high_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionSource {
    ExplicitProfile,
    ApplicationRule,
}

#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub name: String,
    pub profile: Profile,
    pub resolution: ResolutionSource,
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let source = select_config(
        explicit.map(PathBuf::from),
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
        |path| path.is_file(),
    )?;
    let text = fs::read_to_string(&source)
        .with_context(|| format!("failed to read config {}", source.display()))?;
    parse(&text, source)
}

fn parse(text: &str, source: PathBuf) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text).context("invalid AMC TOML configuration")?;
    if raw.version != 1 {
        bail!("unsupported config version {}; expected 1", raw.version);
    }

    let mut profiles = BTreeMap::new();
    for (name, profile) in raw.profiles {
        if name.trim().is_empty() {
            bail!("profile names must not be empty");
        }
        validate_slice(&profile.slice)
            .with_context(|| format!("profile {name:?} has an invalid slice"))?;
        let memory_max_bytes = parse_bytes(&profile.memory_max)
            .with_context(|| format!("profile {name:?} has invalid memory_max"))?;
        if memory_max_bytes == 0 {
            bail!("profile {name:?} memory_max must be greater than zero");
        }
        let memory_swap_max_bytes = parse_bytes(&profile.memory_swap_max)
            .with_context(|| format!("profile {name:?} has invalid memory_swap_max"))?;
        let memory_high_bytes = profile
            .memory_high
            .as_deref()
            .map(parse_bytes)
            .transpose()
            .with_context(|| format!("profile {name:?} has invalid memory_high"))?;
        if let Some(high) = memory_high_bytes
            && high > memory_max_bytes
        {
            bail!("profile {name:?} memory_high must not exceed memory_max");
        }
        profiles.insert(
            name,
            Profile {
                slice: profile.slice,
                memory_max_bytes,
                memory_swap_max_bytes,
                memory_high_bytes,
            },
        );
    }

    for (id, application) in &raw.applications {
        validate_app_id(id)?;
        if !profiles.contains_key(&application.profile) {
            bail!(
                "application {id:?} references unknown profile {:?}",
                application.profile
            );
        }
    }

    Ok(Config {
        source,
        profiles,
        applications: raw.applications,
    })
}

impl Config {
    pub fn resolve(&self, id: &str, explicit_profile: Option<&str>) -> Result<ResolvedProfile> {
        validate_app_id(id)?;
        let (name, resolution) = if let Some(name) = explicit_profile {
            if name.trim().is_empty() {
                bail!("profile name must not be empty");
            }
            (name, ResolutionSource::ExplicitProfile)
        } else if let Some(application) = self.applications.get(id) {
            (
                application.profile.as_str(),
                ResolutionSource::ApplicationRule,
            )
        } else {
            bail!("application {id:?} has no rule; provide --profile or add an application rule");
        };
        let profile = self
            .profiles
            .get(name)
            .with_context(|| format!("unknown profile {name:?}"))?
            .clone();
        Ok(ResolvedProfile {
            name: name.to_owned(),
            profile,
            resolution,
        })
    }
}

pub fn parse_bytes(value: &str) -> Result<u64> {
    let (digits, multiplier) = [
        ("TiB", 1_u64 << 40),
        ("GiB", 1_u64 << 30),
        ("MiB", 1_u64 << 20),
        ("KiB", 1_u64 << 10),
        ("B", 1),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| value.strip_suffix(suffix).map(|v| (v, multiplier)))
    .with_context(|| format!("invalid byte size {value:?}; expected B, KiB, MiB, GiB, or TiB"))?;

    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("byte size must be an integral non-negative value");
    }
    digits
        .parse::<u64>()
        .context("byte value is too large")?
        .checked_mul(multiplier)
        .context("byte value overflows u64")
}

pub fn validate_app_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        bail!("application ID must not be empty");
    }
    if id.len() > 256 {
        bail!("application ID must not exceed 256 bytes");
    }
    if id.chars().any(char::is_control) {
        bail!("application ID must not contain control characters");
    }
    Ok(())
}

fn validate_slice(slice: &str) -> Result<()> {
    if slice.len() > 255 || !slice.ends_with(".slice") {
        bail!("slice must be a systemd .slice unit name of at most 255 bytes");
    }
    let stem = &slice[..slice.len() - ".slice".len()];
    if stem.is_empty()
        || stem.starts_with('-')
        || !stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("slice contains unsafe characters");
    }
    Ok(())
}

fn select_config<F>(
    explicit: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    exists: F,
) -> Result<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    if let Some(path) = explicit {
        if exists(&path) {
            return Ok(path);
        }
        bail!("explicit config {} does not exist", path.display());
    }

    let mut candidates = Vec::new();
    if let Some(path) = xdg_config_home {
        candidates.push(path.join("amc/config.toml"));
    }
    if let Some(path) = home {
        candidates.push(path.join(".config/amc/config.toml"));
    }
    candidates.push(PathBuf::from("/etc/xdg/amc/config.toml"));
    candidates.into_iter().find(|path| exists(path)).context(
        "no AMC config found; use --config, XDG_CONFIG_HOME, HOME, or /etc/xdg/amc/config.toml",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1

[profiles.small]
slice = "app-amc.slice"
memory_max = "256MiB"
memory_swap_max = "0B"

[profiles.large]
slice = "background-amc.slice"
memory_max = "1GiB"
memory_swap_max = "64MiB"

[applications."ai.opencode"]
profile = "small"
"#;

    #[test]
    fn byte_sizes_and_overflow() {
        assert_eq!(parse_bytes("0B").unwrap(), 0);
        assert_eq!(parse_bytes("2KiB").unwrap(), 2048);
        assert_eq!(parse_bytes("3MiB").unwrap(), 3 << 20);
        assert_eq!(parse_bytes("4GiB").unwrap(), 4 << 30);
        assert_eq!(parse_bytes("5TiB").unwrap(), 5 << 40);
        assert!(parse_bytes("1.5GiB").is_err());
        assert!(parse_bytes("1MB").is_err());
        assert!(parse_bytes("18446744073709551615TiB").is_err());
    }

    #[test]
    fn config_search_order_and_explicit_failure() {
        let explicit = PathBuf::from("/explicit");
        let xdg = PathBuf::from("/xdg");
        let home = PathBuf::from("/home/test");
        let selected = select_config(None, Some(xdg.clone()), Some(home), |path| {
            path == xdg.join("amc/config.toml") || path == Path::new("/etc/xdg/amc/config.toml")
        })
        .unwrap();
        assert_eq!(selected, Path::new("/xdg/amc/config.toml"));

        assert!(select_config(Some(explicit), Some(xdg), None, |_| false).is_err());
        assert_eq!(
            select_config(None, None, None, |path| {
                path == Path::new("/etc/xdg/amc/config.toml")
            })
            .unwrap(),
            Path::new("/etc/xdg/amc/config.toml")
        );
    }

    #[test]
    fn invalid_schema_version_and_reference_are_rejected() {
        assert!(parse(&VALID.replace("version = 1", "version = 2"), "x".into()).is_err());
        assert!(parse(&format!("{VALID}\nunknown = true"), "x".into()).is_err());
        assert!(
            parse(
                &VALID.replace("profile = \"small\"", "profile = \"missing\""),
                "x".into()
            )
            .is_err()
        );
        assert!(parse(&VALID.replace("256MiB", "0B"), "x".into()).is_err());
        assert!(parse(&VALID.replace("app-amc.slice", "../bad.slice"), "x".into()).is_err());
    }

    #[test]
    fn explicit_profile_precedes_application_rule() {
        let config = parse(VALID, "config.toml".into()).unwrap();
        let mapped = config.resolve("ai.opencode", None).unwrap();
        assert_eq!(mapped.name, "small");
        assert_eq!(mapped.resolution, ResolutionSource::ApplicationRule);
        // Absent memory_high means "do not emit": never mask native config.
        assert_eq!(mapped.profile.memory_high_bytes, None);

        let explicit = config.resolve("ai.opencode", Some("large")).unwrap();
        assert_eq!(explicit.name, "large");
        assert_eq!(explicit.resolution, ResolutionSource::ExplicitProfile);
        assert!(config.resolve("unknown", None).is_err());
    }

    #[test]
    fn memory_high_is_optional_and_bounded_by_max() {
        let with_high = VALID.replace(
            "memory_swap_max = \"0B\"",
            "memory_swap_max = \"0B\"\nmemory_high = \"128MiB\"",
        );
        let config = parse(&with_high, "config.toml".into()).unwrap();
        let resolved = config.resolve("ai.opencode", None).unwrap();
        assert_eq!(resolved.profile.memory_high_bytes, Some(128 << 20));

        let too_high = VALID.replace(
            "memory_swap_max = \"0B\"",
            "memory_swap_max = \"0B\"\nmemory_high = \"1GiB\"",
        );
        assert!(parse(&too_high, "config.toml".into()).is_err());
        let bad_unit = VALID.replace(
            "memory_swap_max = \"0B\"",
            "memory_swap_max = \"0B\"\nmemory_high = \"1MB\"",
        );
        assert!(parse(&bad_unit, "config.toml".into()).is_err());
    }
}
