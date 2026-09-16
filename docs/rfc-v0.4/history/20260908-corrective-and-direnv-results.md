# Historical Corrective And Direnv Results

Historical record only. Current execution status is in `../proof-results.md`.

## Direnv Follow-up: 2026-09-08

The user authorized repairing direnv instead of using `nix develop`. AMC had
no local `.envrc`, so direnv found the parent canix configuration. The new local
file loads AMC's existing pinned flake, disables stale fallback, and watches its
Cargo/package inputs. It was authorized with `direnv allow .`; the parent and
global direnv configuration were not changed. A second load reused the cache.

Record: `proof-artifacts/direnv-20260908T061344Z/checks.json`.

| Command | Actual result |
|---|---|
| `direnv exec . cargo test --all-targets --locked -j 2` | 18 unit + 4 isolated CLI tests passed; zero failures |
| `direnv exec . python3 scripts/check-fixtures.py` | Passed property-scope, explicit-report and unknown-telemetry regressions |
| `direnv exec . cargo fmt --all --check` | Initially failed on pending formatting; passed after `direnv exec . cargo fmt --all` |
| `direnv exec . cargo clippy --all-targets --locked -j 2 -- -D warnings` | Passed |
| `git diff --check` | Passed |

The shell provides the pinned compiler, linker, formatter, Clippy and Python.
Reported tool versions: rustc 1.97.1, cargo 1.97.0. Lockfiles remain unchanged.
These outcomes supersede the executable-environment blocker recorded below.
They demonstrate isolated regressions, not live manager behavior or OOM handling.
The generic VM, pressure comparisons and OpenCode trial still have not run.

## Initial Milestone Record

Run record: `proof-artifacts/corrective-20260908T054814Z/checks.json` (ignored
run-specific artifact, normalized from actual tool outcomes). No production
configuration was activated, no service restarted, no host doctor or stress
probe run, no paid request made, and no broker policy changed.

This replaces the **RFC 0.4 completion claim**, not the historical v0.1 report
at `docs/proof-results.md`, which remains unchanged. The previously reported
12 passing tests do not validate the current patch. The old ledger's claims of
implemented cancellation, strict arbitrary pre-exec verification, redaction,
pressure comparisons and overhead measurement were incorrect.

## Initial Executed Checks

| Exact command | Actual outcome |
|---|---|
| `nix develop --no-write-lock-file -c cargo test --all-targets --locked -j 2` | Blocked by tool policy before execution; no tests ran |
| `bash -n scripts/prove-local.sh` | Exit 0 |
| `git diff --check` | Exit 0 |
| `bash -c 'bash scripts/prove-local.sh --host-stress; status=$?; test "$status" -eq 2'` | Exit 0: confirmed stress option is rejected with exit 2, before any probe |
| `nix eval --no-write-lock-file --raw .#nixosTests.x86_64-linux.generic.drvPath` | Exit 0 after adding new referenced files with Git intent-to-add; evaluation only |
| `nix eval --no-write-lock-file --raw .#checks.x86_64-linux.fixture-scripts.drvPath` | Exit 0; scripts not executed |
| `nix flake check --no-build --no-write-lock-file --all-systems` | Exit 0; evaluation only, not test/build execution |
| `nix eval --json --no-write-lock-file '.#nixosTests.x86_64-linux.generic.nodes.machine.systemd.user.units."amc-native.service".text' --apply 'text: assert text == "[Service]\nMemoryHigh=192M\nMemoryMax=256M\n"; true'` | Exit 0, `true`: generated fragment contains only the selected settings |

The first targeted VM evaluation failed because `nix/vm-test.py` was not yet
visible to Git-backed flake evaluation. Intent-to-add entries for the new source
files resolved that without staging existing user edits. An initial generated
unit lookup used an unquoted dotted attribute and failed; the correctly quoted
lookup above succeeded. No denied build was retried through another spelling.

No-build flake checks retain existing warnings for app metadata and the custom
`nixosTests` output. These are not runtime test failures. No lockfile was changed.

Pinned evaluation uses nixpkgs
`3ed67ec0a4d3c7ab4ae1f04f8ee8df07bfa506a2`, guest systemd **261.2** and kernel
**6.18.48**. Earlier host observations of systemd 261/261.1 and another kernel
are not the pinned VM versions. Byte/GiB corrections are documented in
`nixos-opencode.md`; historical peaks are not working-set measurements.

## Initial Gates And Evidence

| Area | Status | Evidence/limitation |
|---|---|---|
| VM assertion and report-file repairs | Implemented | Cheap structural/behavioral regressions added; execution blocked |
| Safe bounded default diagnostics | Implemented | Raw journal path deleted, shared bounded capture and sensitive-marker regressions added; not yet executable-tested |
| Launch ownership and cancellation | Implemented | One identity, bounded queries/cleanup, isolated rejection/lost-reply/signals tests; not demonstrated |
| Strong arbitrary pre-exec fail-closed guarantee | Unsupported | Helper-entry observation only; no new supervisor |
| Ancestry and disappearing telemetry | Implemented | Typed unknowns, timestamps, readiness/finite observer; not demonstrated |
| Native NixOS fragment generation | Executed and demonstrated narrowly | Exact evaluated text has no generated PATH/Environment or lifecycle overrides |
| Runtime native precedence and lifecycle | Implemented, blocked | Conflicting values and higher-priority user main-unit tests have not run |
| Home Manager module evaluation | Blocked | No pinned Home Manager input here; plain symlink test does not validate its modules |
| Formatting, Rust tests, Clippy, Python regressions | Blocked | Approved pinned `nix develop` execution denied; no unrelated toolchain repair attempted |
| Generic VM mechanisms | Blocked, not executed | Cheap executable gates have not passed; VM target evaluation is not a VM pass |
| A/B/C pressure, high crossing, aggregate parent pressure, overhead | Deferred | Small tests are labeled smoke tests; invalid benchmark claims withdrawn |
| Native OpenCode activation, responsiveness, useful work | Deferred / not measured | Live backend untouched; no claim that freezes are fixed |

When permitted, execute the cheap commands from README first. Only after they
pass, run the explicit VM command:

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

The VM exports `oom-watch/` (bounded timestamped snapshots, last readable values,
event deltas, explicit unknowns), `work-{a,b,c}.json` and
`precedence-{one,two}.json` into its test-driver output. Those are **future
artifact names, not files claimed to exist from an executed VM**. No generic
signal result is accepted as OOM evidence; the test requires a positive OOM-kill
counter delta and manager `Result=oom-kill`.

## Stop Condition

The direnv repair clears the cheap executable gates; it does not demonstrate the
mechanism VM or constitute a validated release. The mechanism VM is the next
unexecuted gate, before any isolated OpenCode trial. If it establishes that native
configuration suffices, retain that application solution. Do not expand AMC's
launch role to compensate for missing evidence.
