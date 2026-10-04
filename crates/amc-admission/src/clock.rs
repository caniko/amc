//! Scheduling deadlines use the current boot's monotonic clock, including suspend.
use anyhow::Result;

pub fn boot_ms() -> Result<u64> {
    let t = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)?;
    Ok(u64::try_from(t.tv_sec())?
        .saturating_mul(1000)
        .saturating_add(u64::try_from(t.tv_nsec())? / 1_000_000))
}
