# Native memory supervision

AMC can observe enrolled native services and admitted jobs with one host-wide
supervisor. Consumers select application policy and lifecycle authority.
Systemd retains execution, cgroup placement, hard limits and descendant cleanup;
the admission services retain their full-ceiling reservations.

## Interfaces and ownership

```sh
amc supervise serve --policy /etc/amc/supervision.json
amc supervise status
amc supervise replay --file /var/lib/amc-supervision/trace.jsonl
```

`serve` requires root, an explicit policy, a trusted runtime directory and an
exclusive durable-state lock. The NixOS `nixosModules.supervision` module installs
the service and creates its directories. `status` reads public, bounded JSON;
it does not contact a manager. `replay` reads up to 128 MiB of chronological JSONL
with a 1 MiB frame bound and never signals or starts a service.

[`examples/supervision.json`](../examples/supervision.json) is a disabled-actuation
schema example. Replace its unit and ceilings with the actual enrolled native
owner before running it; its values are not workstation defaults.

Policy version 1 has three lifecycle authorities:

| Authority | Intervention |
|---|---|
| `restart` | Stop the original dedicated backend, verify termination, cool down, then start once |
| `terminate` | Stop one disposable invocation; never replay or start its command |
| `observe` | Observe and inhibit new admission; no signal or start authority |

Each static domain names an exact system/user service, native memory/swap
ceilings and recovery priority. An `observe` domain can use `memory_max = 0`
to read its actual leaf and ancestor capacities without prescribing a leaf cap.
Job pools select explicit contract names from per-user admission ledgers and
carry each grant's invocation, cgroup path and inode into attachment. Reading a
ledger never changes or releases its reservations. Brokers need a broker-owned
cancellation interface before their authority can expand.

`shadow` observes and traces without intervention or admission inhibition.
`enforce` additionally inhibits on pressure or unknown evidence and permits
bounded recovery. Consumers should connect admission's optional `--health-file`
only for enforcement. A separate `forecast_recovery` gate controls disruptive
forecast-based recovery; it is independent of directly observed emergencies.

## Forecast interpretation

Every comparable invocation gets its own streaming model. The input contains
memory and swap usage against finite leaf and ancestor hard boundaries, plus
host RAM usage against `MemTotal - reserve_bytes`. Leaf/ancestor protected
boundaries are 90% of their ceilings. These fractions are initial mechanism
settings, not benchmark-derived application optima.

The model combines plateau, damped-growth and sustained-growth local-linear
state-space experts. Student-t-inspired robust innovations reduce outlier
influence; fixed-share likelihood weights allow regime switching. It is an
approximate filter, not a full Bayesian Student-t posterior. Forecast steps are
nominal one-second samples; oversized gaps censor the segment rather than
bridging it.

For a forecast issued at time `t`, the calibration score is the largest absolute
error over **all boundaries and all future steps in its window**. That score
enters the rolling calibration set only after the whole window arrives. A
trailing empirical conformal quantile, with integral coverage-error feedback,
sets the common radius. An unestimable tail or negative radius cannot authorize
forecast recovery. `possible` marks an upper forecast peak crossing a protected
boundary. `strong` requires a calibrated lower peak crossing it.

These intervals describe normalized usage trajectories. They are not OOM
probabilities, leak diagnoses or guaranteed conditional/local coverage. Future
windows overlap, workload streams are dependent and nonstationary, and
intervention censors the unobserved counterfactual future. Exchangeability-based
conformal guarantees do not automatically apply. Warm forecasts can inhibit
when their predicted peak threatens a boundary, but cannot authorize forecast
recovery until calibration is ready.

The e-CUSUM mixture is diagnostic only. Its mathematical null is that the
conditional mean of the clipped normalized innovation is nonpositive. Normal
workload growth and selecting the largest innovation across boundaries can
violate that null. Its value is neither a p-value nor an ever-alarm probability,
and it is not an actuation gate.

Identity/capacity changes, missing observations, sampling gaps and intervention
end comparability. Replay preserves completed, evaluated, uncovered and censored
window counts across boot/policy segments, rejects reordered/interleaved boots
and duplicate domains, and censors missing-domain frames. Report the empirical
miss fraction with its workload, segment and missingness context; it does not
establish deployment reliability by itself.

The statistical design draws on [online conformal PID control](https://arxiv.org/abs/2307.16895),
[time-series prediction research](https://nejsds.nestat.org/journal/NEJSDS/article/59/info),
and sequential/adaptive research at [2509.02844](https://arxiv.org/html/2509.02844v4)
and [2410.13115](https://arxiv.org/html/2410.13115v2). Their published guarantees
are not claims about this combined heuristic or its recovery controller.

## Bounded recovery

An enforceable domain requires `Restart=no`, `KillMode=control-group`,
`OOMPolicy=kill`, matching finite leaf ceilings and a native stop timeout within
the kill deadline. Attachment pins the main process with a pidfd and the
cgroup readers with held descriptors, then rechecks manager identity.

The supervisor publishes inhibition and durably records an attempt before TERM.
At the TERM deadline it may escalate to KILL. Signals target the pinned main
process; systemd cleans up descendants. No stop/restart by a fixed unit name,
raw cgroup mutation or process-tree scanning is used. Original process death,
original cgroup absence/empty population and fresh native inactivity are all
required before cooldown. A replacement while stopping trips the recovery.

Backend cooldown requires continuously healthy known headroom. It has an
absolute `cooldown_ms + 60 seconds` deadline, so unhealthy recovery cannot wait
forever. Starting has a 10-second verification deadline. Restart dispatch
rechecks current host headroom, native lifecycle and the persisted phase's
in-memory authority after its manager query. Queued, expired or revoked actions
cannot inherit a replacement phase. Failed queries, unavailable workers,
unknown termination and exhausted budgets trip rather than broaden authority.

Attempts survive supervisor restart/reboot and policy edits. The accounting
window only grows: shortening policy does not forgive existing recovery
history. A backwards wall clock forbids another attempt; cross-boot accounting
assumes a trustworthy wall clock. Persisted in-flight recovery becomes `tripped`
on startup and is never replayed automatically.

To reconcile a trip, stop the supervisor and resolve the native domain first:

```sh
amc supervise reconcile --state /var/lib/amc-supervision
```

Reconciliation takes the same exclusive state lock and proves original native
termination before clearing the active recovery. It preserves all attempts and
does not start work. A running replacement, replaced cgroup or unreadable state
requires resolving that ambiguity before reconciliation can succeed.

## Evidence and failure behavior

`/run/amc-supervision/health.json` and `status.json` are public read-only files
written atomically with mode 0644, including under service `UMask=0077`.
Admission refuses new entries/grants on missing, malformed, future, wrong-boot,
older-than-three-second, inhibited or degraded health. Existing grants remain
charged. A gracefully stopped enforce-mode supervisor immediately publishes
inhibition; abrupt loss is detected through freshness.

`/var/lib/amc-supervision/recovery.json` and rotating traces are private. Each
trace file rotates at 64 MiB; trace backpressure/failure increments a visible
drop counter. The sample loop has no manager subprocesses. Discovery and
actuation use separate bounded workers; stale discovery inhibits admission.
Durable transitions still perform synchronous filesystem writes, so a stalled
filesystem can stall supervision. Heartbeat freshness and native hard limits
remain the fallback in that case, not a promise of successful software recovery.

Local Rust tests cover forecasting, delayed scoring, identity/gap censoring,
pidfd identity rejection, admission inhibition, recovery deadlines/budgets and
replay segmentation. `nixosTests.x86_64-linux.supervision` defines closed-loop
backend/descendant cleanup, once-only restart, in-flight supervisor restart,
shadow authority and trace/replay cases. Hosted CI must retain its named JUnit
cases and trace/recovery receipts. Defined tests are not passing VM evidence;
application thresholds still need chronological replay, shadow observation and
controlled consumer qualification before enabling forecast recovery.
