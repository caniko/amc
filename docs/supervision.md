# Native memory supervision

One host-wide AMC supervisor can observe enrolled native services and admitted
jobs. Consumers choose application policy and lifecycle authority. Systemd owns
execution, cgroup placement, hard limits and descendant cleanup; admission
services keep their full-ceiling reservations.

## Interfaces and ownership

```sh
amc supervise serve --policy /etc/amc/supervision.json
amc supervise status
amc supervise replay --file /var/lib/amc-supervision/trace.jsonl
```

`serve` requires root, explicit policy, a trusted runtime directory and an
exclusive durable-state lock. The NixOS `nixosModules.supervision` module installs
the service and creates its directories. `status` reads public, bounded JSON
without contacting a manager. `replay` reads up to 128 MiB of chronological JSONL
with a 1 MiB frame limit; it never signals or starts services.

[`examples/supervision.json`](../examples/supervision.json) is a disabled-actuation
schema example; its values are not workstation defaults. Before running it, set
the unit and ceilings to those of the enrolled native owner.

Policy version 1 has three lifecycle authorities:

| Authority | Intervention |
|---|---|
| `restart` | Stop the original dedicated backend, verify termination, cool down, then start once |
| `terminate` | Stop one disposable invocation; never replay or start its command |
| `observe` | Observe and inhibit new admission; no signal or start authority |

Static domains specify an exact system/user service, native memory/swap ceilings
and recovery priority. An `observe` domain can set `memory_max = 0` to read live
leaf and ancestor capacities without setting a leaf cap. Job pools select
explicit contract names from per-user admission ledgers and attach using each
grant's invocation, cgroup path and inode. Ledger reads never change or release
reservations. Expanding broker authority requires a broker-owned cancellation
interface.

`shadow` observes and traces without intervening or inhibiting admission.
`enforce` adds inhibition on pressure or unknown evidence and permits bounded
recovery. Connect admission's optional `--health-file` only for enforcement.
`forecast_recovery` gates disruptive forecast-based recovery independently of
directly observed emergencies.

## Forecast interpretation

Each comparable invocation gets a streaming model fed memory and swap usage
against finite leaf and ancestor hard boundaries, plus host RAM usage against
`MemTotal - reserve_bytes`. Leaf/ancestor protected boundaries are 90% of their
ceilings. These fractions are initial settings, not benchmark-derived
application optima.

The model combines plateau, damped-growth and sustained-growth local-linear
state-space experts. Student-t-inspired robust innovations reduce outlier
influence; fixed-share likelihood weights let regimes switch. This is an
approximate filter, not a full Bayesian Student-t posterior. Forecast steps are
nominal one-second samples; oversized gaps censor, not bridge, segments.

For a forecast at `t`, the score is the largest absolute error over **all
boundaries and all future steps in its window**. It enters rolling calibration
only after the full window arrives.
A trailing empirical conformal quantile with integral coverage-error feedback
sets the common radius. An unestimable tail or negative radius bars forecast
recovery. `possible` marks an upper forecast peak crossing a protected boundary;
`strong` requires a calibrated lower peak crossing it.

Intervals describe normalized usage trajectories, not OOM probabilities, leak
diagnoses or guaranteed conditional/local coverage. Future windows overlap,
workload streams are dependent and nonstationary, and intervention censors the
unobserved counterfactual future. Exchangeability-based conformal guarantees
don't automatically apply. Warm forecasts can inhibit admission if their peak
threatens a boundary, but cannot authorize forecast recovery until calibration
is ready.

The e-CUSUM mixture is diagnostic only: no p-value, ever-alarm probability or
actuation gate. Its null is a nonpositive conditional mean of the clipped
normalized innovation. Normal workload growth or selecting the largest
innovation across boundaries can violate it.

Identity/capacity changes, missing observations, sampling gaps and intervention
end comparability. Replay preserves completed, evaluated, uncovered and censored
window counts across boot/policy segments, rejects reordered/interleaved boots
and duplicate domains, and censors missing-domain frames. Report empirical miss
fractions with workload, segment and missingness context; alone they don't
establish deployment reliability.

The statistical design draws on [online conformal PID control](https://arxiv.org/abs/2307.16895),
[time-series prediction research](https://nejsds.nestat.org/journal/NEJSDS/article/59/info),
and sequential/adaptive research at [2509.02844](https://arxiv.org/html/2509.02844v4)
and [2410.13115](https://arxiv.org/html/2410.13115v2). Their published guarantees
do not establish guarantees for this combined heuristic or recovery controller.

## Bounded recovery

A domain eligible for recovery requires `Restart=no`, `KillMode=control-group`,
`OOMPolicy=kill`, matching finite leaf ceilings and a native stop timeout within
the kill deadline. Attachment pins the main process by pidfd and cgroup readers
by held descriptors, then rechecks manager identity.

Before TERM, the supervisor publishes inhibition and durably records the attempt.
It may escalate to KILL at the TERM deadline. Signals target the pinned main
process; systemd cleans up descendants. No fixed-name stop/restart, raw cgroup
mutation or process-tree scanning. Cooldown requires original process death,
original cgroup absence/empty population and fresh native inactivity evidence.
A replacement during stopping trips recovery.

Backend cooldown requires continuously healthy, known headroom. An absolute
`cooldown_ms + 60 seconds` deadline bounds unhealthy waits. Starting has a
10-second verification deadline. After querying the manager, restart dispatch
rechecks live host headroom, native lifecycle and in-memory authority for the
persisted phase. Queued, expired or revoked actions cannot inherit a replacement
phase. Failed queries, unavailable workers, unknown termination and exhausted
budgets trip rather than expand authority.

Dispatch also rechecks the bounded native stop timeout and independently proves
the original process and descendants are gone. A backend first found failed
enters cooldown with its retained invocation and verified empty service slot,
without signal authority. Losing cleanup evidence during cooldown trips rather
than authorizing a start.

Attempts survive supervisor restart/reboot and policy edits. The accounting
window only grows: shorter policy never forgives recovery history. A backwards
wall clock forbids another attempt; cross-boot accounting requires a trustworthy
wall clock. Startup marks persisted in-flight recovery `tripped`; AMC never
replays it automatically.

Disposable jobs share their enrolled pool's recovery budget across UUID ticket
changes. Persisted per-ticket history migrates to that budget without forgiving
spent attempts.

To reconcile a trip, stop the supervisor and resolve the native domain first:

```sh
amc supervise reconcile --state /var/lib/amc-supervision
```

Reconciliation takes the same exclusive state lock and proves original native
termination before clearing active recovery. It preserves attempts and starts
nothing. Resolve any running replacement, replaced cgroup or unreadable state
before reconciliation can succeed.

## Evidence and failure behavior

The supervisor atomically writes public read-only `/run/amc-supervision/health.json`
and `status.json` with mode 0644, even under service `UMask=0077`.
Admission refuses new entries/grants on missing, malformed, future, wrong-boot,
older-than-three-second, inhibited or degraded health. Existing grants remain
charged. Graceful enforce-mode shutdown publishes inhibition immediately;
freshness checks catch abrupt loss.

`/var/lib/amc-supervision/recovery.json` and rotating traces are private. Traces
rotate at 64 MiB per file; backpressure/failure increments a visible drop counter.
The sample loop runs no manager subprocesses. Discovery and actuation use
separate bounded workers; stale discovery inhibits admission. Synchronous
durable writes can stall supervision if the filesystem stalls. Heartbeat
freshness and native hard limits are the fallback, not a recovery guarantee.

Local Rust tests cover forecasting, delayed scoring, identity/gap censoring,
pidfd identity rejection, admission inhibition, recovery deadlines/budgets and
replay segmentation. `nixosTests.x86_64-linux.supervision` defines closed-loop
backend/descendant cleanup, once-only restart, in-flight supervisor restart,
shadow authority and trace/replay cases. Hosted CI must retain its named JUnit
cases and trace/recovery receipts. The VM stops the trace writer before replay
and export. `replay-input.json` binds the full trace's SHA-256 and byte count to
the replay summary's SHA-256; hosted verification rejects missing or stale
bindings. Test definitions aren't passing VM evidence. Before enabling forecast
recovery, application thresholds still need chronological replay, shadow
observation and controlled consumer qualification.

### Native failure injections

The supervision VM also defines these production-path probes:

- Missing, malformed, oversized, writable, foreign-owned and symlinked heartbeat
  files; expired/future timestamps; changed boot, inhibition and degraded state.
  Each denies a new pool across multiple broker ticks but preserves a live root
  owner's exact grant, identity and full-ceiling commitment.
- Missing/malformed `MemAvailable` and full memory PSI. Corrupted proc files are
  mounted only in the broker's private namespace. Broker restart preserves both
  live grants; restoring the required observations permits the queued third pool.
- Replacement during native failed-backend cooldown. The old recovery trips,
  keeps its attempt and never signals or restarts the active replacement or its
  descendant.
- Two completed recoveries followed by independent domain-budget and host-budget
  exhaustion. Both trips survive supervisor restart with unchanged accounting
  and no third recovery. The host case isolates the host constraint using two
  domains below their individual limits.

An unenrolled backend and descendant stay live under the same invocation
through replacement and budget trials. Failures stay inside the VM, leaving
build-host telemetry and service policy unchanged.

`nix/hosted-ci.py` requires all named execution cases and verifies the exported
`admission-failures.json`, `admission-observations.json`,
`replacement-status.json`, `domain-budget-status.json`,
`host-budget-status.json` and `foreign-status.json`. Validator regressions reject
missing denial variants, early-only observations, lost commitments, missing
restored progress, changed identities, forgiven attempts and foreign disruption.
Qualification still requires the production, workspace-test, strict-Clippy,
shared-admission and supervision outputs at the same immutable source.
