# AMC Workstation Adoption Plan

Recorded: 2026-09-20.

Status: implementation and rollout record. Historical comparison proposals are
optional diagnostics, not delivery dependencies. At plan creation, the user's
request to preserve the discussion authorized this record, not deployment,
stress testing or every architectural proposal. The user owns decisions; no
implementation owners or dates are assigned.

The user later requested "implement telemetry so we can test in production".
The delivered [telemetry prerequisite](#telemetry-prerequisite-2026-09-20)
doesn't by itself deliver consumer policy or broader adoption.

Diagnostic preview (2026-09-29): `amc report CAPTURE_DIR` exposes the existing
offline validator in the installed CLI with Markdown by default, a JSON option,
and an [install-to-report walkthrough](../README.md#install-and-first-report).
The Nix package supplies Python; Cargo installations need Python 3.11+ on
`PATH`. The Nix build and read-only report on the recorded Atlas capture passed.
A rootless Podman container ran Ubuntu 24.04.5 LTS (amd64 image
digest `sha256:496754492fb28b4d3049432f2ca787449331e23fb14f0dd3fffea86bf5a93eb4`,
4 GiB memory and 2 CPU limit) and built the checkout with Rust 1.97.1,
`cargo install --path /src --locked`, and Python 3.12.3. Validator regressions
passed. The installed CLI reported the recorded 1,800-sample Atlas capture in
Markdown and JSON, verifying source installation and offline reporting on that
image. The later [booted Ubuntu diagnostic smoke](ubuntu-diagnostics-20260930.md)
passed source installation, live inspection, completed and interrupted captures,
unavailable measurements, and both formats on Ubuntu 24.04.5 with systemd 255
and kernel 6.8. No verified gaming benefit.

Harness (2026-09-30): the [CS2 comparison guide](gaming-comparison.md) covers
operator-run recording, playback and per-present logging. The release
`workstation-batch` example runs matched native B/C finite batches, with one
shared weighted runner for C, output verification and retained failures. The
live small-hash smoke passed byte-budget serialization, literal argv, useful
output checks, negative verification, native-limit mismatch and termination.
Fixture regressions cover the offline frame reporter. None establishes
comparative performance.

Stability-first (2026-09-30): the selected objective is **maintain a stable,
usable foreground gaming session while bounded background tasks make useful
progress under shared resource pressure**. Operational acceptance covers
native limits/placement, visible admission, useful outputs, overload, recovery and
installation lifecycle. A calibrated A/B/C benchmark is optional. The
[workstation guide](workstation-policy.md) supplies standalone units/policy,
enrollment and removal. Disposable jobs can request a native execution deadline.
The native fixture covers client/coordinator loss; a separate fixture checks
install, restart with entered work, useful progress, disable and removal.
Canix's consumer profile keeps useful jobs running at lower priority, defers
maintenance separately, raises Atlas admission headroom, and bounds system
background work and daemon builds. Distinguish fresh package/consumer rollout
checks from source implementation and active host policy.

## Goal And Confirmed Decisions

The user wants broad prosumer/workstation adoption and AMC to be "the best memory
manager". Explicit choices:

- "Modular design in every direction", rather than choosing an exclusively
  automatic, opt-in, or diagnostic product.
- No universal default priority between responsiveness, preserving running
  work, and maximizing useful throughput.
- Gaming plus background work is the first end-to-end operational scenario.

Product goal: a composable memory-management toolkit for choosing tradeoffs and
running bounded workloads with clear overload and recovery behavior. It
complements native memory management without replacing the kernel allocator or
application lifecycle owner.

"Best" doesn't mean most features, least reported RAM usage, or fastest kill.
Evaluate results against the selected objective within safety and resource
constraints. Don't hide tradeoffs in one weighted score. No agreed numerical
rankings or universal weights.

The opt-in pilot doesn't preclude other modes. Optional automation is a
candidate, not an approved whole-desktop daemon.

## Authority And Scope

The current [README](../README.md) and [RFC 0.4](rfc-v0.4/rfc-application-memory-policy-v0.4.md)
define a narrower diagnostic, integration, and fixture project. This proposal
doesn't override that contract; revise it explicitly before expanding.

Working assumption: Linux with cgroup v2 and systemd is the first enforcement
backend, matching the existing implementation. Other platforms and backends
need demonstrated demand and capability-specific tests. Standalone admission
must not require systemd or a daemon.

Initial scope: this AMC repository, not Canix policy, launchers, restoration or
pins. Cross-repository integration needs separate scope and authorization.
The later go-ahead authorized only the read-only Atlas binding pilot below,
not active service policy changes. The stability-first implementation request
authorizes consumer source policy/integration work; deployment still uses the
consumer's native workflow.

Add no policy language, global process-name classifier, desktop launch broker,
plugin loader, or competing OOM manager just for modularity. Require a
demonstrated need.

## Current Foundations And Evidence

Planning-session source/document observations, not fresh runtime validation:

| Area | Existing foundation | Important limitation |
|---|---|---|
| Observation | [amc-telemetry](../crates/amc-telemetry/src/lib.rs): bounded readers, identity, comparisons, explicit unknowns | Preserve independence from systemd transport, Tokio, subscribers and exporters |
| Diagnostics | `amc inspect`, finite `amc watch`, offline `amc diff`, and `amc report` | Completed collection isn't complete lifetime accounting; missing terminal counters stay unavailable |
| Admission | [amc-runner](../crates/amc-runner/src/lib.rs): sync/async gates, providers, weighted admission, diagnostics | Participating work only; no blanket control over browsers, games, brokers or existing allocations |
| Managed execution | [Runner](../crates/amc-runner/src/systemd/managed.rs) links admission to owned systemd attempts and retains uncertain reservations | Native enforcement is verified separately; independent Runner instances don't share a global budget |
| Coordination | [Coordinator](../crates/amc-runner/src/coordinator.rs) shares an in-process byte budget; optional [persistent service](persistent-admission.md) coordinates across processes | The service's recovery/ownership acceptance and consumer rollout need separate evidence; the first single-runner comparison doesn't require it |
| Packaging/checks | [flake.nix](../flake.nix): Linux packages, checks, feature-matrix validation, explicit VM target | NixOS-focused infrastructure doesn't establish broad distribution/version support |
| Mechanism evidence | [Recorded VM results](rfc-v0.4/proof-results.md): specified containment, lifecycle and integration behavior | No proven pressure comparisons, aggregate policy benefit, real gaming outcomes or general adoption |

Behavior: [telemetry](telemetry.md), [admission contract](admission-contract.md)
and [known boundaries](known-boundaries.md).

Preserve these semantics:

- Applications keep their native lifecycle owner. Controlling an activation
  client doesn't control an independently running server.
- Native unit/drop-in precedence governs. Inspect effective settings rather than
  assuming requests took effect.
- Per-workload caps don't control aggregate admission. Identify the relevant
  ancestor budget and all external execution domains.
- Grants permit growth within a declared ceiling; they don't reserve physical RAM
  or guarantee allocations. Conservative accounting can overlap reservations with
  observed usage.
- Grants aren't revocable during bounded tasks. Release only after confirmed
  termination, not a disconnect or timeout alone.
- Same-user controls don't sandbox processes with equivalent authority.
  Administrative constraints need the appropriate native owner.

### Prior Observer Work

The earlier summary reported completed observer hardening: structural
terminal-state classification; verified-prefix rather than cross-lifetime
deltas; pinned reads; independent final identity/result queries; per-field
timing/coverage; persistence-aware readiness/completion; and explicit
disappearance, restart, replacement, and unknown outcomes. It also reported
admission probe generation checks and lifecycle/integration regressions.

Reported passes: formatting, workspace tests, clippy, runner feature-matrix
tests/docs, fixture checks, and a disposable VM run covering OOM attribution,
restart and frozen-leaf observation. Reported VM derivation:
`/nix/store/rffsmsrph9mg8a8xsf7by5kf6612mq2s-vm-test-run-amc-generic.drv`.
Brainstorming neither reran these checks nor independently attributed them to
the current tree. Don't cite them as fresh validation.

The VM OOM case uses observed `eventDeltas.oom > 0`, zero observed swap, and
manager `Result=oom-kill`; it does not assume a positive `oom_kill` counter on
every tested kernel. Frozen state is not termination or proof of memory relief.
Retain guards and the deprecated observer until their own removal criteria pass.

At plan creation, six files had staged changes: weighted admission,
telemetry measurement, telemetry documentation, the VM driver, `src/watch.rs`,
and `tests/observe.rs`. Telemetry documentation also had unstaged changes.
Preserve both; don't implicitly reset, restage or commit.

## Proposed Modular Boundaries

Logical boundaries, not an instruction to split crates, processes or services now.

| Capability | Responsibility | Independence requirement |
|---|---|---|
| Observation | Usage, pressure, identity, coverage, effective limits | Useful without policy changes or a resident controller |
| Policy selection | Explicit objectives, constraints, scope, permitted actions | No silently selected global priority profile |
| Admission | Decide when new cooperating work may start | Embeddable without a daemon |
| Shared coordination | Coordinate independently running participants | Optional; unnecessary for one shared in-process runner |
| Enforcement | Apply supported native controls to authorized resource domains | Backend-specific, capability-checked, with no false containment claim |
| Integrations/presentation | Application hooks, desktop events, CLI/UI, packaging | Optional consumers rather than alternative policy engines |

Assign a decision owner to each controlled domain. Don't let modules race to
write resource settings. Reject or explicitly resolve conflicts within
administrator constraints and native precedence. Use native rules rather than
a generic merge engine where they suffice.

User-facing priorities aren't implementation defaults. Declining a universal
priority profile doesn't authorize removing library safety defaults or changing
shipped behavior without review.

Optional automation needs explicit enrollment, action permissions, visible
decisions and a disable path. Observation-only onboarding is a proposal, not an
agreed default.

## Candidate Research Directions

All candidates below are AI-assisted ideas from this discussion, not findings.
There are no independent empirical ratings. Evaluation criteria are safety as a
gate, the selected user objective, effects on useful background work, information
gain, feasibility, reversibility, overhead, and implementation cost.

| ID | Idea and assumption | Predicted observation | Evidence against advancing |
|---|---|---|---|
| G1 | Simplify native policy setup and inspection; assumes integration friction is a major barrier | Useful gaming/background tradeoffs without another controller | Native settings can't express or reliably deliver the selected behavior |
| G2 | Prevent background overcommit with weighted admission; assumes useful memory-demand estimates | Capacity-bounded starts, visible waits, useful background progress | Excessive queuing/starvation from estimates or execution outside declared native boundaries |
| G3 | Adapt cooperating workloads to pressure; assumes safe concurrency/cache cuts | Better than static admission under changing demand | Oscillation, delayed response, cache rebuilds or overhead outweigh benefits |

Strongest simpler alternative: native controls with useful configuration,
diagnostics and integration tests. Ship that if sufficient; a new controller
isn't required.

Current weighted defaults include a 1 GiB reserve, 8 GiB exclusivity threshold,
and 0.6 page-cache fraction, not validated gaming recommendations. Test their
contribution and calibration. Large caches don't imply harmful pressure, nor
does low free RAM imply failure.

Evidence checks narrowed the proposed first slice to one runner for enrolled
background jobs, leaving game launch unchanged. Test for an initial benefit
without assuming cross-process coordination or game integration is needed.

## Optional Comparison: Gaming And Background Work

This optional experiment tests whether AMC changes an explicit gaming/background
tradeoff beyond native controls. It doesn't gate the operational profile.

Launch the game normally and discover its actual execution domain. Don't assume
a universal Steam unit name, infer containment from a launcher PID, or require
game use of the admission library. Enroll only selected background jobs under
one shared runner and known native boundaries. This doesn't validate automatic
whole-desktop management.

| Arm | Configuration | Purpose |
|---|---|---|
| A | Existing distro behavior and ordinary background execution | Establish the user's starting point |
| B | Explicit native memory controls and reasonable fixed background concurrency | Establish the strongest simple baseline |
| C | The same native controls with AMC weighted admission | Isolate admission's incremental value |
| D, conditional | C plus pressure-aware adaptation | Test adaptation only after earlier evidence justifies it |

Use identical offered work, inputs, game settings, and native limits for B/C.
Record intentional scheduling differences. Don't tune baseline concurrency to
favor AMC using final comparison data. Attribute native and AMC gains separately.

### Outcomes And Measurement

- Game frame-time distributions, especially p99 and long-stall frequency. Average
  FPS alone isn't enough; frame-time percentiles aren't 1% low FPS.
- Completed useful background work, completion times, admission waits, starvation and
  post-game recovery. Don't count stopping all background work as success
  without explanation.
- OOM events with mechanism/victim attribution, lost work, application failures,
  memory and IO pressure, swap activity, and observed memory usage.
- AMC CPU, memory, launch/admission latency and diagnostic overhead in release
  builds, with relevant enabled/disabled comparisons.
- Coverage and identity per window. Disclose missing final counters and
  collection failures. Keep inconvenient failed runs.

Use an existing frame-time tool such as MangoHud, not a new overlay. Verify
logging resolution and overhead for the game and align game and resource
measurements. The finite observer doesn't automatically cover whole sessions;
choose bounded windows or explicitly design additional collection.

### Experimental Controls

- Start with one identified machine, one repeatable game scene and finite,
  repeatable background jobs. Archive/extraction, builds and indexing are
  candidates, not all required for the first experiment.
- Record hardware, RAM, GPU/VRAM, distro, kernel, systemd, game/Proton/driver
  versions, swap/zram, resource ancestry, active OOM managers, and effective
  settings. Pin source/configuration identities for each attempt.
- Randomize arm order and block on machine/session where appropriate. Control
  shader-cache state, warm-up, thermals, GPU load, and unrelated background work.
- Use independent runs as replicates, not frames. Derive repetition counts and
  practical improvement/noninferiority margins from a separate calibration pilot
  before comparison.
- Predeclare the selected objective, guardrails, observation windows, exclusion
  rules, and analysis. Report uncertainty and sensitivity to workload/parameters.
- Preserve negative, null, and inconclusive results. Lack of statistical
  significance is not proof of equivalence or evidence that a mechanism is useless.
- Compare against deployed pressure management. Don't silently disable an OOM
  manager to favor results. Swap/zram variants are separate controlled
  conditions, not incidental arm differences.

VMs validate mechanisms, not real GPU/game responsiveness. Replay tests can
validate decision logic, not performance of a closed-loop policy that changes
the trace.

### Risks And Safety

- System-RAM admission does not establish VRAM control. Dedicated VRAM and
  integrated-GPU shared memory need separate observations and claims.
- CPU saturation, IO contention, shader compilation and application faults can
  mimic memory stutter. PSI complements app measurements but can't identify
  every cause.
- Admission limits new participating work, not already committed allocations
  or arbitrary nonparticipants. Preserve headroom and aggregate boundaries.
- Freezing doesn't release memory and can retain dependencies needed elsewhere.
  A new freeze/thaw controller is out of scope for the first slice.
- Stress only an explicitly authorized test machine/session: finite loads, host
  budget, cleanup and a watchdog outside the tested domain. Don't stress or
  reconfigure a production workstation just for proof.
- Workload output may contain secrets. Keep evidence allowlisted and local by
  default. Don't upload personal, proprietary or unpublished workload data to
  external AI or visualization services without authorization.

## Accounted Observer Prerequisite

The PSI validator rejects unexpected top-level rows, explicit-null CPU `full`
and out-of-range large integer averages via the normal invalid-input path.
Malformed-total regression inputs supply complete PSI rows so they reach the
total-value guard rather than failing on missing averages.

`scripts/capture-accounted.py` runs bounded, private sessions with a uniquely
named transient user observer and native accounting. It copies and checksums
the executable before launch, retains post-exit state, exports durably, then
cleans up only the observer. Unavailable counters are unknown, not zero. Target
service/scope configuration stays untouched. See the
[accounted observation guide](telemetry.md#accounted-observation) for usage and
recovery boundaries.

Short real-host verification (export timestamp 2026-09-20T23:50:34Z):

- Target: `systemd-journald.service`, observed read-only for three seconds.
- Session: `/data/scratch/tmp/opencode/amc-accounted-smoke-01/`.
- Observer: `amc-observe-33cf084e75f043b79ceeb15a9d3ddc41.service`, invocation
  `41e48d7faed8402bbfe30d8f401e079c`.
- Final accounting: 22,202,000 ns CPU, 3,149,824 bytes peak memory, 32,768 block-IO
  bytes read, 233,472 block-IO bytes written, 2 read and 27 write operations.
- Main-process monotonic duration: 3,025,033 microseconds. Native observer-cgroup
  measurements include children, not per-sample read timing or total overhead
  induced by observation.
- Exported accounting while the observer was `active/exited`, then stopped it.
  A later query returned `LoadState=not-found` and inactive.

Tests cover units/sentinels, zero versus unavailable values, private files,
export-before-stop ordering, preservation after export failure, cancellation,
and observed invocation replacement without controlling the replacement.
`scripts/check-fixtures.py` includes them; they don't contact a real manager.

Only the short accounting-path prerequisite is complete. Don't attribute these
smoke results to the original endurance run, which lacks CPU accounting. The
longer accounting experiment, another passive long run, named-game/frame-time
work and comparative policy evaluation remain pending.

## Ordered Implementation Slices

Source completion isn't consumer rollout or proof of active deployment.
Runtime/install results apply only to the recorded environment and revision.

### Telemetry Prerequisite (2026-09-20)

- [x] Add explicit passive `amc watch UNIT --production` mode to the existing
  collector, without a new daemon or policy engine.
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

Passed workspace/all-target tests (186 tests), strict workspace clippy,
formatting, fixture regressions and the runner feature matrix
(default/none/sync/async/sysinfo/systemd tests, clippy and documentation).
These checks used `direnv exec .`. Workspace tests ran
`cargo test --workspace --all-targets --locked -j 2`; clippy ran directly as
`cargo-clippy clippy --workspace --all-targets --locked -j 2 -- -D warnings`:
`cargo clippy` selected a stale rustup subcommand despite the pinned shell and
failed on a missing linker wrapper. No global toolchain configuration was changed.

The existing `detached_reservation_lasts_until_confirmed_termination` runner
test returned `SpawnFailed` once in the first feature-matrix run. It passed in
isolation and in the full unchanged matrix. The cause remains unknown: residual
test flakiness, not a fixed telemetry defect.

A three-second real-manager, read-only capture of `systemd-journald.service`
produced three persisted samples, `reason: deadline`, `complete: true`, and
`storageDurable: true`. Local smoke artifacts are under
`/data/scratch/tmp/opencode/amc-production-telemetry-20260920-01/`.
Collection-path evidence only: no gaming, stress, long-session endurance or
release-build overhead evidence. This slice changed no services or policies, ran
no new VM and preserved staged work.

Game-specific frame-time capture, calibrated thresholds and A/B/C results are
optional. The record covers later shared admission and the operational guide
separately. Ordinary `watch` defaults didn't change.

### Atlas Pilot Readiness (2026-09-20)

Read-only discovery on pilot host Atlas found running Steam
(`app-cosmic-steam-*.scope`) and Lutris scopes plus `gamemoded.service`, but no
confirmed game owner. Scope names embed PIDs; rediscover them each session.
MangoHud wasn't on `PATH`; frame-time logging remains unverified.

Validated on Atlas, no policy or service changes:

- `amc inspect` accepts `.scope` units and reports the Steam scope's cgroup,
  invocation, limits, and kernel observations.
- `amc watch SCOPE --production --seconds 3` completed (`reason: deadline`,
  `complete: true`, 3/3 samples, 0 missed) with full host context.
- Release binary `target/release/amc` (`amc 0.1.0`, sha256
  `05c94a2b68f27fb10032394a38f186411cda1551453b97aa41ce8b04d8b44520`)
  captured the Steam scope; per-sample read-path timing peaked near 198us
  (debug: ~1127us). `captureDurationUs` measures capture reads, not whole-observer
  overhead.
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
comparison candidate. Proposed scene: record a fixed 60–90 second route locally
and offline on the same training map, archive the demo and hash, and replay
the same file in each arm. No replay is selected or verified. The scene,
playback determinism and representative memory pressure still need calibration.
Don't measure a public network match.

The selected operational objective is **gaming-first stability with bounded
useful background progress**. In a comparison, frame times and stall counts
are scenario-specific supporting observations. Set thresholds and run count
separately; report useful work, failures and recovery alongside them. This
optional protocol doesn't gate usable setup.

MangoHud 0.8.4 is present in the Nix store but not on `PATH`. Its
[upstream configuration](https://github.com/flightlessmango/MangoHud/blob/v0.8.4/data/MangoHud.conf)
supports `output_folder`, `autostart_log`, `log_duration`, and `log_interval`.
Verify per-frame logging for this game and measure overhead with otherwise
identical launches before comparing. Keep logging configuration identical
across arms. Align the frame-time CSV with each AMC capture by run window;
on-screen FPS is no substitute.

Candidate background work: a fixed, clean AMC Cargo workspace build or test
batch with pinned source/lock/toolchain and a fresh, isolated target directory
per run. Predeclare the finite batch, verify memory competition on Atlas and
record completion, elapsed time, failures and admission waits. Match native
per-job and aggregate limits and offered work in B/C. C uses **one shared**
weighted runner; B uses calibrated fixed concurrency. The installed `amc run`
fixture command doesn't provide weighted admission. Tune work estimates and
comparison margins in separate calibration runs. Don't infer benefit from the
prior passive capture or treat frames as independent replicates.

Footprint probe (2026-09-30): one fresh-target
`cargo test --workspace --all-targets --locked -j 2` batch completed all 190
tests in a retained transient service. Native accounting reported a
1,134,256,128-byte peak (about 1.06 GiB), 39.34 CPU seconds, and 30.22 seconds
between main-process start and exit. The service had a 2 GiB max, zero swap,
200% CPU quota and 180-second runtime bound. Accounting export preceded service
stop. Concurrent host activity makes these exploratory timings, not a
comparison baseline. One small batch doesn't establish a representative Atlas
memory-admission challenge. Validate the offered multi-job mix before fixing
weights or useful-work margins.
Evidence: `/data/scratch/tmp/opencode/amc-cargo-calibration-20260930/`, including
the command, limits, complete log, accounting, and source patch (SHA-256
`cfc60dea2cffc6cbcbcaba56d52badcc5f8fd764774bb04fb39af564c91c1045`).

### Consumer-owned Fleetix binding pilot (2026-09-30)

Canix exposes a read-only Atlas binding in
`root/hosts/atlas/features/amc_binding.nix`. Fleetix supplies the declared
`atlas` host and `can` account, while Canix names `opencode.service`, its
`app-amc.slice` owner, and the `agent-tools.slice` domain. Memory limits come
from `canix.amcContracts`, not new Fleetix workload fields. AMC neither activates
resource policy nor decides memory budgets through this binding.

Guarded evaluation passed for `nixosConfigurations.atlas.config.canix.amcBinding`
and the Atlas toplevel derivation. Read-only live inspection found the backend
in `app-amc.slice` at 32 GiB/2 GiB max/swap, the tool slice at 24 GiB/8 GiB
aggregate, and one active tool service there at 16 GiB/4 GiB. These identity/boundary
checks cover the running generation, not weighted coordination or
activation of the new Nix binding.

After Canix configuration changes, repeat acceptance from its checkout on Atlas
with `canix` and `amc` on `PATH`:

```sh
canix repo eval --output json .#nixosConfigurations.atlas.config.canix.amcBinding
amc inspect opencode.service --json
systemctl --user show agent-tools.slice \
  --property=Id,LoadState,ActiveState,ControlGroup,MemoryMax,MemorySwapMax
# While a tool job is active, replace UNIT with its discovered service name:
systemctl --user list-units 'amc-opencode-tool-*.service' --state=running
amc inspect UNIT --json
```

Require active `opencode.service` with `Slice`, leaf cgroup and effective
`MemoryMax`/`MemorySwapMax` matching the declaration (32 GiB/2 GiB). Require active
`agent-tools.slice` with a cgroup ending in that slice and matching aggregate
limits (24 GiB/8 GiB). For a live tool service, require its `Slice` to match
`agent-tools.slice`, its cgroup beneath that slice, and its limits matching the
Canix per-job contract (16 GiB/4 GiB). Check AMC's `memory.max` and
`memory.swap.max` cgroup cells and manager properties; fail on unknown cells. Rediscover transient names each run.
A backend match doesn't verify the tool domain. Native placement doesn't prove
gaming responsiveness or coordinated admission.

The Canix binding asserts agreement with the resolved Home Manager backend
service and native tool slice. The consumer-owned `tests/amc-binding/check-live.py`
checks an evaluated declaration against manager/kernel limits for the backend,
aggregate slice and one live tool service, then rechecks identity. Its adjacent
README lists guarded evaluation and read-only test commands. Offline negative
regressions cover wrong placement, changed invocation, mismatched limits and
unknown cells. Guarded evaluation passed for the pure module regression:
matching resolved settings pass assertions; missing/mismatched service and
slice settings fail. Canix's existing `amc-tool-domains` check includes it.
Tracking the Home Manager module cleared the Git-backed source blocker. Later
guarded full-configuration evaluation failed on unrelated concurrent Canix
work: the pinned toolbelt lacks the newly referenced `gatus-instances` module.
The runtime checker still lacks a fresh declaration from this tree. The earlier
manual live result remains the last complete Atlas acceptance evidence.

### Atlas Endurance Run (started 2026-09-20 ~21:19 local)

The first backgrounded launch (`amc-atlas-endurance-01`, PID 2164183) exited
after 2 samples with `reason: cancelled`, `complete: false` and durable prefix
intact. The tool shell may have reaped its child when the command returned:
alive at +2s, SIGTERM-handled summary before +14s, no other operator action.
The partial directory was retained as aborted-run evidence, not endurance data.

The rerun put the observer under the user manager, not the tool shell, through a
transient user unit:
`systemd-run --user --unit=amc-endurance-01` running release `amc 0.1.0`
(invocation `8f2a3cd22f0f4019a669029740937ed6`),
`watch app-cosmic-steam-1439422.scope --production --seconds 1800`
into `/data/scratch/tmp/opencode/amc-atlas-endurance-02/`.
Early health (~15s): unit active/running, `ready` present, 15/15 samples,
~2.8KB/sample (~5MB projected, well under the 256MiB cap), observer ~4MB RSS.
Completed ~21:49 local: `reason: deadline`, `complete: true`, 1800/1800 samples,
0 missed intervals, contiguous sequences, single invocation, zero unknown host
cells. Full data-quality report: [atlas-endurance-01](atlas-endurance-01.md).
Reproduce validation with `python3 scripts/validate-capture.py <capture-dir>`:
it checks schema, contiguity, counts, bytes and identity and derives report
metrics (nearest-rank p99, rates over the sample span). Regressions:
`scripts/test-validate-capture.py`. The aborted run validates as structurally
sound but incomplete, as required. Only RSS, not observer CPU time, was measured;
capture timing covers reads, not total observer cost. Don't present aggregate
Steam scope telemetry as game-specific evidence.

### 1. Product Contract

- [x] Reconcile product positioning around independent admission/diagnostics and
  consumer-owned gaming-first policy.
- [x] Specify independent capabilities, enrollment, authority, permitted actions,
  failure behavior, and disable/uninstall behavior without a universal priority.
- [x] Use Linux/systemd/cgroup v2, Atlas and ordinary game launching for the
  first operational profile; consumers own workload/budget selection.

Acceptance: users can tell what AMC controls, what it cannot control, how policy
is selected, and how to stop using it. No speculative framework is required.
Dependencies: explicit consumer policy selection and documented native ownership.

### 2. Native Workstation Policy

- [x] Keep enrolled useful work running at reduced parent/leaf CPU/I/O weights,
  with finite memory/swap ceilings and bounded concurrency.
- [x] Give daemon builders their own native ceiling within an aggregate system
  background parent; retain independent maintenance deferral and recovery.
- [ ] Complete exact-package, resolved consumer and live-limit rollout checks
  for the selected integrated revision.

Acceptance: selected jobs run within effective native boundaries, excess work
queues or fails visibly, useful outputs are verified, and foreground exit or
interrupted hooks restore the intended state. Dependencies: slice 1 and the
consumer deployment workflow. Frame-time comparisons are optional.

### 3. Opt-In Gaming/Background Slice

- [x] Provide a standalone policy/enrollment path using
  existing admission and native enforcement where possible.
- [x] Test queued-work behavior, cancellation, restart, native deadlines after
  client/coordinator loss and subsequent progress with bounded native fixtures.
- [x] Document supported outcomes, overload, lifecycle and scope.
- [ ] Complete the consumer's foreground-hook/live policy exercise.

Acceptance: effective containment, visible waits/failures, useful outputs and
recovery. Dependencies: native capability availability and consumer policy;
there is no comparative-performance prerequisite or mandatory game wrapper.

### 4. Optional Shared Coordination

The optional implementation landed in `56bae9f` during this work. All ten native
ledger/socket/store tests passed in the combined workspace run. The independent
Ubuntu and Nix review-fix snapshot predates and doesn't verify the service.
See [the service's contract](persistent-admission.md) for interface and recovery
rules; keep the gates below.

- [x] Supply optional coordination for independently launched tools and servers;
  a single in-process runner remains independently usable.
- [x] Define authenticated ownership, bounded requests, reservation lifecycle,
  persistence/reconciliation, versioning, and unavailable-coordinator behavior.
- [x] Test concurrent clients, disconnects, crashes/restarts, detached work,
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
- [ ] Publish module-specific support, compatibility, operational acceptance,
  failure/rollback procedures, and known limitations.

Acceptance: reproducible user-facing setup and recovery, explicit support bounds,
and no blanket readiness claim for unvalidated modules. Dependencies: an accepted
initial slice and module-specific safety evidence. Other workload families remain
future validation targets, not demonstrated capabilities.

## Verification And Release Gates

For code changes, keep workspace tests, clippy, formatting, runner feature-matrix
tests/docs and fixture-script checks. Use exact commands from the current
flake/README and the pinned environment. Run cheap checks first; run
resource-budgeted VM mechanism tests separately.

For authorized local flake realization, use the approved cache workflow,
`canix cache binary build .#nixosTests.x86_64-linux.generic --include-tests`, with private publication
enabled and limits sized for the host. Cached outputs don't prove fresh
execution. Record command, source/dirty-tree identity, versions, configuration,
artifact locations and verification outcomes for each claim.

Release gates apply to the selected operational objective:

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

Resolved since planning: Atlas selected as pilot host, Counter-Strike 2 as first
candidate, gaming-first objective, implemented telemetry and scope prerequisites,
and a passed booted diagnostic walkthrough on Ubuntu 24.04.5. The fixed Cargo
batch is provisional pending memory-demand profiling.

Next: integrate the chosen AMC revision, finish exact package/consumer checks,
verify effective host limits, exercise foreground entry/exit with intended
useful work, and complete that revision's operational lifecycle checks. Attribute
source implementation, tested modules and active deployment to their own
revisions. Replay, per-frame logging, numerical margins and a comparison
schedule can follow as optional diagnostics without changing the delivery
contract. Source installation smoke doesn't establish every module's support on
every distribution. No agreed launch date, universal threshold, public IPC API
or daemon mandate.

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
