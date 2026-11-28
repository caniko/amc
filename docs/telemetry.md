# AMC Telemetry

`amc inspect`, `amc watch`/`amc diff`, and the `amc-runner` cgroup provider
(strict current/cache parsing) share `crates/amc-telemetry`: bounded readers
with pinned multi-sample handles, strict digits-only parsers, identity, and
comparison. No systemd transport, Tokio runtime, subscribers, exporters, or
`unsafe` in the core.

## What each surface answers

- `amc inspect UNIT`: manager-reported settings (`systemctl show`
  allowlist) vs kernel observations (leaf through visible root) vs
  requested policy (always `unknown`: TOML is not effective policy).
- `amc watch UNIT --seconds N --interval-ms M --output DIR`: finite, read-only
  observation of one unit. Writes `samples.jsonl`, versioned atomic
  `summary.json` (`complete` requires checks below), `manifest.json` (real
  `systemctl --version`, not a resolver probe), `ready` after persisting a valid
  baseline, and `done` after persisting the summary. SIGINT/SIGTERM records
  `cancelled`; an unhandled kill leaves no summary and thus incomplete evidence.
- `amc diff BEFORE AFTER`: offline comparison of two Snapshot files with
  per-field supported/unsupported outcomes. Inputs are size-checked before
  reading; only file fields establish identity.
- `amc report CAPTURE_DIR [--json]`: offline validation and summary of one
  watch capture. Markdown by default; JSON includes coverage, suppressed
  metrics, and collection integrity. Requires Python 3.11+ at runtime (supplied
  by the Nix package). Malformed artifacts exit 2; structurally valid but
  incomplete captures are reported as incomplete. Reuses `scripts/validate-capture.py`
  for sample lifetimes and never queries or changes the target.
- `amc-runner` diagnostics: structured `tracing` events with stable names
  (`amc.admission.*`) and reason codes, plus `diagnostics()` snapshots
  with real timeout/waiter/transition counters. The library installs no
  subscriber; see
  `crates/amc-runner/examples/admission-diagnostics.rs`.

## Production capture

Production observation is opt-in and passive: the same identity-aware
collector never starts, stops, freezes, moves, or changes the target.
Select the native service/scope that owns the work, not an activation client
or assumed universal Steam unit. The output directory must be new and its parent
must exist; the collector rejects symlink parents.

```sh
# Run alongside normal work; this command does not start the workload.
amc watch actual-workload.service --production --output /tmp/amc-session-01

# One hour, sampling every two seconds. Use --system for a system-manager unit.
amc watch actual-workload.service --production --seconds 3600 --interval-ms 2000 \
  --output /tmp/amc-session-02

# Inspect an existing capture without querying or modifying the workload.
jq '{reason, complete, coverage, collection}' /tmp/amc-session-02/summary.json
jq -c '{time: .host.observedUnixMs, available: .host.files["meminfo.MemAvailable"], memoryPressure: .host.files["pressure.memory"]}' \
  /tmp/amc-session-02/samples.jsonl
```

Replace the example unit with the discovered owner. Choose the output location
deliberately: logs contain unit/cgroup identifiers, timestamps, boot identity,
and aggregate resource activity. Nothing uploads. The observer grants no special
privileges; inaccessible measurements stay unknown.

| Mode | Default | Allowed duration / interval | Sample storage ceiling |
|---|---|---|---|
| Ordinary `watch` | 40 seconds / 20 ms | 1-60 seconds / 10-1000 ms | 5000 samples, 8 MiB |
| `watch --production` | 1800 seconds / 1000 ms | 1-86400 seconds / 1000-60000 ms | 86,400 samples, 256 MiB |

Duration, sample count, and bytes are independent limits. Hitting a storage
limit ends collection with an explicit incomplete reason: no rotation,
automatic deletion, or silent overwrite. The byte cap covers `samples.jsonl`;
bounded manifest/summary/marker files add a little. It reserves no disk space
and may not suffice for longer runs.

Ctrl-C or SIGTERM stops only the observer, saves the readable prefix if
possible, and records `cancelled`/`complete:false`. Sleep checks cancellation
at most every 100 ms and never passes the sampling deadline. Manager queries
keep their timeout/grace bounds; arbitrary blocked kernel I/O has no hard
cancellation deadline. SIGKILL can leave no summary. No policy changes or
rollback.

Each tick flushes production samples to the OS for live readers. `ready`
requires a synced valid target baseline; claim sample durability only after the
final flush/sync succeeds, not a flush alone. The process exits on target
disappearance or lifetime change: no following restarts or collecting unbounded
sessions. Missing `done` or summary means incomplete evidence, even with exit
zero.

### Host context

Production adds `host` per sample: its own wall-clock and observer-relative
monotonic capture-start timestamps, plus a map of `{value, unknown}` measurements:

- `meminfo.MemTotal`, `MemAvailable`, `SwapTotal`, `SwapFree`, `Buffers`, `Cached`,
  `Dirty`, and `Writeback` (all prefixed `meminfo.`): **bytes** from procfs `kB`
  via checked multiplication by 1024.
- `vmstat.pswpin` and `vmstat.pswpout`: cumulative **pages**, not bytes.
  `vmstat.pgmajfault`: cumulative major faults. None of these counters is a
  per-interval delta or attributable to the target unit.
- `pressure.memory`, `pressure.cpu`, and `pressure.io`: PSI averages in percent
  and cumulative `total` in microseconds. Older CPU PSI may omit `full`;
  absent stays absent, never zero. System-level CPU `full` does not measure
  application responsiveness.

The module allowlists fields, rejects duplicate keys and invalid units/numbers,
and caps meminfo/vmstat input at 64 KiB each and pressure files at 4 KiB each.
It emits neither malformed input nor unrelated fields. Each missing field is
unknown. Older CPU PSI does not relax strict memory/IO PSI parsing. No process
listings, argv, environment, device identifiers, or journal messages.

`host` is the observer's potentially restricted or virtualized `/proc` view,
not necessarily the whole machine and not the target's cgroup budget. Reads
are sequential, not atomic. `complete` describes target collection, not host
metric presence. Check each unknown reason. `amc diff` compares target
snapshots, not host aggregates.

### Timing and interpretation

Samples add `captureDurationUs` (wall time for target/optional host reads,
not total observer CPU time) and `scheduleLagMs`. Summary `collection` adds
`bytesWritten`, `missedIntervals`, and `maxCaptureDurationUs`. Skip missed
slots rather than sampling in bursts. Manager identity checks run only at
ticks, at most once per second; a 60-second interval can delay restart detection.
Pinned-reader and final-query guards prevent pathname matches alone from
establishing continuity.

Manifest fields `production`, `maxSamples`, `maxOutputBytes`, and `hostScope`
record collection settings without changing defaults or field meanings, and
select no memory priority profile. Unit observers capture separately, with their
own budgets/host reads and relative monotonic clocks starting at zero; do not
equate relative timestamps across observers. Correlate using retained wall-clock
timestamps and boot IDs; account for clock adjustments and sample duration.

For gaming, record frame times separately (e.g. with MangoHud) and keep its
clock/logging configuration. AMC measures neither frame times, VRAM, background
useful-work completion, nor causality. Record game, driver, kernel, swap/zram,
effective ancestry, and binary/source identity. Low PSI does not prove good
frame times; a stall does not prove memory caused it. Observation alone is
not a validated A/B/C experiment or proof of low overhead on every workstation.

### Accounted observation

`scripts/capture-accounted.py`, a stdlib-only Python 3.11+ pilot runner, starts
only an observer in a uniquely named transient **user** service. The target
must exist; its placement, limits, swap, and OOM policy stay unchanged. Use a
validated release build to capture normal work:

```sh
python3 scripts/capture-accounted.py actual-workload.scope \
  --amc target/release/amc --seconds 1800 --output /tmp/amc-accounted-01
python3 scripts/validate-capture.py /tmp/amc-accounted-01/capture --markdown
```

`--system` selects a system-manager **target**, not a privileged observer.
Use a new session directory under an existing parent on a filesystem that
can execute the binary copy. The runner creates a private 0700 directory,
copies/checksums the binary, and writes 0600 JSON records:

- `session.json`: observer name, target manager/unit, requested sampling,
  binary SHA-256, and accounting scope.
- `capture/`: the unchanged manifest/samples/summary/markers produced by `watch`.
- `accounting.json`: final systemd counters, units, unknowns, invocation,
  process start/exit timestamps, and manager result.
- `cleanup.json`: successful stop of the observer after durable accounting export.

The runner configures the observer with native CPU/memory/IO accounting and
`RemainAfterExit=yes` rather than immediate `--collect`. The runner reads
retained final counters, flushes and syncs the export, then stops its own unit.
An invocation change during polling aborts control. Do not restart or replace
the observer during capture.

CPU time is in nanoseconds, peak memory in bytes, and IO in block-IO bytes
(not JSON file lengths) or operation counts. Missing fields, unsupported
counters, and systemd's `UINT64_MAX` become `value:null` with
`unknown:unavailable`; zero stays known. Start/exit timestamps use systemd's
monotonic microseconds, not capture-relative milliseconds. Final counters
can outlast `ControlGroup` becoming empty.

Accounting covers the observer and its child queries, not the outer Python
launcher, service manager, or other services working on their behalf. It
neither measures total host overhead caused by observation nor rules out
frame-time impact. The copied executable and session metadata remain local;
review them before sharing artifacts.

On Ctrl-C/SIGTERM, the runner sends SIGINT only to the observer's main process
and exports retained counters before cleanup. The manager bounds the running
observer to the requested duration plus 60 seconds, with a 10-second stop
timeout. Each manager command has a 20-second timeout. Ambiguous submission, identity,
timeout, or export failures trigger neither resubmission nor blind cleanup.
Inspect `session.json` and the named unit: a failed export can leave partial
JSON and a retained unit needing manual recovery. Observer exit does not
imply export.

Runner success confirms its process/accounting lifecycle only; validate
capture completeness separately. Long-session overhead and broad
distribution/version compatibility remain unvalidated.

## Classification and comparison

- `memory.events*`: scoped counters. Deltas require compatible
  identity/lifetime and monotonic counters; decreases get an explicit reason.
- `cgroup.events`: state transitions (`populated`/`frozen`), no blanket deltas.
- Usage/limits: sampled gauges (timed before/after, not budget accounting).
- `memory.peak*`: kernel high-water marks. An interval peak is not
  `after - before`; sampled maxima (`maxObservedSwap`, `maxObservedCurrent`)
  are separate.
- `memory.pressure`: retain reported avg10/60/300. Interval stall fractions
  (`pressureStall.some/full`) divide the `total` delta by valid elapsed
  microseconds (both in microseconds). A delta larger than elapsed time is
  `incompatible-clocks`, never clamped.

An observation ID names a session, not a workload. `Compatible` requires
positive continuity evidence: equal invocation IDs **and** no contradictory
inode. A recycled path under a stale ID is `replacement-suspected`, not
one lifetime; reboot ends the lifetime even with equal IDs. A path match
alone never suffices. One-sided or missing invocation evidence gives
`missing-identity`; boot/manager-context/UID disagreement gives
`context-mismatch` or `restart-detected`. Snapshots carry
`invocationId`/`bootId`/`inode` only when the collector verified them; never
diff files by path in their absence. Reject overlong identities rather than
risk collisions by truncating them. Collect `InvocationID` if the manager
reports it; `boot_id` is best-effort.

## Completion and coverage (watch)

`complete` requires a valid baseline, persisted outputs, and a clean terminal
state (`deadline` or `disappeared-or-empty`). All other exits—including
missing baseline (`observer-not-ready`), restart (`invocation-changed`),
same-path replacement (`replacement-suspected`), exhausted output or sample
budget, write failure, and cancellation—are incomplete, with explicit
`coverage` flags (`attachedLate` for passive attachment after workload start,
`baselineDelayed` for a baseline after the first tick,
`finalCountersUnavailable`, and `incompletePersistence`) and
`deltaUnsupportedReason` instead of an unsupported number; a last-readable
delta is not lifetime accounting.

An independent final leaf query supplies termination result and endpoint
identity. Unavailable results stay unknown; a restart in the final gap
rewrites `deadline` instead of producing cross-lifetime deltas. If the path
vanishes, the observer still reports any verified readable prefix from the held
object (with `finalCountersUnavailable`), but positive evidence of restart,
replacement, or context mismatch overrides disappearance. Each derived field
uses its own capture time, never a newer tick's clock for stale counters or PSI
totals. Stall fractions also need a compatible lifetime.

Publish `ready` only after flushing and syncing the baseline sample. The
observer opens metric files, including initially unavailable ones, relative
to a held directory handle via Linux `/proc/self/fd`. Pinned file descriptions
ensure a check-to-read race can only read the original object. It checks inode
continuity each tick and polls the manager at most once per second, using a
leaf-only query bounded by the remaining sampling budget (no ancestor
traversal). Cleanup never touches the target. `samples.jsonl` uses the shared
Snapshot envelope: `amc diff` reads watch output directly and byte-limits
inputs while reading rather than by metadata size.

`eventDeltaCoverage` gives the baseline-to-last-readable interval, its elapsed
endpoints, and final counter availability. A vanished path does not erase
captured counters: a supported prefix can survive unknown final coverage.
`complete` means collection completion, not full workload accounting.
`eventDeltaCoverage.lifetimeComplete` is always false for passive attachment.
`amc diff` checks loaded JSON before comparison for supported schema, bounded
nonempty identities, coherent measurements, and metric shape.

Both `amc report` formats preserve these distinctions. JSON pairs
`metrics.targetCoverage` (`knownPoints`/`totalPoints`) with `targetSampledMax`,
which uses only known readings; absence means unavailable, not zero.
Markdown shows sample counts and flags partial coverage. The report also
shows collection flags, missed intervals, durability, and the summary's
event interval and delta limitation. It normalizes flat or nested producer
endpoints and rejects conflicts. Valid Unicode stays readable; escaped lone
surrogates render as literal escapes to keep the report valid UTF-8.

Each loop exit records a `LoopOutcome`. The terminal verdict combines it
with independent endpoint comparability (`Intact`, `Restarted`, `Replaced`,
`Disappeared`, `EndpointUnknown`), not reason strings. Prefix deltas use the
freshest map for intact, disappeared, or unconfirmed endpoints. A confirmed
restart or replacement uses the verified pre-change snapshot (last successful
invocation poll); post-change ticks retain old labels until the next poll and
never feed those deltas. Each derived quantity has its own start and end
marks (elapsed time and sample sequence); `pressureStallCoverage` mirrors
counter coverage. A failed final query adds `endpoint-query-failed`:
collection finished, confirmation did not.

`collection` separates attempted samples from buffered (`persistedSamples`)
and synced (`storageDurable`) ones. Last-readable values keep their per-field
capture marks, not a newer tick's timestamp. Final counters require an intact
terminal, live endpoint, readable last-persisted counters, and durable
persistence. A failed final write leaves stale earlier values unavailable.
If writing `done` fails, the observer repairs the summary to incomplete and
returns the original error. Missing `done` is never success. Existing
`summary.json` consumers ignore unknown additive fields (`collection`,
`pressureStallCoverage`, `endpoint-query-failed`).
`finalCountersAvailable` now requires durability and can flip `true`→`false`
where a persistence failure previously left a stale value marked available.

## Admission diagnostics

- Assign state under the admission lock, then format and run subscribers
  after releasing it. Weighted provider I/O uses separate cache publication,
  never the admission mutex.
- RAII guards reconcile waiter counts on async cancellation. Record
  timeouts and wait milliseconds on every exit.
- A deliberately disabled gate reports `Disabled`; only runtime fail-open
  degradation reports `ThreadCapOnly`.
- `WaitReason` separates global `Pressure` from request-specific
  `Budget`/`Exclusivity`/`PageCache`. Emit resume only on admission.
- Page-cache fractions divide by the cache's own domain total (leaf
  limit, else host total) — never by a minimum aggregated across
  independently limited domains. Missing or malformed host `Buffers`/`Cached`
  lines are unknown, not zero; sysinfo has no cache accounting. Unknown
  accounting skips the check, keeps the observation usable, and sets
  `cacheUnknown`, unless `requirePageCacheBytes` makes it an explicit policy
  failure honoring fail-open/closed. Infer a missing denominator only for
  single-domain observations.
- All waiter counts exclude probing iterations; `waitersByReason` breaks down
  parked requests by reason. Report `waitReason` only while requests are parked;
  another request's admission never clears it. RAII guards reconcile durations even on async cancellation.
  Construct permits before subscriber code so a panic unwinds through a live
  permit without leaking the reservation. Weighted provider I/O uses an
  epoch-ordered probe cache: releases invalidate by epoch, concurrent probes
  publish only if unsuperseded, and admission rechecks that generation under
  state/cache locks. A disabled/degraded gate never probes.
- The refactor leaves thresholds, fail-open policy, hysteresis, and
  conservative reservation accounting unchanged.

## Privacy

Unknown is never zero; never reprint malformed bytes. No journal
bodies, `ExecStart`, argv, environment values, command contents, or raw
provider/subprocess text in diagnostics. `ProviderError::Source` maps to
`provider-source-unavailable` / `provider-unsupported`, including inside
formatted error chains. A regression test uses a sensitive provider and an
enabled subscriber that captures every event field.

## Limits and overhead

Historical single-run debug-build microbenchmarks on this host (synthetic
cgroup dir, fail-open provider; not remeasured for the generation and
directory-handle fixes):

- Snapshot: ~56µs/sample, ~1KiB/sample JSON. At 20ms intervals this is
  ~0.3% of one CPU and ~50KiB/s; a 40s run is ~2MiB (8MiB cap,
  5000-sample cap, 60s/10ms bounds).
- Weighted admission: ~0.8µs per acquire+diagnostics+release (fail-open
  provider, debug build).

These order-of-magnitude smoke results are not production profiles: no
peak-RSS, enabled-vs-disabled diagnostics split, release-build data, or
latency distribution. They predate production host context and do not
estimate its cost.

Sampling is sequential, not atomic. No implementation can guarantee
readable counters after cgroup destruction, recover unwritten evidence after
SIGKILL, or hard-bound arbitrary blocking kernel/filesystem operations. Report
these as unknown/incomplete, not success. No baseline bridges restarts.

## Compatibility

- `Snapshot`/`Inspection` JSON is additive (`schemaVersion`,
  `observedMonotonicMs`, `sequence`, `bootId`, `invocationId`, `inode`,
  `manifest` fields, `deltaUnsupportedReason`, `pressureStall*`). Legacy
  consumers ignore unknown fields; legacy files without identity fields
  compare as `missing-identity`.
- The deprecated Python `scripts/watch-cgroup.py` parser matches the Rust
  core byte-for-byte until deletion (digits-only u64, strict keys, finite PSI
  ranges). VM OOM evidence now comes from `amc watch`: the driver asserts
  `eventDeltas.oom > 0`, `maxObservedSwap == 0`, and `Result == oom-kill`.
  It asserts the `oom` delta, not `oom_kill`: the observed kernel reports the
  group OOM event without charging either kill counter to the unit cgroup.
  Asserting `oom_kill` would assume unverified kernel accounting.
- If a tick's lookup finds no directory, report `disappeared-or-empty`,
  never suspected replacement. Only a different present inode identifies
  a new object. Collected cgroups keep the verdict and readable prefix.
