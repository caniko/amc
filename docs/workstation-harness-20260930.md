# Workstation harness verification — 2026-09-30

Harness verification only, not a CS2 result. See the
[operator guide](gaming-comparison.md).

## Source and native checks

- `daedc20`: waited-unit collection reconciliation with pinned workload/parent
  directory identity; replacement, invisible parent and unknown population tests.
- `13c482b`: bounded B/C background driver and native smoke, plus inclusion of
  the example source in packaged Cargo checks.
- `266900a`: raw MangoHud frame-window report and offline regressions.
- Workspace/all-target: **205 tests** passed on the combined tree, including
  concurrent boot-identity hardening. Strict workspace/all-target clippy
  (`-D warnings`) passed.
- Default, none, sync, async, sysinfo and systemd features passed runner tests,
  strict all-target clippy and warning-free documentation checks.
- `scripts/check-fixtures.py` passed, including capture validation, observer
  lifecycle and the new frame-report regressions. Treefmt and diff checks passed.
- The final live admission fixture passed native limits, streams/status, shared
  admission, SIGKILL recovery, manager loss, client death, cancellation/entry
  ownership and post-OOM progress. This is separate from the B/C mechanism smoke.

Commands used `direnv exec .` and cleared inherited `CARGO_ENCODED_RUSTFLAGS`
for Cargo without changing global toolchain settings.

## Native B/C mechanism

The release driver passed `tests/workstation-batch-systemd.py` on Atlas's
existing `agent-tools.slice`. The test offered three small finite hash jobs to
two workers; C used one shared 64 MiB admission budget with 64 MiB grants.
Receipts showed nonoverlapping C work, all useful outputs verified, confirmed
native completion and zero final reserved bytes. B completed the same work. Negative
verification retained failed artifacts and counted zero useful work. The driver
rejected an aggregate-limit mismatch and a FIFO manifest before work.

An initial attempt failed closed when systemd collected successful transient
units before the runner's final query. Valid receipts did not release capacity.
The retained failure is `batch-native/`; corrected runs are
`batch-native-02/` and `batch-native-03/` beneath
`/data/scratch/tmp/opencode/amc-onwards-20260930/`. The small hash fixture isn't
a calibrated memory-pressure workload; its timings don't demonstrate a benefit.

## Packaging and consumer gaps

A packaged CLI build passed before the Git-backed flake snapshot included the
new example. That CLI build's output was
`/nix/store/gyd3apqbzj3wvc1198c29nqcgpanm49x-amc-0.1.0`.
That build doesn't qualify the full post-harness snapshot. A concurrent Canix
evaluation blocked attempts to evaluate the exact post-harness package, Cargo
checks, fixture check and admission VM, even with a 300-second guarded wait.
Failed requests and source patches: `nix-gates/` and `nix-gates-02/` under the
directory above. Final packaged/VM gates remain open.

Canix's standalone Python binding regressions passed;
`canix config check amc-policy` reported `current`. The `amc-tool-domains` build
runs those negative regressions. Full Atlas declaration evaluation failed
because concurrent Canix work referenced a missing `gatus-instances` toolbelt
export. Fresh declaration/live/variant and host-build gates remain open.
Record the evolving consumer pin separately; local runner/harness commits
were not pushed.

The first fresh-context review flagged parent-visibility, descriptor pinning
and replacement risks, informing the implementation. Follow-up reviewer calls
were unavailable due to provider/tool errors and supplied no evidence.

Still unverified: CS2 replay, logger injection/overhead, a representative
useful-work mix, a calibration-derived numerical protocol and an A/B/C gaming
schedule. These gate the optional comparison, not the
[operational workstation profile](workstation-policy.md).
