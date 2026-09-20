//! Process-wide signal ownership belongs to the CLI, never the runner library.
use anyhow::{Result, bail};
use std::{
    ffi::c_int,
    sync::atomic::{AtomicI32, Ordering},
};

pub use amc_runner::systemd::{QUERY_TIMEOUT, capture, capture_with_timeout};

static CANCELLED: AtomicI32 = AtomicI32::new(0);

// Linux libc ABI on the two supported targets.
unsafe extern "C" {
    fn signal(number: c_int, handler: usize) -> usize;
}

extern "C" fn cancel(number: c_int) {
    CANCELLED.store(number, Ordering::Relaxed);
}

pub struct Signals([usize; 2]);

impl Signals {
    pub fn install() -> Result<Self> {
        CANCELLED.store(0, Ordering::Relaxed);
        let mut previous = [0; 2];
        for (i, number) in [2, 15].into_iter().enumerate() {
            // SAFETY: static C ABI handler performs only an atomic store.
            previous[i] = unsafe { signal(number, cancel as *const () as usize) };
            if previous[i] == usize::MAX {
                if i == 1 {
                    // SAFETY: restore the handler returned by signal().
                    unsafe { signal(2, previous[0]) };
                }
                bail!("could not install launch cancellation handlers");
            }
        }
        Ok(Self(previous))
    }

    pub fn cancelled(&self) -> Option<i32> {
        match CANCELLED.load(Ordering::Relaxed) {
            0 => None,
            number => Some(number),
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for (number, handler) in [2, 15].into_iter().zip(self.0) {
            // SAFETY: restore exactly the previous process handlers.
            unsafe { signal(number, handler) };
        }
    }
}
