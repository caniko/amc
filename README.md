# AMC

AMC provides embeddable memory admission (`amc-runner`) and passive workstation
telemetry (`amc-telemetry` and the CLI). The CLI also contains local
fixture/compatibility launch tools for systemd memory policy. RFC 0.4 in
`docs/rfc-v0.4/` governs native integration. AMC is not a policy standard,
resident policy daemon, sandbox, or transparent replacement for normal launching.

The [workstation adoption plan](docs/workstation-adoption-plan.md) records the
modular product direction, explicit user-selected priorities, and gaming plus
background work as the first evaluation target. General-adoption and performance
claims remain gated on that evaluation; observation does not enable a policy.

## Install and first report

Linux with systemd and cgroup v2 is the initial inspection/capture target.
From a checkout, install the CLI with Nix (`nix profile install .#default`)
or Rust (`cargo install --path . --locked`). `amc report` additionally needs
Python 3.11 or newer on `PATH`; the Nix package includes it. Reporting is
offline and needs neither systemd nor access to the observed machine.
Source installation and offline reporting have also been checked in an Ubuntu
24.04 container; live `inspect`/`watch` on Ubuntu still needs a booted systemd
and cgroup v2 validation host.

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

Choose a new output directory for every capture. The report checks the
capture's identity, stored sample counts, and completion evidence before
deriving available metrics. `complete` describes collection, not proof that
the entire workload lifetime was observed. Unknown/partial measurements stay
visible. See [telemetry](docs/telemetry.md) for interpretation and
[admission](docs/admission-contract.md) for cooperative workloads.

**Real applications stay with their existing native lifecycle owner.** For
OpenCode, the discovered shared backend is `opencode.service`; wrapping an
activation client does not configure that backend. See
[native integration](docs/rfc-v0.4/nixos-opencode.md).

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

Inspection is passive: it does not start services or change policy. It queries
allowlisted manager metadata and samples the leaf and visible cgroup ancestry.
Each snapshot has a timestamp; unavailable/malformed measurements have a null
value and a reason, never an invented zero. The visible mount root is a
namespace boundary, not proof that no hidden ancestor exists. Manager queries
are bounded to 2 seconds and 64 KiB each; ancestry has a disclosed 64-cgroup
limit and a 10-second manager-query budget (one in-flight query can add 2 seconds).

`amc watch` observes one unit for a finite interval (read-only; never starts,
stops, or reconfigures the target) and writes bounded `samples.jsonl` plus an
atomic `summary.json` with explicit coverage flags — completion is earned, and
counter deltas require verified lifetime continuity, never matching paths
alone. `amc diff` compares two observation files offline with per-field
reasons for unsupported comparisons. See [telemetry](docs/telemetry.md) for
the observation model, identity rules, and remaining limits.

`--production` selects passive, lower-frequency capture: 30 minutes at 1 Hz by
default, with host memory/swap and CPU/IO/memory pressure context. It permits up
to 24 hours with 1-60 second sampling, capped at 86,400 samples and 256 MiB of
sample data per observer. Missing data and early stops remain explicit; the
flag is a collection mode, not a production-readiness claim. No service is
started, stopped, moved, or reconfigured. See the [production capture guide](docs/telemetry.md#production-capture).

For observer CPU, peak-memory, and block-IO accounting, use the optional
[accounted capture runner](docs/telemetry.md#accounted-observation). It starts its
own transient user observer, exports retained final counters, then stops that
observer only; target settings remain unchanged.

Default text/JSON diagnostics omit application journal messages, `ExecStart`,
`Environment`, and arbitrary subprocess error streams. This is omission, not
a general-purpose secret scrubber. **`amc run` forwards workload output as a
separate raw interface**; do not publish it as a sanitized diagnostic report.

`amc doctor` is **not passive**: it creates and cleans up a short-lived transient
capability probe with fixture settings. Run it only deliberately; the local
script does not run it automatically. `--host-stress` is explicitly rejected.

## Fixture Compatibility

```sh
amc --config examples/config.toml explain --id amc.proof --json
amc --config examples/config.toml run --id amc.proof --unit-file /private/attempt.unit -- command arg
```

The private TOML interface retains version 1 and its existing search order:
explicit `--config`, XDG config, HOME config, then `/etc/xdg/amc/config.toml`.
It is used only when these fixture tools are invoked. It does not configure
native application services. Requested values are not effective values until
observed; submitted properties and native drop-ins can conflict. No custom
policy merge engine is provided.

Fixtures request `OOMPolicy=kill`, `Restart=no`, memory accounting, max/swap
limits and optional `MemoryHigh`. They do not emit a `KillMode` override.
An absent optional high value is not emitted. Fixture byte parsing is not a
validator for every legal native systemd value. Fixture numbers are not
production defaults, and max-only tests are not recommendations against high.

The existing `systemd-run` service backend preserves literal argv,
`--expand-environment=no`, cwd, environment-name allowlisting, and PTY/pipe
selection. No scope or direct-D-Bus backend is added.

Each attempt has one generated unit identity. `--unit-file` publishes it
privately before submission. Spawn failure is distinct from rejected or
ambiguous submission. Startup control is bounded; after an observed start,
`amc run` has no AMC runtime deadline. `--runtime-max-sec` explicitly gives a
disposable fixture a separate manager-enforced runtime bound. Detached launches
remain running after successful acknowledgment.

SIGINT/SIGTERM during submission triggers bounded cleanup of only that attempt.
An absent/collected unit or unavailable manager leaves an explicit UNKNOWN
outcome. There is no retry, unrestricted fallback, or promise of cleanup after
SIGKILL or manager unavailability. Inspect the recorded identity before any
manual retry; work may already have happened.

Helper self-checks observe settings at helper entry, before allocation. They
do **not** prove arbitrary loader/constructor ordering or fail-closed launch
under every controller failure. That stronger strict-launch guarantee remains
unsupported. Systemd continues to own execution and placement.

`--retain-unit` does not retain kernel counters. Collect evidence while the
cgroup exists; holding file descriptors also provides no indefinite retention
guarantee. Limits are accounting/reclaim controls, not preallocated RAM, an exact
instantaneous physical ceiling, or a fixed-time recovery promise.

## Checks And Evidence

Use the project-local direnv environment pinned by `flake.lock`. Review `.envrc`
and authorize it once with `direnv allow .`; it deliberately does not source the
parent canix environment or fall back to a stale shell. The first load may fetch
the pinned toolchain. `direnv exec` also works in noninteractive agent sessions:

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

The VM target contains mechanism tests and small A/B/C **smoke tests**, not a
validated pressure benchmark. It exports observer snapshots and explicit work
report files. Pressure/high-max/aggregate comparisons and OpenCode UX trials are
deferred until mechanism gates actually pass. See the
[corrective results](docs/rfc-v0.4/proof-results.md) and
[checklist](docs/rfc-v0.4/migration-checklist.md).

The optional NixOS AMC module installs the tool and private compatibility
configuration. Its slices remain fixture-specific. Nix/Lix daemon builders,
containers, remote work and independently launched servers are separate execution
domains. `nixBuildPool` remains disabled and blocked; this milestone neither caps
nor restarts a broker or production backend.
