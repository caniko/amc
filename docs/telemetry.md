# AMC Telemetry

Shared core: `crates/amc-telemetry` (bounded readers including pinned
multi-sample handles, strict digits-only parsers, identity, comparison).
Used by `amc inspect`, `amc watch`/`amc diff`, and the `amc-runner`
cgroup provider (strict current/cache parsing). No systemd transport,
Tokio runtime, subscribers, or exporters in the core. No `unsafe`.

## What each surface answers

- `amc inspect UNIT`: manager-reported settings (`systemctl show`
  allowlist) vs kernel observations (leaf through visible root) vs
  requested policy (always `unknown`: TOML is not effective policy).
- `amc watch UNIT --seconds N --interval-ms M --output DIR`: finite
  read-only observation of one unit. Writes `samples.jsonl`,
  versioned `summary.json` (atomic; `complete` earned, see below),
  `manifest.json` (real `systemctl --version`, not a resolver probe),
  `ready` (only after a persisted valid baseline), `done` (only after the
  summary persists). Handles SIGINT/SIGTERM with an explicit `cancelled`
  reason; an unhandled kill leaves no summary, which is itself
  recognizable as incomplete.
- `amc diff BEFORE AFTER`: offline comparison of two Snapshot files with
  per-field supported/unsupported outcomes. Inputs are size-checked
  before reading; identity comes only from fields the files carry.
- `amc-runner` diagnostics: structured `tracing` events with stable names
  (`amc.admission.*`) and reason codes, plus `diagnostics()` snapshots
  with real timeout/waiter/transition counters. The library never
  installs a subscriber; see
  `crates/amc-runner/examples/admission-diagnostics.rs`.

## Production capture

Production observation is opt-in and passive. It uses the same identity-aware
collector and never starts, stops, freezes, moves, or changes the target.
Select the actual native service/scope that owns the work, not an activation
client or an assumed universal Steam unit. The output directory must be new,
its parent must exist, and symlink parents are rejected.

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

Replace the illustrative unit with the discovered owner. Choose the actual
output location deliberately; local logs still contain unit/cgroup identifiers,
timestamps, boot identity, and aggregate resource activity. Nothing is uploaded.
No special privileges are granted; inaccessible measurements remain unknown.

| Mode | Default | Allowed duration / interval | Sample storage ceiling |
|---|---|---|---|
| Ordinary `watch` | 40 seconds / 20 ms | 1-60 seconds / 10-1000 ms | 5000 samples, 8 MiB |
| `watch --production` | 1800 seconds / 1000 ms | 1-86400 seconds / 1000-60000 ms | 86,400 samples, 256 MiB |

Duration, sample count, and bytes are independent limits. Reaching a storage
limit ends collection with an explicit incomplete reason; there is no rotation,
automatic deletion, or silent overwrite. The byte ceiling covers `samples.jsonl`;
bounded manifest/summary/marker files add a small amount. It is not a disk-space
reservation. Longer duration does not guarantee that the byte cap will suffice.

Ctrl-C or SIGTERM stops the observer only, persists the readable prefix when
possible, and records `cancelled`/`complete:false`. Sleeps check cancellation at
most every 100 ms and never sleep past the sampling deadline. Manager queries
still have their existing timeout/grace bounds; arbitrary blocked kernel I/O
cannot be given a hard cancellation deadline. SIGKILL can leave no summary.
No policy rollback is needed because observation changes no policy.

Production samples are flushed to the OS on each tick for live readers. `ready`
still requires a synced valid target baseline; sample durability is claimed only
after the final flush/sync succeeds. A flush is not a crash-durability guarantee.
The process exits when this target disappears or changes lifetime; it does not
follow restarts or collect an unbounded series of sessions. Missing `done` or
summary is incomplete evidence. Exit zero alone is not a completeness check.

### Host context

Production sample lines add `host`, with its own wall-clock and observer-relative
monotonic capture-start timestamps and a map of `{value, unknown}` measurements:

- `meminfo.MemTotal`, `MemAvailable`, `SwapTotal`, `SwapFree`, `Buffers`, `Cached`,
  `Dirty`, and `Writeback` (each key has the `meminfo.` prefix): **bytes**, converted
  from procfs `kB` with checked multiplication by 1024.
- `vmstat.pswpin` and `vmstat.pswpout`: cumulative **pages**, not bytes.
  `vmstat.pgmajfault`: cumulative major-fault count. These are not per-interval
  deltas and must not be attributed to the target unit.
- `pressure.memory`, `pressure.cpu`, and `pressure.io`: PSI averages in percent
  and cumulative `total` in microseconds. Older CPU PSI may omit `full`; an absent
  row stays absent, never zero. System-level CPU `full` is not an application
  responsiveness metric.

The module allowlists fields, rejects duplicate keys and invalid units/numbers,
bounds meminfo/vmstat input to 64 KiB each and pressure files to 4 KiB each, and
never emits malformed input or unrelated fields. Missing fields are individually
unknown. Existing strict memory/IO PSI parsing is not relaxed for older CPU PSI.
There are no process listings, argv, environment, device identifiers, or journal
messages in this host context.

`host` means the observer's `/proc` view, which may be restricted or virtualized.
It is not a promise of full machine visibility and is not the target's cgroup
budget. Measurements are sequential, not atomic. `complete` continues to describe
target collection; it does not certify every host metric as present. Inspect
each measurement's unknown reason. `amc diff` still compares the target snapshot,
not these aggregate host values.

### Timing and interpretation

Every sample adds `captureDurationUs` (wall time for target/optional host reads,
not total observer CPU time) and `scheduleLagMs`. Summary `collection` adds
`bytesWritten`, `missedIntervals`, and `maxCaptureDurationUs`. Missed scheduling
slots are skipped rather than collected in bursts. Manager identity checks run
at most once per second and only at sample ticks; a 60-second sample interval
can therefore also delay restart detection. The pinned-reader and final-query
guards still prevent claiming continuity from a matching pathname alone.

Manifest fields `production`, `maxSamples`, `maxOutputBytes`, and `hostScope`
record the selected collection contract. Existing defaults and field meanings
are retained; additions do not select a memory priority profile. Multiple unit
observers are separate captures with separate budgets/host reads. Do not equate
their relative monotonic timestamps: each starts at zero independently. Retain
wall-clock timestamps and boot IDs, and account for clock adjustments and sample
duration when correlating captures.

For gaming, record frame times separately using an existing tool such as
MangoHud and retain its clock/logging configuration. AMC does not measure frame
times, VRAM, background useful-work completion, or causality. Capture game,
driver, kernel, swap/zram, effective ancestry, and binary/source identity with
the experiment record. Low PSI is not proof of good frame times; a frame stall
is not proof that memory caused it. This mode enables observation, not a validated
A/B/C experiment or a claim of low overhead on every workstation.

### Accounted observation

`scripts/capture-accounted.py` is a stdlib-only Python 3.11+ pilot runner. It
starts only an observer in a uniquely named transient **user** service. The
target must already exist; its placement, limits, swap, and OOM policy are not
changed. Use a validated release build before capturing normal work:

```sh
python3 scripts/capture-accounted.py actual-workload.scope \
  --amc target/release/amc --seconds 1800 --output /tmp/amc-accounted-01
python3 scripts/validate-capture.py /tmp/amc-accounted-01/capture --markdown
```

`--system` selects a system-manager **target**, not a privileged observer.
The output parent must exist, the session directory must be new, and its
filesystem must permit executing the binary copy. The runner makes a private
0700 directory, copies/checksums the binary, and writes 0600 JSON records:

- `session.json`: observer name, target manager/unit, requested sampling,
  binary SHA-256, and accounting scope.
- `capture/`: the unchanged manifest/samples/summary/markers produced by `watch`.
- `accounting.json`: final systemd counters, units, unknowns, invocation,
  process start/exit timestamps, and manager result.
- `cleanup.json`: successful stop of the observer after durable accounting export.

The observer uses native CPU/memory/IO accounting and `RemainAfterExit=yes`.
The runner does **not** use immediate `--collect`: it reads the retained final
counters, flushes and syncs the export, then stops its own unit. An invocation
change observed during polling aborts control. Do not independently restart or
replace the observer unit while a capture is running.

CPU time is nanoseconds, peak memory is bytes, and IO fields are block-IO bytes
or operation counts. A missing field, unsupported counter, or systemd's
`UINT64_MAX` sentinel becomes `value:null` with `unknown:unavailable`; zero stays
known. IO byte counts are not JSON file lengths. Start/exit timestamps use
systemd's monotonic microsecond clock, not the capture's relative millisecond
clock. Final counters can remain available after `ControlGroup` becomes empty.

The accounting boundary includes the observer and its child query processes.
It excludes the outer Python launcher, the service manager, and other services
performing work on their behalf. It is not a measurement of total host overhead
caused by observation, nor proof of no frame-time impact. The copied executable
and session metadata remain local; review them before sharing an artifact bundle.

Ctrl-C/SIGTERM asks only the observer's main process to finish via SIGINT, then
exports its retained counters before cleanup. A manager runtime limit bounds
the running observer to the requested duration plus 60 seconds, with a 10-second
stop timeout. Each manager command has a 20-second timeout. Ambiguous submission,
identity, timeout, or export failures do not trigger resubmission or blind
cleanup. Inspect `session.json` and the named unit; a failed export may leave a
partial JSON file and a retained unit that needs manual recovery. Do not assume
an accounting export is present merely because the observer has exited.

The runner's successful exit means its process/accounting lifecycle completed;
check the capture's own completeness separately with the validator. Long-session
overhead and broad distribution/version compatibility remain unvalidated.

## Classification and comparison

- `memory.events*`: counters with scope; deltas require compatible
  identity/lifetime and monotonic counters (decrease → explicit reason).
- `cgroup.events`: state transitions (`populated`/`frozen`), never
  blanket-differenced.
- Usage/limits: sampled gauges (before/after with timing, never budget
  accounting).
- `memory.peak*`: kernel high-water marks; interval peak is not
  `after - before`. Sampled maxima (`maxObservedSwap`,
  `maxObservedCurrent`) are reported separately.
- `memory.pressure`: reported avg10/60/300 retained; interval stall
  fractions (`pressureStall.some/full`) derive from `total`
  microseconds over valid elapsed microseconds, strict: a delta larger
  than the elapsed interval is `incompatible-clocks`, never clamped.

Identity: observation ID names a session, not a workload. A `Compatible`
verdict requires positive continuity evidence — equal invocation IDs on
both sides **and** no contradictory inode (a recycled path under a stale
ID is `replacement-suspected`, never one lifetime). A reboot ends the
lifetime even with agreeing IDs. Path match alone is never sufficient;
one-sided or missing invocation evidence compares as
`missing-identity`; boot/manager-context/UID disagreement is
`context-mismatch` or `restart-detected`. Snapshots carry
`invocationId`/`bootId`/`inode` when the collector verified them; files
without them are never differenced by path. Overlong identities are
rejected, never truncated into a possible collision. `InvocationID`
is collected when the manager reports it; `boot_id` is best-effort.

## Completion and coverage (watch)

`complete` is earned: valid baseline, persisted outputs, and a clean
terminal state (`deadline` or `disappeared-or-empty`). Anything else —
missing baseline (`observer-not-ready`), restart (`invocation-changed`),
same-path replacement (`replacement-suspected`), output-budget or
sample-budget exhaustion, write failure, cancellation — is incomplete
evidence with explicit `coverage` flags (`attachedLate` for passive
attachment after workload start, `baselineDelayed` when the baseline
arrived after the first tick, `finalCountersUnavailable`,
`incompletePersistence`) and a `deltaUnsupportedReason` instead of a
dressed-up number. A last-readable delta never masquerades as lifetime
accounting. Termination result and endpoint identity come from an
independent final leaf query; unavailable final results remain unknown,
so a restart in the final gap rewrites `deadline` rather than producing
cross-lifetime deltas. When the pathname disappears, a verified readable
prefix from the held object is still reported (with
`finalCountersUnavailable` set), but positive evidence of a different
lifetime — restart, replacement, or context mismatch — always wins over
the disappearance. Each derived field uses its own capture time:
stale counters and PSI totals are never divided by a newer tick's
clock, and stall fractions additionally require a compatible lifetime.
Readiness (`ready`) is published only after the baseline sample itself
is flushed and synced. The observer opens metric files relative to a held
directory handle through Linux `/proc/self/fd`, including files that
were initially unavailable, then reads through pinned file descriptions
(a check-to-read race can only serve the original object), checks inode
continuity every tick, polls the manager at most once per second with a
leaf-only query bounded by the remaining sampling budget (never the
full ancestor traversal), and never touches the target on cleanup.
`samples.jsonl` carries the shared Snapshot envelope, so `amc diff`
consumes watch output directly; diff inputs are byte-limited while
reading, not via metadata size.

`eventDeltaCoverage` describes the baseline-to-last-readable interval,
including its elapsed endpoints and whether final counters were available.
A vanished pathname does not erase already captured counters from the
held object: a supported prefix may remain while final coverage is unknown.
`complete` describes collection completion, not full workload accounting;
`eventDeltaCoverage.lifetimeComplete` is always false for passive attachment.
Loaded JSON is checked for supported schema, bounded nonempty identities,
measurement coherence and metric shape before comparison.

The summary path is structural, not string-driven. The collection loop
records a `LoopOutcome` at each exit site; the terminal verdict combines
it with the independent endpoint comparability (`Intact`, `Restarted`,
`Replaced`, `Disappeared`, `EndpointUnknown`) without consulting reason
strings. Prefix deltas derive from the freshest map for intact,
disappeared, or unconfirmed endpoints, and from the verified pre-change
snapshot (last successful invocation poll) once a restart or replacement
is known — post-change ticks still carry old labels until the next poll,
so they never feed a delta. Each derived quantity carries its own start
and end marks (elapsed time and sample sequence); `pressureStallCoverage`
mirrors the counter coverage. A failed final query yields the additive
`endpoint-query-failed` reason: collection finished, confirmation did
not. `collection` tracks attempted samples separately from buffered
(`persistedSamples`) and synced (`storageDurable`) ones; last-readable
values keep their own per-field capture marks and never acquire the
newer tick's timestamp. Final counters are available only with an
intact terminal, a live endpoint, readable last-persisted counters,
and durable persistence — a stale previously-persisted value after a
failed final write stays unavailable. If the `done`
marker itself fails, the summary is repaired to incomplete and the
original error is returned; a missing `done` is incomplete evidence,
never success. New fields are additive (`collection`,
`pressureStallCoverage`, `endpoint-query-failed`); existing
`summary.json` consumers ignore unknown fields. One semantic
tightening on an existing field: `finalCountersAvailable` now
requires durable persistence, so it can flip `true`→`false` in
persistence-failure cases where a stale previously-persisted value
was previously reported available.

## Admission diagnostics

- State is assigned under the admission lock; formatting and subscriber
  work run after it is dropped. Weighted provider I/O runs under a
  separate cache publication protocol, never under the admission mutex.
- Waiters are counted with RAII guards, so async cancellation converges
  the count; timeouts and wait milliseconds are recorded on every exit.
- A deliberately disabled gate reports `Disabled`; only runtime
  fail-open degradation reports `ThreadCapOnly`.
- `WaitReason` distinguishes global `Pressure` from request-specific
  `Budget`/`Exclusivity`/`PageCache`; resume is emitted only with an
  actual admission.
- Page-cache fractions divide by the cache's own domain total (leaf
  limit, else host total) — never by a minimum aggregated across
  independently limited domains. Host `Buffers`/`Cached` lines that are
  missing or malformed are unknown, never zero; sysinfo reports no cache
  accounting at all. Unknown accounting skips the check, keeps the
  observation usable, and sets `cacheUnknown` explicitly — unless the
  deployment sets `requirePageCacheBytes`, which turns it into an
   explicit policy failure honoring fail-open/closed. A missing denominator
   is inferred only for a single-domain observation, never across domains.
- Waiter counts cover parked requests only (probing iterations are not
  waiters), split by reason in `waitersByReason`; `waitReason` is
  reported only while requests are parked and is never cleared by
  another request's admission. Durations converge via RAII guards,
  including async cancellation. Permits are constructed before any
  subscriber code runs, so a panicking subscriber unwinds through a
  live permit instead of leaking the reservation. Weighted provider I/O
   uses an epoch-ordered probe cache: releases invalidate
   by epoch, concurrent probes publish only when unsuperseded, admission
   rechecks that generation under the state/cache locks, and
  disabled/degraded gates never probe at all.
- Thresholds, fail-open policy, hysteresis, and conservative reservation
  accounting are unchanged by this refactor.

## Privacy

Unknown is never zero; malformed bytes are never reprinted. No journal
bodies, `ExecStart`, argv, environment values, command contents, or raw
provider/subprocess text in diagnostics. `ProviderError::Source` maps to
`provider-source-unavailable` / `provider-unsupported`, including inside
formatted error chains. Regression-tested with a sensitive provider
under an enabled subscriber that captures every event field.

## Limits and overhead

Historical single-run debug-build microbenchmarks on this host (synthetic
cgroup dir, fail-open provider; not remeasured for the generation and
directory-handle fixes):

- Snapshot: ~56µs/sample, ~1KiB/sample JSON. At 20ms intervals this is
  ~0.3% of one CPU and ~50KiB/s; a 40s run is ~2MiB (8MiB cap,
  5000-sample cap, 60s/10ms bounds).
- Weighted admission: ~0.8µs per acquire+diagnostics+release (fail-open
  provider, debug build).

These are order-of-magnitude smoke numbers, not production profiles:
no peak-RSS measurement, no diagnostics-enabled-vs-disabled split, no
release-build or latency distribution. Treat them as such.
They also predate production host-context capture and do not estimate its cost.
- Sampling is sequential, not atomic. No implementation can guarantee
  readable counters after cgroup destruction, recover unwritten evidence
  after SIGKILL, or impose a hard deadline on arbitrary blocking
  kernel/filesystem operations: those stay explicit unknown/incomplete
  outcomes, not success claims. The observer never bridges restarts with
  one baseline.

## Compatibility

- `Snapshot`/`Inspection` JSON is additive (`schemaVersion`,
  `observedMonotonicMs`, `sequence`, `bootId`, `invocationId`, `inode`,
  `manifest` fields, `deltaUnsupportedReason`, `pressureStall*`). Legacy
  consumers ignore unknown fields; legacy files without identity fields
  compare as `missing-identity`.
- The Python `scripts/watch-cgroup.py` parser is deprecated and matches
  the Rust core byte-for-byte (digits-only u64, strict keys, finite
  PSI ranges) until deletion. VM OOM evidence now comes from
  `amc watch`; the driver asserts `eventDeltas.oom > 0`,
  `maxObservedSwap == 0`, and `Result == oom-kill`. The `oom` event
  delta is asserted rather than `oom_kill` because the observed kernel
  reports the group OOM as an event without charging either kill
  counter to the unit cgroup; asserting `oom_kill` would encode an
  unverified kernel-accounting assumption.
- A per-tick lookup that finds no directory is disappearance
  (`disappeared-or-empty`), never suspected replacement: only a
  different present inode describes a new object. Collected cgroups
  therefore keep the disappearance verdict and their readable prefix.
