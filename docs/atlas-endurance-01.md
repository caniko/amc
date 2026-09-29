# Atlas Endurance Capture 01 — Data-Quality Report

Recorded: 2026-09-20, ~21:19–21:49 local (1800s window).

Status: observation report, not a performance claim. Aggregate Steam scope
telemetry; must not be presented as game-specific evidence.

## Capture

- Observer: release `target/release/amc` 0.1.0, sha256
  `05c94a2b68f27fb10032394a38f186411cda1551453b97aa41ce8b04d8b44520`,
  run as transient user unit `amc-endurance-01.service` (invocation
  `8f2a3cd22f0f4019a669029740937ed6`).
- Command: `amc watch app-cosmic-steam-1439422.scope --production --seconds 1800`
  (1 Hz), output `/data/scratch/tmp/opencode/amc-atlas-endurance-02/`.
- Target: `/user.slice/user-1000.slice/user@1000.service/app.slice/app-cosmic-steam-1439422.scope`,
  invocation `0ca3f44cd62147e79327fd282f3ba554`, unchanged for the whole run.
- A first backgrounded launch was cancelled after 2 samples; shell reaping is the
  suspected cause (see the adoption plan), not an established one. Its directory
  (`amc-atlas-endurance-01`) validates as structurally sound but incomplete
  (`reason: cancelled`, `complete: false`) and is aborted-run evidence only.

## Collection Integrity

| Gate | Evidence |
|---|---|
| Terminal state | `reason: deadline`, `complete: true`, valid baseline, durable storage |
| Sample reconciliation | 1800 attempted / 1800 persisted; sequences 0–1799 contiguous; monotonic span 1799.0s |
| Scheduling | 0 missed intervals; schedule lag max 2ms, p99 0ms |
| Read cost | Per-sample capture max 1911µs, p99 359µs (read-path timing, not whole-observer CPU) |
| Output size | 5,103,053 bytes (~2.8KB/sample; ~2% of the 256MiB cap) |
| Coverage | Zero unknown host cells across 1800 × 14 fields; single invocation, no restart |
| Identity | One observation ID, clock domain, unit, cgroup, invocation, and boot ID across manifest, summary, and all samples |
| Target events | `high/low/max/oom/oom_kill/oom_group_kill/sock_throttled` deltas all 0 |

## Aggregate Observations (Host Scope, Not Target Attribution)

Derived with `scripts/validate-capture.py` from the capture itself: rates use
the sample monotonic span (1799.0s); PSI figures are percent of that elapsed
time; p99 is nearest-rank. Counter units are as observed; page size was not
recorded.

- Host `MemAvailable`: 35.78GB → 29.27GB, minimum 29.11GB.
- Host `SwapFree`: 3.08GB → 3.43GB, minimum 2.58GB (rose overall while paging
  continued; cause not established).
- Swap activity (cumulative pages — do not convert to bytes):
  `pswpin` +498,160 (~277/s), `pswpout` +186,477 (~104/s).
  Major faults +1,453,562 (~808/s).
- Host memory pressure: `some` +4.88M µs (0.272% of elapsed),
  `full` +3.95M µs (0.220% of elapsed). For context, host CPU `some` was 5.62%
  (full 0%) and host IO `some` 14.80% (full 8.75%) over the same window.
- Target sampled maxima: `memory.current` 3.12GB, `memory.swap.current` 3.84GB.
- No OOM or limit events on the target during the window.

## Explicit Non-Claims

No frame times, VRAM data, background useful-work accounting, causality between
pressure and any stutter, or AMC performance benefit. Swap counters are pages,
not bytes. Observer CPU time was not measured (only ~4MB RSS via `ps`);
capture timing covers reads, not total observer cost.

## Follow-Ups

1. Named game session with verified process/cgroup ownership and frame-time logging.
2. Measure observer CPU (e.g. `systemd-run --property=CPUAccounting` + `systemctl show`
   `CPUUsageNSec`, or `pidstat`) during the next long capture.
3. If manual analysis repeats, add one small offline session-summary command using
   this observed format — only then.
