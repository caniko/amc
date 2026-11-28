# AMC

AMC provides embeddable memory admission (`amc-runner`) and passive workstation
telemetry (`amc-telemetry` and the CLI). The CLI includes local
fixture/compatibility launch tools for systemd memory policy. RFC 0.4 in
`docs/rfc-v0.4/` governs native integration. AMC is not a policy standard,
sandbox or transparent replacement for normal launching.

- The optional [persistent per-user admission service](docs/persistent-admission.md)
  coordinates native managed jobs across independent processes with durable
  recovery. The [shared host broker](docs/shared-host-admission.md) adds advance
  foreground preparation, finite completion rights for existing work, and
  bounded swap-return recovery before new memory growth.
- The optional [native memory supervisor](docs/supervision.md) adds per-invocation
  whole-window forecasting, admission inhibition and identity-bound recovery
  for explicitly authorized backends or disposable jobs. Consumers choose shadow
  or enforcement mode and keep a separate forecast-recovery rollout gate.
- The [workstation guide](docs/workstation-policy.md) covers standalone native
  setup, enrollment, deadlines and recovery for stable, usable foreground gaming
  with background jobs that stay bounded and make useful progress. Consumers own
  the policy; observation alone doesn't enable it. The
  [adoption record](docs/workstation-adoption-plan.md) tracks implementation and
  rollout. The optional [CS2 guide](docs/gaming-comparison.md) covers demo
  recording, per-present logging and matched background-work diagnostics.

See the [full documentation index](docs/README.md).

## Install and first report

The initial inspection/capture target is Linux with systemd and cgroup v2.
From a checkout, install the CLI with Nix (`nix profile install .#default`)
or Rust (`cargo install --path . --locked`). `amc report` runs offline without
systemd or access to the observed machine. It needs Python 3.11 or newer on
`PATH`; the Nix package includes it. Source installation and live inspect →
capture → report passed in a booted Ubuntu 24.04.5 guest with systemd 255 and
cgroup v2, including cancelled captures and unavailable measurements.
The [Ubuntu validation record](docs/ubuntu-diagnostics-20260930.md) has the exact
environment, reproduction command, and artifacts.

```sh
# Identify the actual service/scope that owns your workload first.
systemctl --user list-units --type=service,scope --state=running --no-pager
systemctl --user show actual-workload.service --property=ControlGroup --property=InvocationID

# Replace actual-workload.service with the unit you found. Capture is passive.
amc inspect actual-workload.service
amc watch actual-workload.service --production --seconds 60 --output ./amc-session-01
amc report ./amc-session-01
amc report ./amc-session-01 --json
```

Use a new output directory per capture. Before deriving available metrics, the
report checks capture identity, stored sample counts and completion evidence.
`complete` describes collection, not proof of full workload-lifetime coverage.
Unknown/partial measurements stay visible. See [telemetry](docs/telemetry.md)
for interpretation and [admission](docs/admission-contract.md) for cooperative workloads.

**Real applications stay with their existing native lifecycle owner.** For
OpenCode, discovery found the shared backend `opencode.service`; wrapping an
activation client doesn't configure it. See
[native integration](docs/rfc-v0.4/nixos-opencode.md).
You can still contain their *tool commands* per call: the flake's
`lib.mkToolShell pkgs { slice; memoryMaxBytes; memorySwapMaxBytes; }` builds a
shell for the agent's shell setting that runs each `-c` command through
`amc exec` (or `amc admission exec` with `admission = true`), and
`lib.mkServiceWrap` admits a launched backend such as `codex app-server`.
See [Nix execution policy](docs/nix-execution-policy.md) for `lib.executionArgs`,
`lib.mkNixBuilderPackage` and `lib.nixBuilderSettings`.

## Diagnostics

```sh
amc inspect opencode.service --json
amc inspect some-system-unit.service --system --json
amc watch some-unit.service --seconds 10 --output /tmp/amc-watch-out
amc watch some-unit.service --production --output /tmp/amc-production-out
amc diff /tmp/amc-watch-out/before.json /tmp/amc-watch-out/after.json
amc report /tmp/amc-production-out --json
./scripts/prove-local.sh opencode.service
```

Inspection queries allowlisted manager metadata and samples the leaf and visible
cgroup ancestry without starting services or changing policy. Each snapshot has
a timestamp; unavailable/malformed measurements carry null and a reason, never
an invented zero. The visible mount root marks a namespace boundary; hidden
ancestors may exist. Each manager query gets up to 2 seconds and 64 KiB.
Ancestry has a disclosed 64-cgroup limit and a 10-second manager-query budget
(one in-flight query can add 2 seconds).

`amc watch` observes one unit for a finite interval in read-only mode. It never
starts, stops or reconfigures the target. It writes bounded `samples.jsonl`
and an atomic `summary.json` with explicit coverage flags. Completion requires evidence; counter deltas
require verified lifetime continuity, not just matching paths. `amc diff`
compares two observation files offline and explains unsupported comparisons
per field. See [telemetry](docs/telemetry.md) for
the observation model, identity rules, and remaining limits.

`--production` selects passive, lower-frequency capture: 30 minutes at 1 Hz
by default, with host memory/swap and CPU/IO/memory pressure context. It supports
up to 24 hours with 1-60 second sampling and caps each observer at 86,400 samples
and 256 MiB of sample data. It reports missing data and early stops and never
starts, stops, moves or reconfigures services. The flag selects a collection mode,
not a production-readiness claim. See the [production capture guide](docs/telemetry.md#production-capture).

The optional [accounted capture runner](docs/telemetry.md#accounted-observation)
accounts for observer CPU, peak memory and block-IO. It starts a transient user
observer, exports retained final counters, then stops only that observer without
changing target settings.

Default text/JSON diagnostics omit application journal messages, `ExecStart`,
`Environment`, and arbitrary subprocess error streams; this isn't secret scrubbing.
**`amc run` forwards raw workload output through a separate
interface**; don't publish it as a sanitized diagnostic report.

`amc doctor` is **not passive**: it creates and cleans up a short-lived transient
capability probe with fixture settings. Run it only deliberately; the local
script doesn't run it automatically. `amc doctor` rejects `--host-stress`.

## Fixture Compatibility

```sh
amc --config examples/config.toml explain --id amc.proof --json
amc --config examples/config.toml run --id amc.proof --unit-file /private/attempt.unit -- command arg
```

The private TOML interface stays at version 1 and searches in this order:
explicit `--config`, XDG config, HOME config, then `/etc/xdg/amc/config.toml`.
Only the fixture tools use it; it doesn't configure native application services.
Use observation to establish effective values: submitted properties can conflict
with native drop-ins. There is no custom policy merge engine.

Fixtures request `OOMPolicy=kill`, `Restart=no`, memory accounting and max/swap
limits. They request `MemoryHigh` only when supplied and emit no `KillMode`
override. Fixture byte parsing doesn't validate every legal native systemd value. Fixture
numbers aren't production defaults; max-only tests don't advise against high.

The existing `systemd-run` service backend preserves literal argv,
`--expand-environment=no`, cwd, environment-name allowlisting, and PTY/pipe
selection. No scope or direct-D-Bus backend is added.

Each attempt gets one generated unit identity, which `--unit-file` publishes
privately before submission. Spawn failure differs from rejected or ambiguous
submission. Startup control is bounded; after an observed start, `amc run` has
no AMC runtime deadline. `--runtime-max-sec` sets a separate manager-enforced
runtime bound for a disposable fixture. Detached launches keep running after
successful acknowledgment.

SIGINT/SIGTERM during submission triggers bounded cleanup of only that attempt.
An unavailable manager or an unobserved collected unit yields UNKNOWN. A waited
unit observed at startup can complete after native collection only if a successful
final manager query and pinned workload-domain evidence establish termination
beneath the original visible parent. AMC doesn't retry, fall back to unrestricted
execution, or promise cleanup after SIGKILL or manager unavailability. Inspect
the recorded identity before retrying manually; work may already have happened.

Waited `amc exec` and `amc admission exec` synchronize before payload execution.
An exec-only helper waits on a one-attempt Unix socket until the runner pins
the native invocation and cgroup. Both peers verify PID/UID; the runner also
checks the helper's MainPID and native placement. Exchange failure, disconnect,
cancellation or a 30-second startup timeout prevents payload execution. Exec
preserves PID, argv and streams; admitted entry keeps all broker checks. The
native runtime ceiling covers helper and payload. Acknowledgment is startup
evidence only; completion requires an independent final manager query and an empty pinned
domain. Detached and library callers keep their existing acknowledgment rules.

Helper self-checks observe settings at entry, before allocation. They do **not**
prove arbitrary loader/constructor ordering or fail-closed launch under every
controller failure; that stronger strict-launch guarantee remains unsupported.
Systemd still owns execution and placement.

`--retain-unit` does not retain kernel counters. Collect evidence while the
cgroup exists; open file descriptors don't guarantee indefinite retention either.
Limits are accounting/reclaim controls, not preallocated RAM, an exact
instantaneous physical ceiling, or a fixed-time recovery promise.

## Checks And Evidence

Use the project-local direnv environment pinned by `flake.lock`. Review `.envrc`
before authorizing it once with `direnv allow .`. It deliberately avoids the
parent canix environment and never falls back to a stale shell. The first load
may fetch the pinned toolchain. `direnv exec` works in noninteractive agent sessions:

```sh
direnv allow .
direnv exec . cargo fmt --all --check
direnv exec . cargo test --all-targets --locked -j 2
direnv exec . cargo clippy --all-targets --locked -j 2 -- -D warnings
direnv exec . python3 scripts/check-fixtures.py
nix flake check --no-build --no-write-lock-file --all-systems
```

VM tests remain outside ordinary checks. Only after cheap gates pass:

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

The VM target runs mechanism tests and small A/B/C **smoke tests**, not a
validated pressure benchmark. It exports observer snapshots and explicit work
report files. Pressure/high-max/aggregate comparisons and OpenCode UX trials
remain deferred until mechanism gates pass. See the
[corrective results](docs/rfc-v0.4/proof-results.md) and
[checklist](docs/rfc-v0.4/migration-checklist.md).

The optional NixOS AMC module installs the tool and private compatibility
configuration; its slices are fixture-specific. Nix/Lix daemon builders,
containers, remote work and independently launched servers run in separate
execution domains. `nixBuildPool` stays disabled and blocked; this milestone
neither caps nor restarts a broker or production backend.
