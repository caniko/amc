//! Fail-closed heartbeat shared by the host supervisor and per-user admission.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub version: u32,
    pub boot_id: String,
    pub observed_boot_ms: u64,
    pub inhibit: bool,
    pub degraded: bool,
}

pub use crate::clock::boot_ms;

impl Health {
    pub fn permits(&self, boot: &str, now: u64) -> bool {
        self.version == 1
            && self.boot_id == boot
            && now >= self.observed_boot_ms
            && now - self.observed_boot_ms <= 3000
            && !self.inhibit
            && !self.degraded
    }
}

/// Only a regular, non-writable-by-others root/operator-owned heartbeat can
/// permit admission. Missing, stale, malformed and future timestamps pause.
pub fn permits(path: &Path) -> Result<bool> {
    let file: File = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.mode() & 0o022 == 0
            && (metadata.uid() == 0 || metadata.uid() == nix::unistd::geteuid().as_raw()),
        "untrusted supervisor heartbeat"
    );
    let mut bytes = Vec::new();
    file.take(16_385).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16_384,
        "supervisor heartbeat exceeds size bound"
    );
    let health: Health = serde_json::from_slice(&bytes)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(health.permits(boot.trim(), boot_ms()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_future_reboot_or_inhibition_cannot_admit() {
        let mut h = Health {
            version: 1,
            boot_id: "boot".into(),
            observed_boot_ms: 10_000,
            inhibit: false,
            degraded: false,
        };
        assert!(h.permits("boot", 13_000));
        assert!(!h.permits("boot", 13_001));
        assert!(!h.permits("boot", 9999));
        assert!(!h.permits("other", 10_000));
        h.inhibit = true;
        assert!(!h.permits("boot", 10_000));
        h.inhibit = false;
        h.degraded = true;
        assert!(!h.permits("boot", 10_000));
    }
}
