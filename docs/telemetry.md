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
  `amc watch`; the driver asserts `eventDeltas.oom_kill > 0`,
  `maxObservedSwap == 0`, and `Result == oom-kill` as before.
