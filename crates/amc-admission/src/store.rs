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
                let ledger: Ledger = serde_json::from_slice(&data)
                    .context("invalid admission ledger; refusing to forget reservations")?;
                ledger.validate()?;
                validate_boot_id(&ledger.boot_id)?;
                if ledger.boot_id == boot_id {
                    ledger
                } else {
                    Ledger::new(boot_id.into())
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    lock.metadata()?.len() == 0,
                    "initialized admission ledger is missing; refusing to forget reservations"
                );
                Ledger::new(boot_id.into())
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
        ledger.validate()?;
        validate_boot_id(&ledger.boot_id)?;
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
