# Historical Corrective And Direnv Results

For current execution status, see `../proof-results.md`. This record is historical.

## Direnv Follow-up: 2026-09-08

The user authorized repairing direnv instead of using `nix develop`. Without a
local `.envrc`, direnv loaded the parent canix configuration. The new file loads
AMC's existing pinned flake, disables stale fallback, and watches its
Cargo/package inputs. `direnv allow .` authorized it. The repair changed neither
parent nor global direnv configuration. The second load reused the cache.

Record: `proof-artifacts/direnv-20260908T061344Z/checks.json`.

| Command | Actual result |
|---|---|
| `direnv exec . cargo test --all-targets --locked -j 2` | 18 unit + 4 isolated CLI tests passed; zero failures |
| `direnv exec . python3 scripts/check-fixtures.py` | Passed property-scope, explicit-report and unknown-telemetry regressions |
| `direnv exec . cargo fmt --all --check` | Initially failed on pending formatting; passed after `direnv exec . cargo fmt --all` |
| `direnv exec . cargo clippy --all-targets --locked -j 2 -- -D warnings` | Passed |
| `git diff --check` | Passed |

The shell provides the pinned compiler, linker, formatter, Clippy and Python
(reported rustc 1.97.1, cargo 1.97.0). Lockfiles remain unchanged. These isolated
regression results supersede the environment blocker below but do not
demonstrate live manager behavior or OOM handling. The generic VM, pressure
comparisons and OpenCode trial still have not run.

## Initial Milestone Record

Run record: `proof-artifacts/corrective-20260908T054814Z/checks.json` (ignored,
run-specific, normalized from actual tool outcomes). This run did not activate
production configuration, restart services, run host doctor or stress probes,
make paid requests, or change broker policy.

This replaces the **RFC 0.4 completion claim** and leaves the historical v0.1
report at `docs/proof-results.md` unchanged. The 12 previously passing tests
do not validate the current patch. The old ledger incorrectly reported
cancellation, strict arbitrary pre-exec verification, redaction, pressure
comparisons and overhead measurement as implemented.

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

The first targeted VM evaluation failed because Git-backed flake evaluation
could not see `nix/vm-test.py`. Intent-to-add entries exposed the new source
files without staging existing user edits. The initial generated unit lookup
failed on an unquoted dotted attribute; the quoted lookup above succeeded.
No denied build was retried under another spelling.

No-build flake checks still warn about app metadata and the custom `nixosTests`
output. These are not runtime test failures. Lockfiles did not change.

The pinned evaluation uses nixpkgs `3ed67ec0a4d3c7ab4ae1f04f8ee8df07bfa506a2`,
guest systemd **261.2** and kernel **6.18.48**. Earlier host observations of
systemd 261/261.1 and another kernel do not describe the pinned VM. Byte/GiB
corrections are in `nixos-opencode.md`; historical peaks do not measure working
sets.

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

When permitted, run the cheap README checks first. Run this VM command only
after they pass:

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

The VM exports `oom-watch/` (bounded timestamped snapshots, last readable values,
event deltas, explicit unknowns), `work-{a,b,c}.json` and
`precedence-{one,two}.json` into its test-driver output. These are **planned
output names; this milestone produced no VM outputs**. The test rejects generic
signal results as OOM evidence; it requires a positive OOM-kill counter delta
and manager `Result=oom-kill`.

## Stop Condition

The direnv repair clears the cheap executable gates, not the mechanism VM or
release validation. At this milestone, the mechanism VM was the next unexecuted
gate, before any isolated OpenCode trial. If it shows native configuration
suffices, keep that application solution. Do not expand AMC's launch role to
compensate for missing evidence.
