use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    config::parse_bytes,
    systemd::{cgroup_directory, parse_unified_cgroup},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default)]
pub struct ExpectedLimits {
    pub memory_max: Option<String>,
    pub memory_swap_max: Option<String>,
    pub memory_high: Option<String>,
    pub memory_oom_group: Option<u8>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfReport {
    pub cgroup_path: String,
    pub memory_max: String,
    pub memory_swap_max: String,
    /// Kernel memory.high, or "unknown" with reason when unreadable.
    pub memory_high: String,
    pub memory_oom_group: String,
}

pub fn self_report(output: Option<&Path>, expected: &ExpectedLimits) -> Result<SelfReport> {
    let cgroup_text =
        fs::read_to_string("/proc/self/cgroup").context("failed to read /proc/self/cgroup")?;
    let cgroup_path = parse_unified_cgroup(&cgroup_text)?;
    let directory = cgroup_directory(&cgroup_path)?;
    let report = SelfReport {
        cgroup_path,
        memory_max: read_value(directory.join("memory.max"))?,
        memory_swap_max: read_value(directory.join("memory.swap.max"))?,
        // memory.high may be absent on older kernels: report unknown, not zero.
        memory_high: read_value(directory.join("memory.high"))
            .unwrap_or_else(|error| format!("unknown ({error:#})")),
        memory_oom_group: read_value(directory.join("memory.oom.group"))?,
    };
    check_expected(
        "memory.max",
        &report.memory_max,
        expected.memory_max.as_deref(),
    )?;
    check_expected(
        "memory.swap.max",
        &report.memory_swap_max,
        expected.memory_swap_max.as_deref(),
    )?;
    if let Some(high) = expected.memory_high.as_deref() {
        check_expected("memory.high", &report.memory_high, Some(high))?;
    }
    if let Some(expected) = expected.memory_oom_group {
        if expected > 1 {
            bail!("expected memory.oom.group must be 0 or 1");
        }
        if report.memory_oom_group != expected.to_string() {
            bail!(
                "memory.oom.group is {}, expected {expected}",
                report.memory_oom_group
            );
        }
    }

    let json = json_bytes(&report)?;
    std::io::stdout().write_all(&json)?;
    if let Some(output) = output {
        atomic_write(output, &json)?;
    }
    Ok(report)
}

fn check_expected(name: &str, actual: &str, expected: Option<&str>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = parse_bytes(expected)?;
    if actual == "max" || actual.parse::<u64>().ok() != Some(expected) {
        bail!("{name} is {actual}, expected {expected}");
    }
    Ok(())
}

fn read_value(path: PathBuf) -> Result<String> {
    fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))
        .map(|value| value.trim().to_owned())
}

pub fn hog(
    report_output: Option<&Path>,
    expected: &ExpectedLimits,
    chunk_size: &str,
    maximum: &str,
    progress_interval: &str,
    delay_ms: u64,
) -> Result<()> {
    self_report(report_output, expected)?;
    let chunk_size = parse_bytes(chunk_size)?;
    let maximum = parse_bytes(maximum)?;
    let progress_interval = parse_bytes(progress_interval)?;
    if chunk_size == 0 || maximum == 0 || progress_interval == 0 {
        bail!("chunk size, maximum, and progress interval must be greater than zero");
    }

    let mut chunks = Vec::new();
    let mut allocated = 0_u64;
    let mut next_progress = progress_interval;
    while allocated < maximum {
        let amount = chunk_size.min(maximum - allocated);
        let amount = usize::try_from(amount).context("chunk size exceeds this platform's usize")?;
        let mut chunk = vec![0_u8; amount];
        chunk.fill(0xa5); // Commit every page rather than relying on virtual allocation.
        chunks.push(chunk);
        allocated += amount as u64;
        if allocated >= next_progress || allocated == maximum {
            eprintln!("hog allocated {allocated} bytes");
            while next_progress <= allocated {
                next_progress = next_progress.saturating_add(progress_interval);
                if next_progress == u64::MAX {
                    break;
                }
            }
        }
        if delay_ms != 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    std::hint::black_box(chunks);
    Ok(())
}

/// Finite useful-work task: touches a bounded working set with
/// incompressible-ish data (xorshift, not zeros), checksums each iteration,
/// and records per-iteration latency. A surviving heartbeat alone is not
/// proof of usability; completions + latency are.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkReport {
    pub iterations_completed: u64,
    pub bytes_touched: u64,
    pub checksum: u64,
    pub max_iteration_us: u64,
    pub median_iteration_us: u64,
    pub p95_iteration_us: u64,
}

pub fn work(
    report_output: Option<&Path>,
    expected: &ExpectedLimits,
    size: &str,
    iterations: u64,
) -> Result<WorkReport> {
    // Helper-entry observation before allocation, not a loader/constructor guarantee.
    self_report(None, expected)?;
    let size = parse_bytes(size)?;
    if size == 0 || iterations == 0 {
        bail!("size and iterations must be greater than zero");
    }
    if size > 1 << 31 {
        bail!("size must not exceed 2GiB per iteration");
    }
    if iterations > 10_000 {
        bail!("iterations must not exceed 10000 (bounded fixture)");
    }
    let size = size as usize;
    let mut state: u64 = 0x9e3779b97f4a7c15;
    let mut checksum: u64 = 0;
    let mut latencies = Vec::with_capacity(iterations as usize);
    let mut bytes_touched = 0_u64;
    for _ in 0..iterations {
        let start = Instant::now();
        let mut chunk = vec![0_u8; size];
        // xorshift64*: cheap, not trivially compressible, touches every page.
        for word in chunk.chunks_mut(8) {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let value = state.wrapping_mul(0x2545f4914f6cdd1d);
            word.copy_from_slice(&value.to_le_bytes()[..word.len()]);
        }
        // Checksum so the work cannot be optimized away.
        let mut acc: u64 = 0;
        for word in chunk.chunks(8) {
            let mut bytes = [0; 8];
            bytes[..word.len()].copy_from_slice(word);
            acc = acc.wrapping_add(u64::from_le_bytes(bytes));
        }
        checksum = checksum.wrapping_add(acc);
        std::hint::black_box(&chunk);
        bytes_touched += size as u64;
        latencies.push(micros(start.elapsed()));
    }
    latencies.sort_unstable();
    let report = WorkReport {
        iterations_completed: iterations,
        bytes_touched,
        checksum,
        max_iteration_us: *latencies.last().unwrap_or(&0),
        median_iteration_us: percentile(&latencies, 50),
        p95_iteration_us: percentile(&latencies, 95),
    };
    let json = json_bytes(&report)?;
    std::io::stdout().write_all(&json)?;
    if let Some(output) = report_output {
        atomic_write(output, &json)?;
    }
    Ok(report)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatSample {
    sequence: u64,
    scheduled_us: u64,
    observed_us: u64,
    gap_us: u64,
    delay_us: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatSummary {
    pub count: usize,
    pub max_gap_us: u64,
    pub median_delay_us: u64,
    pub p95_delay_us: u64,
    pub p99_delay_us: u64,
}

pub fn heartbeat(
    samples_output: &Path,
    summary_output: &Path,
    interval_ms: u64,
    duration_ms: u64,
) -> Result<HeartbeatSummary> {
    if interval_ms == 0 || duration_ms < interval_ms {
        bail!("interval must be positive and duration must be at least one interval");
    }
    let interval = Duration::from_millis(interval_ms);
    let duration = Duration::from_millis(duration_ms);
    let start = Instant::now();
    let end = start + duration;
    let mut next = start + interval;
    let mut previous = start;
    let mut samples = Vec::new();

    while next <= end {
        let now = Instant::now();
        if now < next {
            thread::sleep(next - now);
        }
        let observed = Instant::now();
        samples.push(HeartbeatSample {
            sequence: samples.len() as u64 + 1,
            scheduled_us: micros(next.duration_since(start)),
            observed_us: micros(observed.duration_since(start)),
            gap_us: micros(observed.duration_since(previous)),
            delay_us: micros(observed.saturating_duration_since(next)),
        });
        previous = observed;
        next += interval;
    }

    let mut delays: Vec<_> = samples.iter().map(|sample| sample.delay_us).collect();
    delays.sort_unstable();
    let summary = HeartbeatSummary {
        count: samples.len(),
        max_gap_us: samples
            .iter()
            .map(|sample| sample.gap_us)
            .max()
            .unwrap_or(0),
        median_delay_us: percentile(&delays, 50),
        p95_delay_us: percentile(&delays, 95),
        p99_delay_us: percentile(&delays, 99),
    };
    atomic_write(samples_output, &json_bytes(&samples)?)?;
    atomic_write(summary_output, &json_bytes(&summary)?)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(summary)
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len().saturating_sub(1))]
}

fn json_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .context("output path must have a file name")?
        .to_string_lossy();
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values: Vec<_> = (1..=100).collect();
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&values, 99), 99);
    }

    #[test]
    fn expected_limit_rejects_max_and_accepts_exact_bytes() {
        assert!(check_expected("memory.max", "max", Some("1GiB")).is_err());
        assert!(check_expected("memory.max", "1073741824", Some("1GiB")).is_ok());
        assert!(check_expected("memory.max", "1", Some("1GiB")).is_err());
    }
}
