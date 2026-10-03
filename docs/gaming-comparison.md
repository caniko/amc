# CS2 recording and matched background-work comparison

The workstation objective is a **stable, usable foreground gaming session while
bounded background tasks make useful progress under shared resource pressure**.
The [operational guide](workstation-policy.md) covers delivering that behavior.
This optional comparison guide retains CS2 recording and scenario-specific
frame-time diagnostics; an A/B/C result is not a delivery requirement. The native
smoke verifies the harness mechanism; its small hash jobs are not a representative
performance workload.
See the [dated verification record](workstation-harness-20260930.md) for exact
source commits, native results, retained failed attempts and remaining gates.

## Record the replay

1. In CS2, enable **Settings → Game → Enable Developer Console**. Open the
   console with the configured console key (usually `~`). Use a local Practice
   session on one fixed map, offline rather than a public match.
2. Set the intended resolution, graphics preset, frame cap and camera route.
   Write them down. Let shaders settle and walk the route once before recording.
3. In the console, start a new recording with a simple, unique name:

   ```text
   record amc-calibration-v1
   ```

   Walk a fixed 60–90-second route, including the same effects and view changes.
   Then end the recording:

   ```text
   stop
   ```

4. Find the resulting `.dem`. CS2 normally writes local recordings beneath
   `game/csgo/` in its installation. On the current Atlas installation that is
   `/data/nvme0/can/games/steamapps/steamapps/common/Counter-Strike Global Offensive/game/csgo/`.
   The filesystem result and console confirmation are authoritative. If the
   installed build rejects a command, use console `find record`, `find stop`,
   and `find playdemo` and retain the error; do not proceed with an empty file.
5. Copy the demo into a new local experiment directory and record its SHA-256,
   Steam app ID `730`, build ID, map, route, tick range, settings and date. Keep
   the original too. A demo can contain player information; use the local scene.
6. Verify playback from the same installed build:

   ```text
   playdemo amc-calibration-v1
   ```

   Confirm the map loads and the route lasts the expected duration. Keep normal
   playback speed; a `timedemo`/fast-forward benchmark changes the workload.
   Replay it at least twice during calibration. Record loading boundaries,
   camera choice, playback speed and a repeatable measured tick/time window.
   A saved demo alone does not establish repeatability across game updates.

These are operator-run CS2 console steps. They have not yet been verified on the
installed build (`25588766` at initial discovery). Console commands and demo
compatibility must be rechecked when that build changes.

## Capture per-present frame times with MangoHud

Use one new output directory per attempt. Install a MangoHud version usable in
Steam's runtime and verify its version. MangoHud 0.8.4 was found in Atlas's Nix
store; availability inside CS2 is still a runtime acceptance gate.

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

Start playback, then toggle logging with left Shift + F2 at the declared marker;
toggle it again after the intended window. The finite `log_duration` is a backup
bound. Close the game normally so logs finish. Avoid `autostart_log` for scene
alignment: it delays from application startup, not from replay readiness.

In upstream **0.8.4**, `log_interval=0` calls the logger from the present path;
nonzero values use a periodic logging thread. `frametime` is milliseconds and
`elapsed` is nanoseconds since logging started. Logging writes and flushes each
row. This is per-present instrumentation, not input-to-display latency, and can
affect the measurement. The summary CSV and on-screen FPS are not raw frames.

Validate the raw CSV with the supplied offline tool (example window and stall
threshold only; choose the real values during calibration):

```sh
python3 scripts/frame-report.py /absolute/attempt/frames/CS2_raw.csv \
  --log-interval-ms 0 --start-seconds 5 --end-seconds 65 --stall-ms 50
```

The tool rejects periodic-log declarations, malformed/nonmonotonic samples,
missing window endpoints and summary files. It reports nearest-rank p95/p99,
stall count/rate and frame count. It does **not** certify that CS2 loaded this
configuration or that every game frame was logged. Confirm that independently:
check row count and elapsed increments against present rate, inspect pauses,
verify only the intended swapchain is represented, and retain the raw CSV.

The CSV clock starts at logging, not process launch or the filename timestamp.
Bracket the actual logging-start marker with wall-clock and monotonic timestamps
on the same boot, recording the uncertainty. Align the frame window and workload
receipt times using that marker. Do not align solely by CSV creation time. Keep
any alignment uncertainty in the result; choose an acceptable bound during
calibration before freezing the comparison.

During separate instrumentation calibration, compare otherwise identical
replays with MangoHud absent, injected with logging off, and injected with the
chosen logging configuration. Use an independent measurement for the absent
condition, or report that its overhead remains unknown. Do the same for AMC's
resource observer. Keep logging and observer settings identical for A/B/C.

## Discover the real game owner

Keep CS2 on its normal Steam launch path. While it is running, locate its PID
and read `/proc/PID/cgroup`. Match that path to manager metadata, then archive:

```sh
systemctl --user list-units --type=scope --state=running
systemctl --user show UNIT --property=Id,ControlGroup,InvocationID,Slice
amc inspect UNIT --json
```

If it is a system-manager unit, use `amc inspect UNIT --system --json`. Check
that the actual CS2 process is below the reported path. An aggregate Steam scope
may contain several applications; disclose that scope and do not label its
memory totals as CS2-only. Rediscover transient names for every attempt.

Use a finite [accounted observer](telemetry.md#accounted-observation) or
`amc watch UNIT --production --seconds SECONDS --output NEW_DIRECTORY`. Validate
each capture with `amc report`; preserve partial and unavailable measurements.
`amc diff` checks workload continuity and is not the independent-arm analyzer.

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

All manifest jobs are offered at the common batch-start timestamp. Workers claim
them in manifest order, so native launch order among concurrent workers is not
strict FIFO. C can change actual starts through admission. Offer the batch at
the same declared replay marker in B/C. The driver is the B/C background-work
component, not a game launcher or a complete automatic experiment controller.
For A, record and use the user's ordinary background command/concurrency and
placement; do not relabel a capped B run as existing distro behavior.

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
its live limits, applies identical per-job memory/CPU/runtime/lifecycle settings
to B/C, and verifies each unit's identity, placement and kernel memory limits
before work. It does not reconfigure the aggregate or game. Each job's verifier
must inspect outputs from **this attempt**, not a stale artifact. Use unique
artifact/target directories and pinned inputs. Commands are literal argv with an
absolute executable; wrappers can set job-specific toolchain/cache variables.
The submitting environment is forwarded, excluding manager/lifecycle variables;
freeze relevant variables and keep evidence/logs private.

Jobs are bounded to 32, concurrency to 8, and admission/runtime to 1,800 seconds
each. Systemd owns job cgroups; SIGINT/SIGTERM requests owned-unit cleanup.
If a coordinator is SIGKILLed, native runtime bounds remain the backstop: inspect
the recorded unit identities before running another attempt. No job is replayed
automatically and existing output directories are refused.

Artifacts include the copied manifest, boot/run identity, native startup identity,
workload/verifier logs, receipts, manager accounting and per-job/aggregate results.
Useful work requires both a verified output receipt and a successful confirmed
native outcome. Unknown/failure/cancellation remains recorded and counts as zero
successful useful units. Receipt kernel counters are sampled **before helper
exit**, with unavailable cells null; they are not complete unit-lifetime totals.
Manager final counters may be unavailable after collection. `housekeeping`
can be unknown for an already collected successful unit while its termination
was independently confirmed by the runner's pinned startup domain.

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

If conducting a comparative experiment, archive separate calibration attempts
that establish:

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

Freeze those values in a protocol with an input/hash inventory. Archive a seeded,
balanced A/B/C schedule randomized within sessions, and execute it as written.
Keep every attempt, including setup failures and interrupted runs; predeclared
exclusions annotate rather than erase attempts. Compare run-level metrics with
uncertainty, never treating frames as independent replicates. Report game tails,
useful work inside the game window and eventual batch drain, failures, waits,
overhead and coverage together. Native-only, negative, null and inconclusive
results are all valid outcomes.

## Sources

The logger format and per-present path were checked against MangoHud **v0.8.4**:
[configuration](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/data/MangoHud.conf),
[logging.cpp](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/src/logging.cpp)
and [overlay.cpp](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/src/overlay.cpp).
Valve's [demo documentation](https://developer.valvesoftware.com/wiki/Demo_Recording_Tools)
was bot-protected during this session; the CS2 steps above require local
confirmation rather than claiming an upstream-verified or executed replay.
