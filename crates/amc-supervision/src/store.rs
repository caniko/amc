use crate::{native::bounded_json, recovery::State};
use amc_admission::store::private_directory;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub struct Store {
    directory: PathBuf,
    lock: File,
}
impl Store {
    pub fn open(path: &Path, boot: &str) -> Result<(Self, State)> {
        private_directory(path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path.join("lock"))?;
        lock.try_lock()?;
        let file = path.join("recovery.json");
        let mut state = if file.exists() {
            bounded_json(&file, Some(nix::unistd::geteuid().as_raw()))?
        } else {
            ensure!(
                lock.metadata()?.len() == 0,
                "initialized recovery state missing; cannot forgive attempts"
            );
            State::new(boot.into())
        };
        state.validate()?;
        state.reconcile_startup(boot);
        Ok((
            Self {
                directory: path.into(),
                lock,
            },
            state,
        ))
    }
    pub fn save(&self, state: &State) -> Result<()> {
        state.validate()?;
        atomic_json(&self.directory.join("recovery.json"), state, 0o600, true)?;
        if self.lock.metadata()?.len() == 0 {
            (&self.lock).write_all(b"AMC recovery v1\n")?;
            self.lock.sync_all()?;
        }
        Ok(())
    }
}

pub fn atomic_json(path: &Path, value: &impl Serialize, mode: u32, durable: bool) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= 1_048_576,
        "supervisor state exceeds byte bound"
    );
    let next = path.with_extension("next");
    match fs::remove_file(&next) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&next)?;
    file.write_all(&bytes)?;
    // Service UMask=0077 must not hide the public read-only health heartbeat.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    if durable {
        file.sync_all()?;
    }
    fs::rename(next, path)?;
    if durable {
        File::open(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("state path lacks parent"))?,
        )?
        .sync_all()?;
    }
    Ok(())
}
