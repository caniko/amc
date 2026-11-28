# CS2 recording and matched background-work comparison

The workstation objective is a **stable, usable foreground gaming session while
bounded background tasks make useful progress under shared resource pressure**.
The [operational guide](workstation-policy.md) covers delivery; it doesn't
require an A/B/C result. This guide covers optional CS2 recording and
scenario-specific frame-time diagnostics. The native smoke verifies the harness
mechanism, but its small hash jobs aren't a representative performance workload. See the
[dated verification record](workstation-harness-20260930.md) for exact source
commits, native results, retained failures and open gates.

## Record the replay

1. In CS2, enable **Settings → Game → Enable Developer Console**. Open the
   console with its configured key (usually `~`). Use offline local Practice on
   one fixed map, not a public match.
2. Set and record the resolution, graphics preset, frame cap and camera route.
   Let shaders settle and walk the route once before recording.
3. Start a recording in the console with a simple, unique name:

   ```text
   record amc-calibration-v1
   ```

   Walk a fixed 60–90-second route with the same effects and view changes. Stop:

   ```text
   stop
   ```

4. Find the `.dem`. CS2 normally writes local recordings under `game/csgo/` in
   the CS2 installation. On the current Atlas installation:
   `/data/nvme0/can/games/steamapps/steamapps/common/Counter-Strike Global Offensive/game/csgo/`.
   Trust the filesystem result and console confirmation. If the build rejects a
   command, use console `find record`, `find stop`, and `find playdemo`. Keep the
   error and don't proceed with an empty file.
5. Copy the demo into a new local experiment directory and record its SHA-256,
   Steam app ID `730`, build ID, map, route, tick range, settings and date.
   Keep the original. Use the local scene: demos can contain player information.
6. Verify playback from the same installed build:

   ```text
   playdemo amc-calibration-v1
   ```

   Confirm the map loads and the route lasts as expected. Keep normal playback
   speed: `timedemo`/fast-forward changes the workload. Replay at least twice
   during calibration. Record load boundaries, camera choice, playback speed and
   a repeatable measured tick/time window. A saved demo doesn't prove
   repeatability across game updates.

The operator runs these CS2 console steps. They remain unverified on the
installed build (`25588766` at discovery). Recheck commands and demo
compatibility on build changes.

## Capture per-present frame times with MangoHud

Use a new output directory per attempt. Install MangoHud for Steam's runtime
and verify its version. MangoHud 0.8.4 was found in Atlas's Nix store; verify
availability inside CS2 before accepting the setup.

Create an absolute `MangoHud.conf` for that attempt:

```ini
no_display
log_interval=0
log_duration=120
output_folder=/absolute/new/attempt/frames
permit_upload=0
toggle_logging=Shift_L+F2
```

Replace the output path with an existing, private directory. Set these **Steam
launch options** for CS2, using the absolute configuration path:

```text
MANGOHUD_CONFIGFILE=/absolute/new/attempt/MangoHud.conf mangohud %command%
```

Start playback, then press left Shift + F2 at the declared marker and again
after the intended window. The finite `log_duration` bounds logging as a
fallback. Close the game normally to finish logs. Avoid `autostart_log`
for scene alignment: it starts its delay at application startup, not replay
readiness.

In upstream **0.8.4**, `log_interval=0` calls the logger from the present path;
nonzero values use a periodic logging thread. `frametime` is milliseconds;
`elapsed` is nanoseconds since logging started. Per-present instrumentation,
including row writes and flushes, can affect the measurement; it does not
measure input-to-display latency. The summary CSV and on-screen FPS aren't raw
frames.

Validate the raw CSV offline (example window and stall threshold; choose yours
during calibration):

```sh
python3 scripts/frame-report.py /absolute/attempt/frames/CS2_raw.csv \
  --log-interval-ms 0 --start-seconds 5 --end-seconds 65 --stall-ms 50
```

The tool rejects periodic-log declarations, malformed/nonmonotonic samples,
missing window endpoints and summary files. It reports nearest-rank p95/p99,
stall count/rate and frame count. It does **not** certify that CS2 loaded the
configuration or that MangoHud logged every game frame. Independently check
configuration uptake and frame coverage: compare row count and elapsed
increments to present rate, inspect pauses, check for only the intended
swapchain, and keep the raw CSV.

The CSV clock starts at logging, not process launch or the filename timestamp.
Bracket the actual start marker with wall-clock and monotonic timestamps from
the same boot. Use that marker, not CSV creation time alone, to align the frame
window and workload receipts. Record uncertainty and carry it into results.
Choose an acceptable bound during calibration, before freezing the comparison.

Calibrate instrumentation separately: compare otherwise identical replays with
MangoHud absent, injected without logging, and injected with the chosen logging
configuration. Measure the absent condition independently or report overhead as
unknown. Repeat for AMC's resource observer. Keep logging and observer settings
identical for A/B/C.

## Discover the real game owner

Launch CS2 normally through Steam. While it runs, find its PID and read
`/proc/PID/cgroup`. Match the path to manager metadata, then archive:

```sh
systemctl --user list-units --type=scope --state=running
systemctl --user show UNIT --property=Id,ControlGroup,InvocationID,Slice
amc inspect UNIT --json
```

If it is a system-manager unit, use `amc inspect UNIT --system --json`. Check
that the CS2 process is below the reported path. Disclose any aggregate Steam
scope: it may contain other apps, so don't label its memory totals CS2-only.
Rediscover transient names each attempt.

Use a finite [accounted observer](telemetry.md#accounted-observation) or
`amc watch UNIT --production --seconds SECONDS --output NEW_DIRECTORY`. Validate
each capture with `amc report`; preserve partial and unavailable measurements.
`amc diff` checks workload continuity; it is not an analyzer for independent
arms.

## B/C finite-batch driver

Build the experiment example from the approved project environment:

```sh
direnv exec . env CARGO_ENCODED_RUSTFLAGS= \
  cargo build --example workstation-batch --release --locked -j 2
target/release/examples/workstation-batch check /absolute/batch.json
target/release/examples/workstation-batch run --arm b \
  --manifest /absolute/batch.json --output /absolute/new/attempt/batch
```

For C, change only `--arm c` and the new output directory. B has fixed worker
concurrency. C uses the same concurrency ceiling and **one shared weighted
`Runner`**, observing the worker slice, its visible ancestors and host memory.
The manifest adds a finite byte-budget domain and reserve; unknown provider
data fails closed. Cache-ratio gating and oversized-job exclusivity are disabled
for this comparison so their defaults are not additional hidden treatments.

All jobs share the batch-start offer timestamp. Workers claim jobs in manifest
order, but concurrent native launches aren't strict FIFO. C admission can shift
actual starts. Offer the batch at the same declared replay marker in B/C. The
driver handles B/C background work, not game launches or full experiment
automation. For A, record and use the user's ordinary background
command/concurrency and placement. Don't relabel a capped B run as existing
distro behavior.

Manifest shape (illustrative values and commands, **not calibrated policy**):

```json
{
  "version": 1,
  "slice": "agent-tools.slice",
  "aggregate_memory_max": 25769803776,
  "aggregate_memory_swap_max": 8589934592,
  "concurrency": 2,
  "budget_bytes": 4294967296,
  "reserve_bytes": 536870912,
  "max_ram_fraction": 0.9,
  "runtime_seconds": 180,
  "admission_seconds": 180,
  "cpu_percent": 200,
  "jobs": [{
    "id": "cargo-clean-0",
    "weight_bytes": 1610612736,
    "memory_max": 2147483648,
    "memory_swap_max": 0,
    "cwd": "/absolute/frozen/amc-source",
    "argv": ["/absolute/workload-script"],
    "verify_argv": ["/absolute/verify-workload-script"],
    "useful_units": 1
  }]
}
```

The consumer must already own/configure the aggregate slice. The driver verifies
the aggregate's live limits, applies identical per-job memory/CPU/runtime/lifecycle
settings to B/C, and verifies every unit's identity, placement and kernel memory
limits before work. It reconfigures neither aggregate nor game. Each job's
verifier must inspect **this attempt's** outputs, not stale artifacts. Use
unique artifact/target directories and pinned inputs.
Commands use literal argv and an absolute executable. Wrappers can set job-specific
toolchain/cache variables. The driver forwards the submitting environment except
manager/lifecycle variables. Freeze relevant variables and keep evidence/logs private.

Limits: 32 jobs, 8 concurrent jobs, and 1,800 seconds each for admission/runtime.
Systemd owns job cgroups; SIGINT/SIGTERM requests owned-unit cleanup. If a
coordinator is SIGKILLed, native runtime bounds still apply. Inspect recorded
unit identities before starting any new attempt. The driver never replays jobs
automatically and rejects existing output directories.

Artifacts include the copied manifest, boot/run identity, native startup identity,
workload/verifier logs, receipts, manager accounting and per-job/aggregate results.
Useful work needs both a verified output receipt and confirmed native success.
Unknown/failure/cancellation stays recorded and counts as zero useful units.
Receipts sample kernel counters **before helper exit** and mark unavailable
cells null; these aren't unit-lifetime totals. Manager final counters may be
unavailable after collection. `housekeeping` can be unknown for a collected
successful unit even when the runner independently confirmed termination through
its pinned startup domain.

Native mechanism smoke, using small finite jobs:

```sh
direnv exec . python3 tests/workstation-batch-systemd.py \
  --driver target/release/examples/workstation-batch \
  --slice agent-tools.slice --output /absolute/new/native-smoke
```

It checks shared byte-budget serialization, literal arguments, useful-output
verification, failure retention, aggregate-limit mismatch and termination.
Its small hash jobs do not establish memory competition or gaming benefit.

## Optional comparison: calibrate, freeze, then compare

For a comparison, archive separate calibration attempts that establish:

- Replay repeatability, a warm-up/reset/cache procedure, the measured window and
  timing alignment, logger and observer overhead, and source/config hashes.
- A realistic finite mix of useful jobs with isolated outputs, observed peak
  memory/CPU/IO, conservative weights, and actual memory competition on Atlas.
  A single approximately 1.06 GiB Cargo test batch is insufficient by itself.
- Native aggregate/per-job limits, reasonable B concurrency, C budget/reserve,
  useful-work floor, stall threshold, practical p99 improvement margin,
  throughput noninferiority margin and repetition count.
- Failure/exclusion rules, game/process identity checks, watchdog/reset procedure,
  OOM attribution, thermals, swap/zram and existing pressure-management settings.

Freeze those calibrated values in the protocol, together with the input/hash
inventory. Archive and follow a seeded, balanced A/B/C schedule randomized
within sessions. Keep all attempts, including setup failures and interruptions;
annotate predeclared exclusions, don't erase attempts. Compare run-level metrics
with uncertainty; frames aren't independent replicates.
Report game tails, useful work inside the measured game window and eventual
batch drain, failures, waits, overhead and coverage together. Native-only,
negative, null and inconclusive results are valid.

## Sources

The logger format and per-present path were checked against MangoHud **v0.8.4**:
[configuration](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/data/MangoHud.conf),
[logging.cpp](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/src/logging.cpp)
and [overlay.cpp](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/src/overlay.cpp).
Valve's [demo documentation](https://developer.valvesoftware.com/wiki/Demo_Recording_Tools)
was bot-protected during this session. The CS2 steps need local confirmation;
they don't claim upstream verification or an executed replay.
