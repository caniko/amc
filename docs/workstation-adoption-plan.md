# AMC Workstation Adoption Plan

Recorded: 2026-09-20.

Status: planning record, not an implemented feature set or release-readiness claim.
The user requested that this discussion be preserved. That authorizes this
document, not deployment, stress testing, or every proposed architectural choice.
The user is the decision owner; implementation owners and dates are unassigned.

Implementation update: the user subsequently requested "implement telemetry so
we can test in production". The [telemetry prerequisite](#telemetry-prerequisite-2026-09-20)
below records that narrower delivered slice. It does not close the gaming
benchmark, policy, coordination, or broader-adoption gates.

Diagnostic-preview update (2026-09-29): `amc report CAPTURE_DIR` now exposes the
existing offline validator through the installed CLI, with a Markdown default,
JSON option, and an [install-to-report walkthrough](../README.md#install-and-first-report).
The Nix package supplies Python; Cargo installations need Python 3.11+ on
`PATH`. A Nix package build and read-only report against the recorded Atlas
capture passed. An Ubuntu 24.04.5 LTS rootless Podman container (amd64 image
digest `sha256:496754492fb28b4d3049432f2ca787449331e23fb14f0dd3fffea86bf5a93eb4`,
4 GiB memory and 2 CPU limit) compiled the checkout with Rust 1.97.1,
`cargo install --path /src --locked`, and Python 3.12.3. Validator regressions
passed, and the installed CLI reported the recorded 1,800-sample Atlas capture
in Markdown and JSON. This verifies source installation and offline reporting
on that Ubuntu image; live `inspect`/`watch` still needs a booted Ubuntu
systemd/cgroup v2 host. No gaming benefit has been verified.

## Goal And Confirmed Decisions

The user's goal is general adoption in prosumer/workstation situations and to
make AMC "the best memory manager". The following choices were explicit:

- "Modular design in every direction", rather than choosing an exclusively
  automatic, opt-in, or diagnostic product.
- No universal default priority between responsiveness, preserving running
  work, and maximizing useful throughput.
- Gaming plus background work is the first end-to-end acceptance scenario.

Proposed product goal: a composable memory-management toolkit that lets users
select their tradeoff, then demonstrates that it delivers that tradeoff better
than appropriate alternatives. It complements native memory management rather
than replacing the kernel allocator or the application's lifecycle owner.

"Best" is not maximum feature count, minimum reported RAM usage, or killing an
offender fastest. Evaluate results for an explicitly selected objective, subject
to safety and resource constraints. Report tradeoffs instead of hiding them in
one weighted score. No numerical rankings or universal weights were agreed.

The first opt-in pilot does not limit the eventual product to opt-in operation.
Optional automation remains a candidate, not an approved whole-desktop daemon.

## Authority And Scope

The current [README](../README.md) and [RFC 0.4](rfc-v0.4/rfc-application-memory-policy-v0.4.md)
describe a narrower diagnostic, integration, and fixture project. This plan
records a proposed expansion; it does not silently supersede that contract.
Revise the product contract deliberately before implementing the expansion.

Working assumption: Linux with cgroup v2 and systemd is the first enforcement
backend, because that is the existing implementation. Other platforms and
backends require demonstrated demand and capability-specific tests. Standalone
admission must not acquire a mandatory systemd or daemon dependency.

Work is scoped to this AMC repository. Do not change Canix GameMode policy,
production service placement, launchers, restoration behavior, pins/gitlinks,
the headroom observer, or the freeze/thaw controller as part of this plan.
Cross-repository integration needs a separate scope and authorization.
The later request to proceed authorized the narrow, read-only Atlas binding
pilot recorded below; it did not authorize changing active service policy.

Do not install a new policy language, global process-name classifier, desktop
launch broker, plugin loader, or competing OOM manager merely to make the design
look modular. New mechanisms must solve a demonstrated requirement.

## Current Foundations And Evidence

These are source/document observations from the planning session, not a fresh
runtime validation:

| Area | Existing foundation | Important limitation |
|---|---|---|
| Observation | [amc-telemetry](../crates/amc-telemetry/src/lib.rs) provides bounded readers, identity, comparisons, and explicit unknowns | It is independent of systemd transport, Tokio, subscribers, and exporters; preserve that boundary |
| Diagnostics | `amc inspect`, finite `amc watch`, offline `amc diff`, and `amc report` | Collection completion is not complete lifetime accounting; missing terminal counters stay unavailable |
| Admission | [amc-runner](../crates/amc-runner/src/lib.rs) has sync/async gates, providers, weighted admission, and diagnostics | Admission affects participating work; it cannot control every browser, game, broker, or already-running allocation |
| Managed execution | [Runner](../crates/amc-runner/src/systemd/managed.rs) couples admission to owned systemd attempts and retains uncertain reservations | Native enforcement is verified separately; independent Runner instances do not share one global budget |
| Coordination | [Coordinator](../crates/amc-runner/src/coordinator.rs) shares a byte budget in-process | Cross-process transport, authentication, and restart reconciliation are future work |
| Packaging/checks | [flake.nix](../flake.nix) provides Linux packages, checks, feature-matrix validation, and an explicit VM target | NixOS-focused infrastructure is not evidence of broad distribution/version support |
| Mechanism evidence | [Recorded VM results](rfc-v0.4/proof-results.md) demonstrate specified containment, lifecycle, and integration behavior | Pressure comparisons, aggregate policy benefit, real gaming outcomes, and general adoption remain unproven |

See [telemetry](telemetry.md), [admission contract](admission-contract.md), and
[known boundaries](known-boundaries.md) for the behavioral details.

Important existing semantics to preserve:

- Real applications retain their native lifecycle owner. Controlling an
  activation client does not control an independently running server.
- Native unit/drop-in precedence remains authoritative. Requested settings are
  not effective settings until inspected.
- Per-workload caps are not aggregate admission control. Identify the relevant
  ancestor budget and every external execution domain.
- Grants are permission to grow within a declared ceiling, not reserved physical
  RAM or an allocation-success guarantee. Current accounting is conservative and
  can overlap reservations with already observed usage.
- The grant contract is non-revocable during a bounded task. Release requires
  confirmed termination; a client disconnect or timeout alone is insufficient.
- Same-user controls are not a sandbox against a process with equivalent
  authority. Administrative constraints require the appropriate native owner.

### Prior Observer Work

The earlier session summary reported completed observer hardening: structural
terminal-state classification; verified-prefix rather than cross-lifetime
deltas; pinned reads; independent final identity/result queries; per-field
timing/coverage; persistence-aware readiness/completion; and explicit
disappearance, restart, replacement, and unknown outcomes. Admission probe
generation checks and lifecycle/integration regressions were also reported.

That summary reported successful formatting, workspace tests, clippy, runner
feature-matrix tests/docs, fixture checks, and a disposable VM run including
OOM attribution, restart, and frozen-leaf observation. Its reported VM derivation
was `/nix/store/rffsmsrph9mg8a8xsf7by5kf6612mq2s-vm-test-run-amc-generic.drv`.
These results were not rerun or independently reattributed to the current tree
during brainstorming. Do not use this paragraph as fresh validation evidence.

The VM OOM case uses observed `eventDeltas.oom > 0`, zero observed swap, and
manager `Result=oom-kill`; it does not assume a positive `oom_kill` counter on
every tested kernel. Frozen state is not termination or proof of memory relief.
Retain guards and the deprecated observer until their own removal criteria pass.

At plan creation, six files already had staged changes: weighted admission,
telemetry measurement, telemetry documentation, the VM driver, `src/watch.rs`,
and `tests/observe.rs`. Telemetry documentation also had unstaged changes.
Preserve that work and staging; do not reset, restage, or commit it implicitly.

## Proposed Modular Boundaries

These are logical responsibilities, not instructions to immediately split the
repository into additional crates, processes, or services.

| Capability | Responsibility | Independence requirement |
|---|---|---|
| Observation | Usage, pressure, identity, coverage, effective limits | Useful without policy changes or a resident controller |
| Policy selection | Explicit objectives, constraints, scope, permitted actions | No silently selected global priority profile |
| Admission | Decide when new cooperating work may start | Embeddable without a daemon |
| Shared coordination | Coordinate independently running participants | Optional; unnecessary for one shared in-process runner |
| Enforcement | Apply supported native controls to authorized resource domains | Backend-specific, capability-checked, with no false containment claim |
| Integrations/presentation | Application hooks, desktop events, CLI/UI, packaging | Optional consumers rather than alternative policy engines |

Each controlled domain needs a clear decision owner. Modules must not race to
write the same resource settings. Reject or explicitly resolve conflicting
requests while respecting administrator constraints and native precedence.
Do not invent a parallel generic merge engine where native rules suffice.

Distinguish user-facing priority selection from implementation defaults. The
request for no universal priority profile does not authorize deleting existing
library safety defaults or changing shipped behavior without review.

Optional automation must have explicit enrollment, action permissions, visible
decisions, and a disable path. Starting with observation-only onboarding is a
proposal, not a newly agreed default policy.

## Candidate Research Directions

All candidates below are AI-assisted ideas from this discussion, not findings.
There are no independent empirical ratings. Evaluation criteria are safety as a
gate, the selected user objective, effects on useful background work, information
gain, feasibility, reversibility, overhead, and implementation cost.

| ID | Idea and assumption | Predicted observation | Evidence against advancing |
|---|---|---|---|
| G1 | Make native policy easy to configure and inspect; assumes integration friction is a major barrier | Users obtain useful gaming/background tradeoffs without another controller | Native settings cannot express or reliably deliver the selected behavior |
| G2 | Prevent background overcommit through weighted admission; assumes useful memory-demand estimates | Better frame-time stability or useful background work than native limits with sensible fixed concurrency | Adequately precise matched comparisons establish no worthwhile benefit, or estimates cause excessive queuing/starvation |
| G3 | Adapt cooperating workloads to pressure; assumes safe concurrency/cache reductions exist | Better results than static admission under changing demand | Oscillation, delayed response, cache rebuilding, or overhead outweighs the benefit |

Strongest simpler alternative: native controls plus useful configuration,
diagnostics, and integration tests. If that suffices, ship that result rather
than treating a new controller as the required outcome.

Current weighted defaults include a 1 GiB reserve, 8 GiB exclusivity threshold,
and 0.6 page-cache fraction. These are existing implementation defaults, not
validated gaming recommendations. Test their contribution and calibration;
do not equate a large cache with harmful pressure or low free RAM with failure.

After evidence checking, the proposed first slice was narrowed to managing
enrolled background jobs through one runner while leaving game launch unchanged.
This avoids assuming a cross-process coordinator or game integration is needed
to establish the first benefit.

## First Experiment: Gaming And Background Work

Focal question: can AMC improve an explicitly selected gaming/background
tradeoff beyond well-configured native controls?

Keep the game on its ordinary launch path. Discover its actual execution domain
on the chosen setup; do not assume a universal Steam unit name, infer containment
from a launcher PID, or require the game to use the admission library.
Enroll only selected background jobs under one shared runner and known native
boundaries. This proves nothing yet about whole-desktop automatic management.

| Arm | Configuration | Purpose |
|---|---|---|
| A | Existing distro behavior and ordinary background execution | Establish the user's starting point |
| B | Explicit native memory controls and reasonable fixed background concurrency | Establish the strongest simple baseline |
| C | The same native controls with AMC weighted admission | Isolate admission's incremental value |
| D, conditional | C plus pressure-aware adaptation | Test adaptation only after earlier evidence justifies it |

Use identical offered work, inputs, game settings, and native limits for B/C.
Record any intentional differences in scheduling. Choose baseline concurrency
without using the final comparison data to favor AMC. Attribute gains from
native controls separately from gains due to AMC.

### Outcomes And Measurement

- Game frame-time distributions, especially p99 and long-stall frequency;
  average FPS alone is insufficient. Frame-time percentiles and 1% low FPS are
  different summaries and must not be conflated.
- Completed useful background work, completion times, admission waits,
  starvation, and recovery after gaming ends. Merely stopping all background
  work is not an unexplained success.
- OOM events with mechanism/victim attribution, lost work, application failures,
  memory and IO pressure, swap activity, and observed memory usage.
- AMC CPU, memory, launch/admission latency, and diagnostic overhead, measured
  with release builds and relevant enabled/disabled comparisons.
- Coverage and identity for each observation window. Missing final counters or
  collection failures remain explicit; do not discard inconvenient failed runs.

Reuse suitable existing tools such as MangoHud for frame-time collection rather
than building an overlay. Verify logging resolution and instrumentation overhead
for the selected game. Align game and resource measurements in time.
The existing finite observer is not automatically a whole-session collector;
choose bounded windows or explicitly design the additional collection needed.

### Experimental Controls

- Begin with one identified machine, one repeatable game scene, and finite,
  repeatable background jobs. Archive/extraction, builds, and indexing are
  candidate workloads, not all mandatory for the first experiment.
- Record hardware, RAM, GPU/VRAM, distro, kernel, systemd, game/Proton/driver
  versions, swap/zram, resource ancestry, active OOM managers, and effective
  settings. Pin source/configuration identities for each attempt.
- Randomize arm order and block on machine/session where appropriate. Control
  shader-cache state, warm-up, thermals, GPU load, and unrelated background work.
- Treat independent runs as replicates, not individual frames. Determine
  repetition counts and practical improvement/noninferiority margins from a
  separate calibration pilot before comparative evaluation.
- Predeclare the selected objective, guardrails, observation windows, exclusion
  rules, and analysis. Report uncertainty and sensitivity to workload/parameters.
- Preserve negative, null, and inconclusive results. Lack of statistical
  significance is not proof of equivalence or evidence that a mechanism is useless.
- Compare against existing pressure management where deployed. Do not silently
  disable an OOM manager to obtain a favorable result. Swap/zram variations are
  separate controlled conditions, not incidental changes between arms.

VMs validate mechanisms, not real GPU/game responsiveness. Replay tests can
validate decision logic but cannot establish the performance of a closed-loop
policy that would change the trace itself.

### Risks And Safety

- System-RAM admission does not establish VRAM control. Dedicated VRAM and
  integrated-GPU shared memory need separate observations and claims.
- CPU saturation, IO contention, shader compilation, and application faults can
  resemble memory-induced stutter. PSI complements application measurements;
  it does not identify every cause.
- Admission limits new participating work, not already committed allocations
  or arbitrary nonparticipants. Preserve headroom and aggregate boundaries.
- Freezing a workload is not memory release and can retain dependencies needed
  elsewhere. A new freeze/thaw controller is outside this first slice.
- Stress only an explicitly authorized test machine/session with finite loads,
  a host resource budget, cleanup, and a watchdog outside the tested domain.
  Do not stress or reconfigure a production workstation merely to gather proof.
- Workload output can contain secrets. Keep default evidence allowlisted and
  local; do not upload personal, proprietary, or unpublished workload data to
  external AI or visualization services without authorization.

## Accounted Observer Prerequisite

The PSI validator now rejects unexpected top-level rows, explicit-null CPU
`full`, and out-of-range large integer averages through the normal invalid-input
path. Regression inputs contain complete PSI rows so malformed-total tests
exercise the intended guard rather than missing average fields.

Implemented `scripts/capture-accounted.py`: a bounded, private session runner
using a uniquely named transient user observer, native accounting, retained
post-exit state, durable export, and then observer-only cleanup. It copies and
checksums the executable before launching and records unavailable counters as
unknown rather than zero. Target service/scope configuration is untouched.
Usage and recovery boundaries are in the [accounted observation guide](telemetry.md#accounted-observation).

Short real-host verification (export timestamp 2026-09-20T23:50:34Z):

- Target: `systemd-journald.service`, observed read-only for three seconds.
- Session: `/data/scratch/tmp/opencode/amc-accounted-smoke-01/`.
- Observer: `amc-observe-33cf084e75f043b79ceeb15a9d3ddc41.service`, invocation
  `41e48d7faed8402bbfe30d8f401e079c`.
- Final accounting: 22,202,000 ns CPU, 3,149,824 bytes peak memory, 32,768 block-IO
  bytes read, 233,472 block-IO bytes written, 2 read and 27 write operations.
- Main-process monotonic duration: 3,025,033 microseconds. These are native
  observer-cgroup measurements including children, not per-sample read timing
  and not total system overhead induced by observation.
- Accounting was exported while the observer was `active/exited`; cleanup then
  stopped it. A subsequent query returned `LoadState=not-found` and inactive.

Tests cover units/sentinels, zero versus unavailable values, private files,
export-before-stop ordering, preservation after export failure, cancellation,
and observed invocation replacement without controlling the replacement.
They are included in `scripts/check-fixtures.py` and do not contact a real manager.

This closes the short accounting-path prerequisite, not the longer accounting
experiment. The original endurance run still has no CPU accounting; do not
retroactively assign these smoke results to it. A further passive long run,
named-game/frame-time work, and comparative policy evaluation remain pending.

## Ordered Implementation Slices

The telemetry prerequisite below is implemented. The larger roadmap items remain
pending. Check them off only with attributable verification, not because an
interface or test exists.

### Telemetry Prerequisite (2026-09-20)

- [x] Add explicit passive `amc watch UNIT --production` mode using the existing
  collector, rather than a new daemon or policy engine.
- [x] Accept native `.service` and `.scope` owners in read-only diagnostics,
  without adding scope launching or changing application placement.
- [x] Default to 30 minutes at 1 Hz; validate up to 24 hours with 1-60 second
  sampling, 86,400 samples, and a 256 MiB sample-stream ceiling per observer.
- [x] Add allowlisted aggregate memory/swap, swap/fault counters, and
  CPU/IO/memory PSI from the observer's procfs view, with explicit unknowns and
  bounded reads. Retain older CPU PSI without fabricating a missing `full` row.
- [x] Flush production samples for live readers, preserve final durability and
  identity checks, interrupt long sleeps on cancellation, skip missed sampling
  slots rather than burst, and record collection/timing/budget metadata.
- [x] Document practical usage, clock/namespace/privacy boundaries, cancellation,
  output limits, and unmeasured gaming/VRAM/performance claims in
  [the production capture guide](telemetry.md#production-capture).

Verification for this slice: workspace/all-target tests (186 tests), strict
workspace clippy, formatting, fixture regressions, and the runner feature matrix
(default/none/sync/async/sysinfo/systemd tests, clippy, and documentation) passed.
Commands use `direnv exec .`; the workspace test command is
`cargo test --workspace --all-targets --locked -j 2`. Clippy was run directly as
`cargo-clippy clippy --workspace --all-targets --locked -j 2 -- -D warnings`:
`cargo clippy` selected a stale rustup subcommand despite the pinned shell and
failed on a missing linker wrapper. No global toolchain configuration was changed.

An existing runner test, `detached_reservation_lasts_until_confirmed_termination`,
had one transient `SpawnFailed` result in the first feature-matrix run. Its
isolated rerun and the full unchanged matrix passed. The cause was not established;
record this as residual test flakiness, not as a fixed telemetry defect.

A three-second real-manager, read-only capture of `systemd-journald.service`
produced three persisted samples, `reason: deadline`, `complete: true`, and
`storageDurable: true`. Local smoke artifacts are under
`/data/scratch/tmp/opencode/amc-production-telemetry-20260920-01/`.
This is collection-path evidence only, not gaming, stress, long-session endurance,
or release-build overhead evidence. No services or policies were changed and no
new VM run was performed for this slice. Existing staged work was preserved.

Still needed: the actual gaming target and frame-time capture, representative
background useful-work measurements, calibrated thresholds, comparative A/B/C
results, and long-session/overhead validation. No production deployment,
cross-process coordinator, automatic priority policy, or frame-time/VRAM collector
was added. Ordinary `watch` defaults remain unchanged.

### Atlas Pilot Readiness (2026-09-20)

Pilot host is Atlas. Read-only discovery found Steam (`app-cosmic-steam-*.scope`)
and Lutris scopes running, plus `gamemoded.service`. No game owner was
identified (scopes were present; which one owns a game was not established);
scope names embed PIDs and must be rediscovered per session.
MangoHud was not on `PATH`; frame-time logging availability is pending.

Validated on Atlas, no policy or service changes:

- `amc inspect` accepts `.scope` units and reports the Steam scope's cgroup,
  invocation, limits, and kernel observations.
- `amc watch SCOPE --production --seconds 3` completed (`reason: deadline`,
  `complete: true`, 3/3 samples, 0 missed) with full host context.
- Release binary `target/release/amc` (`amc 0.1.0`, sha256
  `05c94a2b68f27fb10032394a38f186411cda1551453b97aa41ce8b04d8b44520`)
  captured the Steam scope; per-sample read-path timing peaked near 198us
  (debug: ~1127us). That is capture-read timing from `captureDurationUs`,
  not a whole-observer overhead estimate.
  Smoke artifacts: `/data/scratch/tmp/opencode/amc-atlas-steam-scope-01/`,
  `/data/scratch/tmp/opencode/amc-atlas-steam-release-01/`.

Session protocol when the game is running (passive only, user-run):

1. Discover owners: `systemctl --user list-units --type=scope --state=running`
   and `systemctl --user show --property=ControlGroup,InvocationID,Slice <scope>`.
   Record the game scope plus any background job units actually running.
2. Record environment: game/launcher and settings, kernel/driver, swap/zram,
   `amc --version`, release binary sha256, and `amc inspect <scope>` output.
3. Capture 30 minutes at 1 Hz per target into separate new directories:
   `amc watch <scope> --production --output <new-dir>` (release binary).
   Log frame times separately if supported; note visible stutters and normal exit.
4. Validate before interpreting: check `reason`, `complete`, `coverage`,
   `collection` (bytes/missed/capture lag), unknown metrics, and observer CPU,
   memory, and write activity. Each observer's monotonic clock starts at zero;
   correlate via wall-clock timestamps and boot IDs.
5. Stop only the observers if they interfere. Do not change limits, restart
   apps, touch swap, disable OOM handling, or follow replacement workloads.

Still open: a repeatable game scene and its actual runtime owner, calibrated
background work, frame-time logging, and a scheduled 30-minute session.

### First comparison candidate (2026-09-30)

**Counter-Strike 2** (Steam app 730) is installed on Atlas and is the first
comparison candidate. An offline, locally recorded demo is the proposed scene:
record a fixed 60–90 second route on the same training map, archive the demo
and its hash, and replay the same file for each arm. No replay has been
selected or verified yet, so the scene, playback determinism, and a
representative memory-pressure level remain calibration gates, not established
facts. Do not use a public network match as the measurement scene.

The selected objective for this first experiment is **gaming-first**: compare
the p99 frame time and frequency of long stalls for C versus B, subject to a
minimum of completed, useful background work. Arm A provides context for the
user's starting point. Choose the stall threshold, the useful-work floor, the
practical improvement margin, and the number of independent runs from separate
calibration attempts before collecting comparison results. Report both the
responsiveness and throughput outcomes even if the primary objective fails.

MangoHud 0.8.4 is present in the Nix store but not on `PATH`. Its
[upstream configuration](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/data/MangoHud.conf)
supports `output_folder`, `autostart_log`, `log_duration`, and `log_interval`.
First verify that it logs per-frame data for this game and quantify its
instrumentation overhead using otherwise identical launches; retain the same
logging configuration in every comparison arm. A frame-time CSV must be
aligned by run window with each AMC capture; an on-screen FPS value is not a
substitute.

Candidate useful background work is a fixed, clean AMC Cargo workspace build
or test batch with pinned source/lock/toolchain and a fresh, isolated target
directory per run. Predeclare the finite batch, verify it actually competes
for memory on Atlas, and record completion, elapsed time, failures, and
admission waits. For arms B/C, use identical native per-job and aggregate
limits and the same offered batch; C enrolls it under **one shared** weighted
runner, while B uses calibrated fixed concurrency. The installed `amc run`
fixture command is not a weighted-admission runner. Tune the work estimate and
comparison margins on separate calibration runs; do not infer benefit from
the prior passive capture or count frames as independent replicates.

### Consumer-owned Fleetix binding pilot (2026-09-30)

Canix now exposes a read-only Atlas binding in
`root/hosts/atlas/features/amc_binding.nix`: Fleetix supplies the declared
`atlas` host and `can` account, while Canix names `opencode.service`, its
`app-amc.slice` owner, and the `agent-tools.slice` domain. Memory limits come
from `canix.amcContracts`, not new Fleetix workload fields. The binding adds
no resource-policy activation or memory-budget decision to AMC.

Guarded evaluation of `nixosConfigurations.atlas.config.canix.amcBinding` and
the Atlas toplevel derivation passed. Read-only live inspection found the
backend in `app-amc.slice` at 32 GiB/2 GiB max/swap, the tool slice at
24 GiB/8 GiB aggregate, and one active tool service in that slice at
16 GiB/4 GiB. These are identity/boundary checks on the currently running
generation, not proof that a weighted runner is coordinating those jobs or
that the new Nix binding has been activated.

Repeat the acceptance check from the Canix checkout on Atlas after the Canix
configuration changes (`canix` and `amc` must both be on `PATH`):

```sh
canix repo eval --output json .#nixosConfigurations.atlas.config.canix.amcBinding
amc inspect opencode.service --json
systemctl --user show agent-tools.slice \
  --property=Id,LoadState,ActiveState,ControlGroup,MemoryMax,MemorySwapMax
# While a tool job is active, replace UNIT with its discovered service name:
systemctl --user list-units 'amc-opencode-tool-*.service' --state=running
amc inspect UNIT --json
```

Require an active `opencode.service` whose `Slice`, leaf cgroup, and effective
`MemoryMax`/`MemorySwapMax` match the declaration (32 GiB/2 GiB). Require an
active `agent-tools.slice` with a cgroup ending in that slice and matching
aggregate limits (24 GiB/8 GiB). For a live tool service, require its `Slice`
and cgroup to place it beneath `agent-tools.slice`, with the Canix per-job
contract (16 GiB/4 GiB). Check AMC's cgroup `memory.max` and `memory.swap.max`
cells as well as manager properties; fail rather than treating an unknown cell
as a match. Re-discover transient unit names on each run. A backend match alone
does not attest to the tool domain, and native placement does not establish
gaming responsiveness or coordinated admission.

### Atlas Endurance Run (started 2026-09-20 ~21:19 local)

A first backgrounded launch (`amc-atlas-endurance-01`, PID 2164183) exited after
2 samples with `reason: cancelled`, `complete: false`, durable prefix intact.
Suspected cause: the tool shell reaped the backgrounded child when its command
returned (alive at +2s, SIGTERM-handled summary before +14s, no other operator
action). This is suspected, not proven. The partial directory is retained as
aborted-run evidence; it must not be presented as endurance data.

The rerun uses a transient user unit so the observer is a child of the user
manager, not the tool shell:
`systemd-run --user --unit=amc-endurance-01` running release `amc 0.1.0`
(invocation `8f2a3cd22f0f4019a669029740937ed6`),
`watch app-cosmic-steam-1439422.scope --production --seconds 1800`
into `/data/scratch/tmp/opencode/amc-atlas-endurance-02/`.
Early health (~15s): unit active/running, `ready` present, 15/15 samples,
~2.8KB/sample (~5MB projected, well under the 256MiB cap), observer ~4MB RSS.
Completed ~21:49 local: `reason: deadline`, `complete: true`, 1800/1800 samples,
0 missed intervals, contiguous sequences, single invocation, zero unknown host
cells. Full data-quality report: [atlas-endurance-01](atlas-endurance-01.md).
Validation is reproducible: `python3 scripts/validate-capture.py <capture-dir>`
checks schema, contiguity, counts, bytes, and identity, and derives report
metrics (nearest-rank p99, rates over the sample span); its regressions live in
`scripts/test-validate-capture.py`. The aborted first attempt validates as
structurally sound but incomplete, as required.
Residual gap: observer CPU time was not measured (only RSS); capture timing
covers reads, not total observer cost. Aggregate Steam scope telemetry must not
be presented as game-specific evidence.

### 1. Product Contract

- [ ] Reconcile broader product positioning with the current fixture-only scope.
- [ ] Specify independent capabilities, enrollment, authority, permitted actions,
  failure behavior, and disable/uninstall behavior without a universal priority.
- [ ] Confirm the initial platform, first pilot machine/game/workload, and owners.

Acceptance: users can tell what AMC controls, what it cannot control, how policy
is selected, and how to stop using it. No speculative framework is required.
Dependencies: user review of unresolved scope choices. Likely area: project docs.

### 2. Gaming Baseline

- [ ] Reuse the observer/harness foundations and add only missing repeatable
  workload and frame-time collection pieces.
- [ ] Run calibration, then fix the A/B/C protocol and practical acceptance margins.
- [ ] Produce matched, attributable, reproducible A/B/C results with uncertainty.

Acceptance: the data distinguishes native enforcement benefit from admission
benefit and includes useful background work, failures, coverage, and overhead.
Dependencies: slice 1 and explicit hardware-test authorization. Likely areas:
test scripts/fixtures and evidence records; observer changes only if required.

### 3. Opt-In Gaming/Background Slice

- [ ] Provide one complete, explicitly selected policy/enrollment path using
  existing admission and native enforcement where possible.
- [ ] Test reserve/cache heuristics, queued-work behavior, cancellation,
  starvation, and background recovery after the gaming phase.
- [ ] Retain only justified behavior; document supported outcomes and limitations.

Acceptance: improvement in the selected objective within predeclared guardrails,
or a recorded result supporting the simpler native-only direction. An integration
or usability improvement must be labeled separately from a performance gain.
Dependencies: slice 2. Do not require Steam/GameMode changes for this slice.

### 4. Shared Coordination, Conditional

- [ ] Confirm that independently running clients actually need one shared budget;
  one runner serving several jobs is not evidence that IPC is required.
- [ ] Define authenticated ownership, bounded requests, reservation lifecycle,
  persistence/reconciliation, versioning, and unavailable-coordinator behavior.
- [ ] Test concurrent clients, disconnects, crashes/restarts, detached work,
  unknown identities, and unauthorized attempts to control or release work.

Acceptance: no premature capacity release, duplicate grants, unauthorized
control, or silent unrestricted fallback. Reconcile existing workload identities
before admitting new work after recovery. Dependencies: demonstrated multi-client
need and the existing admission/managed-execution contracts; not a pilot blocker.

### 5. Broader Adoption

- [ ] Validate install, upgrade, configuration discovery, removal, and useful
  diagnostics on declared supported environments, including non-NixOS users.
- [ ] Expand to additional machines, kernel/systemd versions, RAM/storage/GPU
  configurations, and workloads without extrapolating from one gaming run.
- [ ] Publish module-specific support, compatibility, benchmark results,
  failure/rollback procedures, and known limitations.

Acceptance: reproducible user-facing setup and recovery, explicit support bounds,
and no blanket readiness claim for unvalidated modules. Dependencies: an accepted
initial slice and module-specific safety evidence. Other workload families remain
future validation targets, not demonstrated capabilities.

## Verification And Release Gates

For code changes, retain the existing workspace tests, clippy, formatting,
runner feature-matrix tests/docs, and fixture-script checks. Inspect the current
flake/README for exact commands and use the pinned project environment. Run cheap
checks before the separately invoked, resource-budgeted VM mechanism tests.

For authorized local flake realization, use the approved cache workflow,
`canix cache build .#nixosTests.x86_64-linux.generic`, with private publication
enabled and resource limits selected for the actual host. A cached output is not
proof of fresh execution. Capture command, source/dirty-tree identity, versions,
configuration, artifact locations, and verification outcomes for each claim.

Release gates apply regardless of selected performance objective:

- No unauthorized targets or second writer against manager-owned cgroups.
- No silent unrestricted fallback when managed operation was requested.
- No automatic replay of uncertain submissions or arbitrary interrupted work.
- No reservation release based only on elapsed time or client disappearance.
- No hidden policy/priority selection or claim of effective controls from config
  text alone.
- Explicit behavior on missing capabilities, incomplete evidence, and control
  failure; no invented zeros or false success.
- Coexistence with native lifecycle, administrator policy, and existing pressure
  management; no broad group-killing of a session or broker by default.
- Bounded resource use, privacy-aware diagnostics, and tested disable/recovery
  paths appropriate to each shipped module.

## Open Decisions And Next Action

Resolved since planning: pilot host is Atlas; telemetry prerequisite and scope
support are implemented and smoke-validated (see above). Still unresolved:
chosen game and launcher path, background work under test, frame-time logging
method, hardware-test approval for the 30-minute session, initial supported
versions/distributions, desired numerical tradeoff, interface for explicit
profile selection, and implementation ownership.
No launch date, universal threshold, public IPC API, or daemon mandate was agreed.

Next action: user names the game and starts it normally, then runs the Atlas
session protocol above. Do not build a plugin framework, cross-process daemon,
or automatic tuner before that evidence. Revisit G2/G3 after measured results;
revisit shared coordination when independent clients demonstrate the need.
A native-only result remains an acceptable outcome.

## Sources And Provenance

This is an AI-assisted planning record based on repository inspection, user
choices, a bounded upstream documentation check, and one adversarial AI review.
It is not an independent expert consensus, user study, exhaustive literature
review, or benchmark. Initial independent exploration attempts did not yield
usable completed reports; do not count them as corroborating reviewers.

Documentation checked on 2026-09-20:

- [Linux PSI](https://docs.kernel.org/accounting/psi.html): resource-stall metrics
  and threshold notifications, not proof of AMC outcomes.
- [systemd pressure handling](https://systemd.io/PRESSURE/): existing application
  cooperation protocol; prefer it over inventing a parallel pressure interface.
- [systemd-oomd source documentation](https://github.com/systemd/systemd/blob/main/man/systemd-oomd.service.xml):
  pressure-driven cgroup termination, not cooperative cache reclamation.
- [earlyoom](https://github.com/rfjakob/earlyoom/blob/master/README.md): an existing
  available-memory/swap-based intervention approach, not a measured comparator
  in this session.
- [MangoHud](https://github.com/flightlessmango/MangoHud/blob/master/README.md):
  existing graphics performance instrumentation to evaluate for the pilot.

These upstream URLs can change and may describe newer capabilities than deployed
versions. Pin relevant versions/revisions in executable experiments. The rendered
systemd resource-control and oomd manual URLs returned HTTP 418 during research;
oomd source documentation was consulted instead. No comprehensive competitive
performance survey or novelty claim was made.

The adversarial review reinforced launch-domain discovery, VRAM limits,
cross-process budget limitations, and policy-owner conflicts. Unsupported Steam
unit assumptions, a description conflating oomd with reclaim, and arbitrary
statistical thresholds were rejected rather than carried into the plan.

No tests, benchmarks, service changes, or policy changes were performed during
brainstorming. Creating this document does not change that evidence status.
