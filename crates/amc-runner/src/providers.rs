//! Built-in [`MemoryProvider`] implementations.
//!
//! [`MemoryProvider`]: crate::MemoryProvider

use std::sync::Arc;

use crate::provider::{
    MemoryDomain, MemoryProvider, MemoryStats, ProviderError, SharedMemoryProvider,
};

/// Select the preferred memory provider for the current platform.
///
/// Provider precedence is:
///
/// 1. Linux: cgroup-v2 accounting for the calling process when it is in a
///    cgroup-v2 hierarchy (host domains plus limited ancestors).
/// 2. Linux: `/proc/meminfo` when no cgroup-v2 hierarchy is present.
///    `sysinfo` is not consulted on Linux even when the feature is enabled.
/// 3. Non-Linux with the `sysinfo` feature: `SysinfoProvider`.
///
/// On unsupported target/feature combinations this returns a provider that
/// reports [`ProviderError::Unsupported`]. A cgroup placement error yields a
/// failing provider that reports that error (sanitized by diagnostics).
#[must_use]
pub fn default_provider() -> SharedMemoryProvider {
    #[cfg(target_os = "linux")]
    {
        match CgroupV2Provider::for_self() {
            Ok(Some(provider)) => Arc::new(provider),
            Ok(None) => ProcMeminfoProvider::shared(),
            Err(error) => {
                let message = error.to_string();
                Arc::new(move || Err(ProviderError::new(message.clone())))
            }
        }
    }

    #[cfg(all(not(target_os = "linux"), feature = "sysinfo"))]
    {
        SysinfoProvider::shared()
    }

    #[cfg(all(not(target_os = "linux"), not(feature = "sysinfo")))]
    {
        Arc::new(UnsupportedProvider)
    }
}

#[cfg(all(not(target_os = "linux"), not(feature = "sysinfo")))]
struct UnsupportedProvider;

#[cfg(all(not(target_os = "linux"), not(feature = "sysinfo")))]
impl MemoryProvider for UnsupportedProvider {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        Err(ProviderError::Unsupported)
    }
}

/// Reads `/proc/meminfo` on Linux and returns `1.0 - MemAvailable/MemTotal`.
///
/// This is cheap (a single short file read), always-current (kernel updates
/// the file on every read), and unaffected by per-process memory accounting
/// quirks. It is the recommended provider on Linux.
pub struct ProcMeminfoProvider;

impl ProcMeminfoProvider {
    /// Path read by this provider.
    pub const PATH: &'static str = "/proc/meminfo";

    /// Construct an `Arc` ready to hand to a gate.
    #[must_use]
    pub fn shared() -> SharedMemoryProvider {
        Arc::new(Self)
    }
}

impl MemoryProvider for ProcMeminfoProvider {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        Ok(self.stats()?.used_fraction())
    }

    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        let contents = std::fs::read_to_string(Self::PATH)
            .map_err(|e| ProviderError::new(format!("read {}: {e}", Self::PATH)))?;
        parse_proc_meminfo_stats(&contents)
    }
}

/// Reads cgroup-v2 memory accounting for the calling process.
///
/// When the application runs inside a `systemd-run --scope -p MemoryHigh=…`
/// (or any other cgroup with a memory limit), the *host* `MemAvailable` is
/// no longer the relevant signal — the cgroup will be throttled long before
/// host memory is exhausted, and a host-level gate will happily admit work
/// that the kernel then forces into swap.
///
/// This provider locates the calling process's cgroup-v2 path (from
/// `/proc/self/cgroup`), or roots at an explicit directory via
/// [`CgroupV2Provider::for_dir`] (e.g. a worker unit's `ControlGroup`
/// resolved with `systemd::unit_cgroup_dir`), then reports:
///
/// - `total_bytes` / `available_bytes` — min of host and each limited
///   ancestor (`memory.high` and `memory.max`, including zero).
/// - `used_fraction` — max used fraction across those domains.
/// - `page_cache_bytes` — leaf `memory.stat` `file` accounting.
///
/// Linux-only. Use [`ProcMeminfoProvider`] or `SysinfoProvider` when the
/// host's view is the authoritative one.
#[cfg(target_os = "linux")]
pub struct CgroupV2Provider {
    base_dir: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl CgroupV2Provider {
    /// Build a provider for the calling process's cgroup. Returns `None` if
    /// the process is not in a cgroup-v2 hierarchy (e.g. legacy cgroup-v1).
    /// Locate the calling process's cgroup-v2 directory.
    ///
    /// `Ok(None)` means this process is not in a cgroup-v2 hierarchy.
    /// Unreadable placement or a missing directory is `Err`.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when `/proc/self/cgroup` is unreadable or
    /// the v2 path does not exist.
    pub fn for_self() -> Result<Option<Self>, ProviderError> {
        let cgroup_line = std::fs::read_to_string("/proc/self/cgroup")
            .map_err(|e| ProviderError::new(format!("read /proc/self/cgroup: {e}")))?;
        let Some(path) = cgroup_line.lines().find_map(|l| l.strip_prefix("0::")) else {
            return Ok(None);
        };
        let base_dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        Ok(Some(Self::for_dir(base_dir)?))
    }

    /// Build a provider rooted at an arbitrary cgroup-v2 directory, e.g. a
    /// worker unit's `ControlGroup` resolved through the manager. Ancestor
    /// limits up to the hierarchy root are observed exactly like
    /// [`Self::for_self`]; the directory must exist.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when the directory does not exist.
    pub fn for_dir(base_dir: std::path::PathBuf) -> Result<Self, ProviderError> {
        if !base_dir.is_dir() {
            return Err(ProviderError::new(format!(
                "cgroup path missing: {}",
                base_dir.display()
            )));
        }
        Ok(Self { base_dir })
    }

    /// Wrap in an `Arc` for handoff to a gate.
    ///
    /// # Errors
    /// Returns [`ProviderError`] when cgroup-v2 placement cannot be observed.
    pub fn shared() -> Result<Option<SharedMemoryProvider>, ProviderError> {
        Ok(Self::for_self()?.map(|p| Arc::new(p) as SharedMemoryProvider))
    }

    fn read_current_at(dir: &std::path::Path) -> Option<u64> {
        // Shared strict parser from the observation core: rejects signs,
        // overflow, and non-numeric text rather than coercing them.
        let text = std::fs::read_to_string(dir.join("memory.current")).ok()?;
        amc_telemetry::parse("memory.current", text.trim()).and_then(|v| v.as_u64())
    }

    /// Leaf `memory.stat` file-cache bytes. `None` means unavailable and
    /// must not be read as observed zero; see `MemoryStats::page_cache_opt`.
    fn read_file_cache_at(dir: &std::path::Path) -> Option<u64> {
        let stat = std::fs::read_to_string(dir.join("memory.stat")).ok()?;
        parse_cgroup_file_cache_opt(&stat)
    }
}

#[cfg(target_os = "linux")]
impl MemoryProvider for CgroupV2Provider {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        Ok(self.stats()?.used_fraction())
    }

    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        let host = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| ProviderError::new(format!("read /proc/meminfo: {e}")))
            .and_then(|s| parse_proc_meminfo_stats(&s))?;
        let mut layers = Vec::new();
        let mut dir = self.base_dir.clone();
        let mut leaf_cache = None;
        let mut reached_root = false;
        for index in 0..64 {
            let at_sys_root = dir.as_path() == std::path::Path::new("/sys/fs/cgroup");
            if at_sys_root {
                reached_root = true;
            }
            if let Some(limit) = layer_limit(&dir, at_sys_root)? {
                layers.push((limit, Self::read_current_at(&dir)));
            }
            if index == 0 {
                leaf_cache = Self::read_file_cache_at(&dir);
            }
            if at_sys_root {
                break;
            }
            if !dir.pop() {
                break;
            }
        }
        if !reached_root {
            return Err(ProviderError::new(
                "cgroup ancestry truncated; remaining limits unknown",
            ));
        }
        combine_domain(&host, &layers, leaf_cache)
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitObservation {
    Unlimited,
    Bytes(u64),
}

#[cfg(target_os = "linux")]
fn parse_limit_text(trimmed: &str) -> Result<LimitObservation, ProviderError> {
    if trimmed == "max" {
        return Ok(LimitObservation::Unlimited);
    }
    if trimmed.is_empty() {
        return Err(ProviderError::new("empty memory limit"));
    }
    trimmed
        .parse()
        .map(LimitObservation::Bytes)
        .map_err(|_| ProviderError::new("malformed memory limit"))
}

#[cfg(target_os = "linux")]
fn observe_limit_file(
    dir: &std::path::Path,
    name: &str,
) -> Result<Option<LimitObservation>, ProviderError> {
    let path = dir.join(name);
    match std::fs::metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ProviderError::new(format!(
            "stat {}: {error}",
            path.display()
        ))),
        Ok(meta) if !meta.is_file() => Err(ProviderError::new(format!(
            "{} is not a regular file",
            path.display()
        ))),
        Ok(_) => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|error| ProviderError::new(format!("read {}: {error}", path.display())))?;
            Ok(Some(parse_limit_text(raw.trim())?))
        }
    }
}

#[cfg(target_os = "linux")]
fn layer_limit(
    dir: &std::path::Path,
    at_sys_root: bool,
) -> Result<Option<LimitObservation>, ProviderError> {
    let high = observe_limit_file(dir, "memory.high")?;
    let max = observe_limit_file(dir, "memory.max")?;
    match (high, max) {
        (None, None) if at_sys_root => Ok(None),
        (None, None) => Err(ProviderError::new(format!(
            "cgroup memory controller missing at {}",
            dir.display()
        ))),
        (Some(_), None) | (None, Some(_)) => Err(ProviderError::new(format!(
            "partial cgroup memory limits at {}",
            dir.display()
        ))),
        (Some(high), Some(max)) => Ok(Some(tighter_limit(high, max))),
    }
}

#[cfg(target_os = "linux")]
fn tighter_limit(high: LimitObservation, max: LimitObservation) -> LimitObservation {
    match (high, max) {
        (LimitObservation::Unlimited, other) | (other, LimitObservation::Unlimited) => other,
        (LimitObservation::Bytes(a), LimitObservation::Bytes(b)) => {
            LimitObservation::Bytes(a.min(b))
        }
    }
}

/// Host plus each limited ancestor as separate domains.
///
/// `page_cache_bytes` is `None` when leaf cache accounting is unavailable;
/// it is carried as unknown, never coerced to observed zero. The cache
/// belongs to the leaf layer: its denominator is the leaf's effective
/// limit when the leaf is limited, else the host total.
#[cfg(target_os = "linux")]
fn combine_domain(
    host: &MemoryStats,
    layers: &[(LimitObservation, Option<u64>)],
    page_cache_bytes: Option<u64>,
) -> Result<MemoryStats, ProviderError> {
    let mut domains = host.domains().to_vec();
    let mut leaf_total = None;
    for (index, &(limit, current)) in layers.iter().enumerate() {
        let LimitObservation::Bytes(limit) = limit else {
            continue;
        };
        if index == 0 {
            leaf_total = Some(limit);
        }
        let current = current.ok_or_else(|| {
            ProviderError::new("cgroup memory.current unreadable under a configured limit")
        })?;
        domains.push(MemoryDomain::new(limit, limit.saturating_sub(current))?);
    }
    let stats = MemoryStats::from_domains(domains, page_cache_bytes)?;
    match (page_cache_bytes, leaf_total) {
        (Some(_), Some(leaf)) => Ok(stats.with_cache_total(leaf)),
        (Some(_), None) => Ok(stats.with_cache_total(host.total_bytes())),
        (None, _) => Ok(stats),
    }
}

/// Returns a fixed value (useful for tests).
#[derive(Debug, Clone)]
pub struct FixedProvider {
    /// The constant fraction returned by [`MemoryProvider::used_fraction`].
    pub fraction: f64,
}

impl FixedProvider {
    /// Create a fixed-value provider.
    #[must_use]
    pub const fn new(fraction: f64) -> Self {
        Self { fraction }
    }

    /// Wrap in an `Arc` for handoff to a gate.
    #[must_use]
    pub fn shared(fraction: f64) -> SharedMemoryProvider {
        Arc::new(Self::new(fraction))
    }
}

impl MemoryProvider for FixedProvider {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        crate::finite_fraction(self.fraction)
    }

    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        let fraction = crate::finite_fraction(self.fraction)?;
        const ONE_GIB: u64 = 1024 * 1024 * 1024;
        let total = ONE_GIB;
        let available = (((1.0 - fraction).max(0.0)) * total as f64) as u64;
        // Test double with a declared zero cache over its own total.
        Ok(MemoryStats::new(total, available.min(total), 0)?.with_cache_total(total))
    }
}

/// Cross-platform memory provider backed by the `sysinfo` crate.
///
/// Available when the `sysinfo` feature is enabled (default).
#[cfg(feature = "sysinfo")]
pub struct SysinfoProvider {
    system: std::sync::Mutex<sysinfo::System>,
}

#[cfg(feature = "sysinfo")]
impl SysinfoProvider {
    /// Build a fresh sysinfo provider with memory refresh enabled.
    #[must_use]
    pub fn new() -> Self {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        Self {
            system: std::sync::Mutex::new(system),
        }
    }

    /// Wrap in an `Arc` for handoff to a gate.
    #[must_use]
    pub fn shared() -> SharedMemoryProvider {
        Arc::new(Self::new())
    }
}

#[cfg(feature = "sysinfo")]
impl Default for SysinfoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "sysinfo")]
impl MemoryProvider for SysinfoProvider {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        Ok(self.stats()?.used_fraction())
    }

    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        let mut system = self
            .system
            .lock()
            .map_err(|e| ProviderError::new(format!("sysinfo mutex poisoned: {e}")))?;
        system.refresh_memory();
        let total = system.total_memory();
        if total == 0 {
            return Err(ProviderError::new("sysinfo reports total_memory = 0"));
        }
        let available = system.available_memory().min(total);
        // sysinfo reports no page-cache accounting: unknown, never a
        // fabricated zero. Callers that require the measurement fail
        // through the explicit policy instead.
        Ok(MemoryStats::new_with_cache(total, available, None)?.with_cache_total(total))
    }
}

#[cfg(test)]
fn parse_proc_meminfo(contents: &str) -> Result<f64, ProviderError> {
    Ok(parse_proc_meminfo_stats(contents)?.used_fraction())
}

fn parse_proc_meminfo_stats(contents: &str) -> Result<MemoryStats, ProviderError> {
    let mut total_kib = None;
    let mut available_kib = None;
    let mut buffers_kib: Option<u64> = None;
    let mut cached_kib: Option<u64> = None;

    for line in contents.lines() {
        if let Some(v) = parse_meminfo_line(line, "MemTotal") {
            total_kib = Some(v);
        } else if let Some(v) = parse_meminfo_line(line, "MemAvailable") {
            available_kib = Some(v);
        } else if is_meminfo_key(line, "Buffers") {
            // Present-but-malformed is unknown, not zero: coerce neither.
            buffers_kib = parse_meminfo_line(line, "Buffers");
        } else if is_meminfo_key(line, "Cached") {
            cached_kib = parse_meminfo_line(line, "Cached");
        }
    }

    let total = total_kib.ok_or_else(|| ProviderError::new("MemTotal not found"))?;
    let available = available_kib.ok_or_else(|| ProviderError::new("MemAvailable not found"))?;

    if total == 0 {
        return Err(ProviderError::new("MemTotal must be > 0"));
    }
    if available > total {
        return Err(ProviderError::new("MemAvailable cannot exceed MemTotal"));
    }

    // Host page cache is Buffers+Cached, but only when both lines parsed.
    // A missing or malformed line is unknown accounting, never observed zero.
    let page_cache = match (buffers_kib, cached_kib) {
        (Some(buffers), Some(cached)) => Some(buffers.saturating_add(cached).saturating_mul(1024)),
        _ => None,
    };
    let total = total.saturating_mul(1024);
    Ok(
        MemoryStats::new_with_cache(total, available.saturating_mul(1024), page_cache)?
            .with_cache_total(total),
    )
}

fn is_meminfo_key(line: &str, key: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(name, _)| name.trim() == key)
}

fn parse_meminfo_line(line: &str, key: &str) -> Option<u64> {
    let (name, rest) = line.split_once(':')?;
    if name.trim() != key {
        return None;
    }
    // Strict digits-only: `str::parse::<u64>` would accept a leading `+`.
    let digits = rest.split_whitespace().next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

#[cfg(target_os = "linux")]
fn parse_cgroup_file_cache_opt(contents: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let rest = line.strip_prefix("file ")?.trim();
        if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        rest.parse::<u64>().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_proc_meminfo() {
        let contents = "MemTotal:       65536000 kB\nMemFree:         1024000 kB\nMemAvailable:   13107200 kB\n";
        let used = parse_proc_meminfo(contents).unwrap();
        assert!((used - 0.80).abs() < 0.01);
    }

    #[test]
    fn errors_when_available_exceeds_total() {
        let contents = "MemTotal: 1000 kB\nMemAvailable: 2000 kB\n";
        assert!(parse_proc_meminfo(contents).is_err());
    }

    #[test]
    fn errors_when_total_zero() {
        let contents = "MemTotal: 0 kB\nMemAvailable: 0 kB\n";
        assert!(parse_proc_meminfo(contents).is_err());
    }

    #[test]
    fn errors_when_field_missing() {
        let contents = "MemTotal: 1000 kB\n";
        assert!(parse_proc_meminfo(contents).is_err());
    }

    #[test]
    fn default_provider_constructs() {
        let _provider = default_provider();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_cgroup_file_cache() {
        let contents = "anon 1024\nfile 4096\nkernel 512\n";
        assert_eq!(parse_cgroup_file_cache_opt(contents), Some(4096));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestor_limit_beats_leaf_and_host() {
        let host = MemoryStats::new(8 << 30, 6 << 30, 0).unwrap();
        let stats = combine_domain(
            &host,
            &[
                (LimitObservation::Unlimited, Some(200 << 20)),
                (LimitObservation::Bytes(1 << 30), Some(800 << 20)),
            ],
            Some(0),
        )
        .unwrap();
        assert_eq!(stats.total_bytes(), 1 << 30);
        assert_eq!(stats.available_bytes(), 224 << 20);
        assert!((stats.used_fraction() - 0.78125).abs() < 1e-9);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_pressure_is_not_hidden_by_empty_leaf() {
        let host = MemoryStats::new(1000, 200, 0).unwrap();
        let stats =
            combine_domain(&host, &[(LimitObservation::Bytes(100), Some(0))], Some(0)).unwrap();
        assert_eq!(stats.total_bytes(), 100);
        assert_eq!(stats.available_bytes(), 100);
        assert!((stats.used_fraction() - 0.80).abs() < 1e-9);
        assert_eq!(stats.domains().len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tighter_of_high_and_max_and_zero_limit() {
        assert_eq!(
            tighter_limit(LimitObservation::Bytes(512), LimitObservation::Bytes(256)),
            LimitObservation::Bytes(256)
        );
        let host = MemoryStats::new(1000, 900, 0).unwrap();
        let stats =
            combine_domain(&host, &[(LimitObservation::Bytes(0), Some(0))], Some(0)).unwrap();
        assert_eq!(stats.total_bytes(), 0);
        assert_eq!(stats.available_bytes(), 0);
        assert_eq!(stats.used_fraction(), 1.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_limit_is_unknown_not_unlimited() {
        assert!(parse_limit_text("nope").is_err());
        assert!(parse_limit_text("").is_err());
        assert_eq!(
            parse_limit_text("max").unwrap(),
            LimitObservation::Unlimited
        );
        assert_eq!(parse_limit_text("0").unwrap(), LimitObservation::Bytes(0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_current_under_limit_is_error() {
        let host = MemoryStats::new(8 << 30, 4 << 30, 0).unwrap();
        assert!(
            combine_domain(&host, &[(LimitObservation::Bytes(1 << 30), None)], Some(0)).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sys_root_without_memory_files_is_not_an_error() {
        let dir = crate::test_support::scratch_dir("amc-runner-cgroup");
        assert!(layer_limit(&dir, true).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn for_dir_rejects_missing_directories() {
        let dir = crate::test_support::scratch_dir("amc-runner-cgroup");
        assert!(CgroupV2Provider::for_dir(dir.join("missing")).is_err());
        assert!(CgroupV2Provider::for_dir(dir.clone()).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn for_dir_observes_a_live_worker_domain() {
        let placement = std::fs::read_to_string("/proc/self/cgroup").expect("cgroup v2 placement");
        let path = placement
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("cgroup v2 hierarchy");
        let dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        let stats = CgroupV2Provider::for_dir(dir).unwrap().stats().unwrap();
        assert!(stats.total_bytes() > 0);
        assert!(stats.available_bytes() <= stats.total_bytes());
        assert!((0.0..=1.0).contains(&stats.used_fraction()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partial_limits_are_unknown() {
        let dir = crate::test_support::scratch_dir("amc-runner-cgroup");
        std::fs::write(dir.join("memory.high"), b"512\n").unwrap();
        assert!(layer_limit(&dir, true).is_err());
        assert!(layer_limit(&dir, false).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_file_memory_interface_is_unknown() {
        let dir = crate::test_support::scratch_dir("amc-runner-cgroup");
        std::fs::create_dir(dir.join("memory.max")).unwrap();
        assert!(observe_limit_file(&dir, "memory.max").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cache_denominator_is_the_leaf_limit_not_the_aggregate_minimum() {
        // Leaf limit 2000, tighter ancestor 1000, host 8000: the aggregate
        // minimum is 1000, but the leaf cache belongs to the leaf domain.
        let host = MemoryStats::new(8000, 7000, 0).unwrap();
        let stats = combine_domain(
            &host,
            &[
                (LimitObservation::Bytes(2000), Some(500)),
                (LimitObservation::Bytes(1000), Some(200)),
            ],
            Some(600),
        )
        .unwrap();
        assert_eq!(stats.total_bytes(), 1000);
        assert_eq!(stats.page_cache_total(), Some(2000));
        // Unlimited leaf: the cache belongs to the host domain.
        let stats = combine_domain(
            &host,
            &[(LimitObservation::Unlimited, Some(500))],
            Some(600),
        )
        .unwrap();
        assert_eq!(stats.page_cache_total(), Some(8000));
        // Unknown cache carries no denominator either.
        let stats =
            combine_domain(&host, &[(LimitObservation::Bytes(2000), Some(500))], None).unwrap();
        assert_eq!(stats.page_cache_opt(), None);
        assert_eq!(stats.page_cache_total(), None);
        // Host accounting: cache denominator is the host total.
        let stats = parse_proc_meminfo_stats(
            "MemTotal: 8000 kB\nMemAvailable: 7000 kB\nBuffers: 100 kB\nCached: 500 kB\n",
        )
        .unwrap();
        assert_eq!(stats.page_cache_total(), Some(8000 * 1024));
    }

    #[test]
    fn strict_current_parser_rejects_signs_and_overflow() {
        // Shared core parser via the provider: signs and overflow are
        // unknown, not coerced.
        assert_eq!(
            amc_telemetry::parse("memory.current", "18446744073709551616"),
            None
        );
        assert_eq!(amc_telemetry::parse("memory.current", "-1"), None);
        assert_eq!(amc_telemetry::parse("memory.current", "+1"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_cache_is_unknown_not_zero() {
        // `memory.stat` without a `file` line: cache unknown, not observed 0.
        assert_eq!(parse_cgroup_file_cache_opt("anon 1024\nkernel 512\n"), None);
        assert_eq!(
            parse_cgroup_file_cache_opt("anon 1024\nfile 4096\n"),
            Some(4096)
        );
        // Explicit zero stays zero and known.
        let known_zero = MemoryStats::new(1000, 500, 0).unwrap();
        assert!(known_zero.page_cache_known());
        assert_eq!(known_zero.page_cache_opt(), Some(0));
        let unknown =
            MemoryStats::from_domains(vec![MemoryDomain::new(1000, 500).unwrap()], None).unwrap();
        assert!(!unknown.page_cache_known());
        assert_eq!(unknown.page_cache_opt(), None);
        // Legacy accessor conflates for compat; new code must use opt().
        assert_eq!(unknown.page_cache_bytes(), 0);
    }

    #[test]
    fn meminfo_cache_is_unknown_when_lines_missing_or_malformed() {
        // Both lines present and numeric: known Buffers+Cached.
        let stats = parse_proc_meminfo_stats(
            "MemTotal: 8000 kB\nMemAvailable: 7000 kB\nBuffers: 100 kB\nCached: 500 kB\n",
        )
        .unwrap();
        assert_eq!(stats.page_cache_opt(), Some(600 * 1024));
        // Missing lines: unknown, not zero.
        let stats = parse_proc_meminfo_stats("MemTotal: 8000 kB\nMemAvailable: 7000 kB\n").unwrap();
        assert_eq!(stats.page_cache_opt(), None);
        assert!(!stats.page_cache_known());
        // Malformed line: unknown, not zero and not partial.
        let stats = parse_proc_meminfo_stats(
            "MemTotal: 8000 kB\nMemAvailable: 7000 kB\nBuffers: nope kB\nCached: 500 kB\n",
        )
        .unwrap();
        assert_eq!(stats.page_cache_opt(), None);
        // Signed values are rejected, not coerced.
        let stats = parse_proc_meminfo_stats(
            "MemTotal: 8000 kB\nMemAvailable: 7000 kB\nBuffers: +100 kB\nCached: 500 kB\n",
        )
        .unwrap();
        assert_eq!(stats.page_cache_opt(), None);
    }

    #[test]
    fn admit_error_display_never_reprints_provider_source() {
        let sensitive = ProviderError::new("SENSITIVE token=abc pid=1234");
        let rendered = format!("{}", crate::AdmitError::Provider(sensitive));
        assert!(!rendered.contains("SENSITIVE"));
        assert!(rendered.contains("provider-source-unavailable"));
    }
}
