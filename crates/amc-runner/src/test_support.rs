//! Shared test doubles: temp-dir helpers, fake managers, memory providers.
//!
//! Each helper existed as a near-identical copy in two or more test modules;
//! they live here so clones cannot drift.

#[cfg(all(feature = "systemd", target_os = "linux"))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::provider::{MemoryProvider, MemoryStats, ProviderError};

/// Shared `used_fraction` body so test doubles only differ in `stats`.
#[allow(dead_code)]
pub(crate) fn stats_fraction(
    stats: Result<MemoryStats, ProviderError>,
) -> Result<f64, ProviderError> {
    Ok(stats?.used_fraction())
}

/// Unique temp dir; caller removes it. `prefix` names the owner.
#[allow(dead_code)]
pub(crate) fn scratch_dir(prefix: &str) -> PathBuf {
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    // Concurrent tests must never share a directory, even on equal clock ticks.
    std::fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn concurrent_scratch_directories_have_exclusive_ownership() {
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                (0..32)
                    .map(|_| scratch_dir("amc-scratch-ownership"))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let directories: Vec<_> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    let unique: std::collections::BTreeSet<_> = directories.iter().collect();
    assert_eq!(unique.len(), directories.len());
    for directory in directories {
        std::fs::remove_dir(directory).unwrap();
    }
}

/// Executable fake `systemctl show` manager script.
#[cfg(all(feature = "systemd", target_os = "linux"))]
pub(crate) fn show_manager(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("sh"))
        .find(|p| p.is_file())
        .unwrap();
    let manager = dir.join("manager");
    std::fs::write(&manager, format!("#!{}\n{body}\n", shell.display())).unwrap();
    std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o700)).unwrap();
    manager
}

/// Atomically updatable available bytes; fixed values are the never-mutated
/// case, so this covers both `FixedBytes` and `MutableAvailable`.
#[allow(dead_code)]
pub(crate) struct AtomicAvailable {
    total: u64,
    pub(crate) available: AtomicU64,
}

#[allow(dead_code)]
impl AtomicAvailable {
    pub(crate) fn shared(total: u64, available: u64) -> Arc<Self> {
        Arc::new(Self {
            total,
            available: AtomicU64::new(available),
        })
    }
}

impl MemoryProvider for AtomicAvailable {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        stats_fraction(self.stats())
    }

    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        MemoryStats::new(self.total, self.available.load(Ordering::SeqCst), 0)
    }
}

/// First probe sees pressure (100 available), later probes see full budget.
/// Covers `FlipFlop` and `Probe`.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct FirstLowThenFull {
    calls: AtomicUsize,
}

#[allow(dead_code)]
impl FirstLowThenFull {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl MemoryProvider for FirstLowThenFull {
    fn used_fraction(&self) -> Result<f64, ProviderError> {
        stats_fraction(self.stats())
    }

    fn stats(&self) -> Result<MemoryStats, ProviderError> {
        let calls = self.calls.fetch_add(1, Ordering::SeqCst);
        MemoryStats::new(1024, if calls == 0 { 100 } else { 1024 }, 0)
    }
}
