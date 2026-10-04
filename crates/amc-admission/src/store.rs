//! Private, locked, atomic durable snapshots. Failed persistence is fatal.
use crate::ledger::Ledger;
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const MAX_STATE_BYTES: u64 = 1_048_576;

pub(crate) trait Snapshot: serde::Serialize + serde::de::DeserializeOwned {
    fn validate(&self) -> Result<()>;
    fn boot_id(&self) -> &str;
    fn fresh(boot: &str) -> Self;
}

impl Snapshot for Ledger {
    fn validate(&self) -> Result<()> {
        Ledger::validate(self)
    }
    fn boot_id(&self) -> &str {
        &self.boot_id
    }
    fn fresh(boot: &str) -> Self {
        Ledger::new(boot.into())
    }
}

impl Snapshot for crate::host::HostLedger {
    fn validate(&self) -> Result<()> {
        Self::validate(self)
    }
    fn boot_id(&self) -> &str {
        &self.boot_id
    }
    fn fresh(boot: &str) -> Self {
        Self::new(boot.into())
    }
}

fn validate_boot_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36
            && id.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                }
            }),
        "invalid kernel boot identity; refusing to forget reservations"
    );
    Ok(())
}

pub fn fresh_id() -> Result<String> {
    let id = fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .to_owned();
    ensure!(
        id.len() == 36 && crate::ledger::valid_name(&id),
        "invalid random identity"
    );
    Ok(id)
}

pub fn private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == nix::unistd::geteuid().as_raw()
            && metadata.mode() & 0o077 == 0,
        "admission directory must be owned by this user with mode 0700"
    );
    Ok(())
}

pub struct Store {
    directory: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(directory: &Path, boot_id: &str) -> Result<(Self, Ledger)> {
        Self::open_snapshot(directory, boot_id)
    }

    pub(crate) fn open_snapshot<T: Snapshot>(directory: &Path, boot_id: &str) -> Result<(Self, T)> {
        validate_boot_id(boot_id)?;
        private_directory(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(directory.join("lock"))?;
        lock.try_lock()
            .context("another admission coordinator owns this state")?;
        let path = directory.join("ledger.json");
        let ledger = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                ensure!(
                    file.metadata()?.is_file(),
                    "admission ledger is not a regular file"
                );
                let mut data = Vec::new();
                file.take(MAX_STATE_BYTES + 1).read_to_end(&mut data)?;
                ensure!(
                    data.len() as u64 <= MAX_STATE_BYTES,
                    "admission ledger exceeds size bound"
                );
                let ledger: T = serde_json::from_slice(&data)
                    .context("invalid admission ledger; refusing to forget reservations")?;
                ledger.validate()?;
                validate_boot_id(ledger.boot_id())?;
                if ledger.boot_id() == boot_id {
                    ledger
                } else {
                    T::fresh(boot_id)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    lock.metadata()?.len() == 0,
                    "initialized admission ledger is missing; refusing to forget reservations"
                );
                T::fresh(boot_id)
            }
            Err(error) => return Err(error.into()),
        };
        Ok((
            Self {
                directory: directory.into(),
                _lock: lock,
            },
            ledger,
        ))
    }

    pub fn save(&self, ledger: &Ledger) -> Result<()> {
        self.save_snapshot(ledger)
    }

    pub(crate) fn save_snapshot<T: Snapshot>(&self, ledger: &T) -> Result<()> {
        ledger.validate()?;
        validate_boot_id(ledger.boot_id())?;
        let data = serde_json::to_vec(ledger)?;
        ensure!(
            data.len() as u64 <= MAX_STATE_BYTES,
            "admission ledger exceeds size bound"
        );
        let path = self.directory.join("ledger.next");
        // A leftover next file was never committed. The exclusive state lock
        // makes cleanup safe, and O_NOFOLLOW forbids following a replacement.
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&path, self.directory.join("ledger.json"))?;
        File::open(&self.directory)?.sync_all()?;
        if self._lock.metadata()?.len() == 0 {
            (&self._lock).write_all(b"AMC durable state v1\n")?;
            self._lock.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;
    use crate::{
        host::{HostLedger, Identity, Reservation},
        ledger::ClientIdentity,
    };

    #[test]
    fn host_grants_survive_restart_and_missing_or_corrupt_state_fails_closed() {
        let boot = "01234567-89ab-cdef-0123-456789abcdef";
        let path = std::env::temp_dir().join(format!("amc-host-store-{}", fresh_id().unwrap()));
        let (store, mut ledger) = Store::open_snapshot::<HostLedger>(&path, boot).unwrap();
        ledger.reservations.push(Reservation {
            id: "work".into(),
            domain: "tools".into(),
            identity: Identity {
                cgroup: "/users/tools/job".into(),
                inode: 1,
                uid: 1000,
                pid: 123,
                start_ticks: 5,
            },
            memory_bytes: 50,
            swap_bytes: 0,
            requested_ms: 0,
            deadline_ms: 10,
            granted: true,
            owners: vec![ClientIdentity {
                pid: 123,
                start_ticks: 5,
            }],
        });
        store.save_snapshot(&ledger).unwrap();
        assert!(Store::open_snapshot::<HostLedger>(&path, boot).is_err());
        drop(store);
        let (store, ledger) = Store::open_snapshot::<HostLedger>(&path, boot).unwrap();
        assert_eq!(ledger.committed(), 50);
        drop(store);
        fs::write(path.join("ledger.json"), "broken").unwrap();
        assert!(Store::open_snapshot::<HostLedger>(&path, boot).is_err());
        fs::remove_file(path.join("ledger.json")).unwrap();
        assert!(Store::open_snapshot::<HostLedger>(&path, boot).is_err());
        fs::remove_dir_all(path).unwrap();
    }
}
