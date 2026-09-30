# Workstation harness verification — 2026-09-30

This records mechanism verification, not a CS2 result. The operator guide is
[gaming-comparison.md](gaming-comparison.md).

## Source and native checks

- `daedc20`: waited-unit collection reconciliation with pinned workload/parent
  directory identity; replacement, invisible parent and unknown population tests.
- `13c482b`: bounded B/C background driver and native smoke, plus example source
  inclusion in packaged Cargo checks.
- `266900a`: raw MangoHud frame-window report and offline regressions.
- Workspace/all-target tests passed **205 tests** on the combined tree (including
  the concurrent boot-identity hardening). Strict workspace/all-target clippy
  passed with `-D warnings`.
- Runner tests, strict all-target clippy and warning-free documentation passed
  for default, none, sync, async, sysinfo and systemd features.
- `scripts/check-fixtures.py` passed, including capture validation, observer
  lifecycle and the new frame-report regressions. Treefmt and diff checks passed.
- The final live admission fixture passed native limits, streams/status, shared
  admission, SIGKILL recovery, manager loss, client death, cancellation/entry
  ownership and post-OOM progress. This is separate from the B/C mechanism smoke.

Commands used `direnv exec .` and cleared inherited `CARGO_ENCODED_RUSTFLAGS`
for Cargo. No global toolchain settings were changed.

## Native B/C mechanism

The release driver passed `tests/workstation-batch-systemd.py` on Atlas's
existing `agent-tools.slice`. Three small finite hash jobs were offered with
two workers; C used one shared 64 MiB admission budget with 64 MiB grants.
Receipts showed nonoverlapping C work, all useful outputs verified, confirmed
native completion and zero final reserved bytes. B completed the same work.
Negative verification retained failed artifacts and counted zero useful work;
aggregate-limit mismatch and a FIFO manifest were rejected before work.

An initial attempt failed closed when systemd collected successful transient
units before the runner's final query. Its valid receipts did not cause capacity
release. The retained failed attempt is `batch-native/`; the corrected runs are
`batch-native-02/` and `batch-native-03/` beneath
`/data/scratch/tmp/opencode/amc-onwards-20260930/`. The small hash fixture is not
a calibrated memory-pressure workload, and its timings are not a benefit claim.

## Packaging and consumer gaps

A packaged CLI build passed before the new example was included in the
Git-backed flake snapshot, at
`/nix/store/gyd3apqbzj3wvc1198c29nqcgpanm49x-amc-0.1.0`.
That is not full post-harness check evidence. Attempts to evaluate the exact
post-harness package, Cargo checks, fixture check and admission VM were blocked
by a concurrent Canix evaluation, including a 300-second guarded wait.
The failed requests and source patches are retained in `nix-gates/` and
`nix-gates-02/` under the directory above. The final packaged/VM gates remain
outstanding.

Canix's standalone Python binding regressions passed and `canix config check
amc-policy` reported `current`. The `amc-tool-domains` build now invokes those
negative regressions. A full Atlas declaration evaluation failed on concurrent
Canix's missing `gatus-instances` toolbelt export. Fresh declaration/live/variant
and host-build acceptance remain outstanding. The consumer's evolving pin must
be recorded separately; these local runner/harness commits were not pushed.

The first fresh-context review identified parent-visibility, descriptor pinning
and replacement risks and informed the implementation. Follow-up reviewer calls
were unavailable (provider/tool errors); they supply no additional review evidence.

No CS2 replay, logger injection/overhead, representative useful-work mix,
calibration-derived numerical protocol or A/B/C gaming schedule has been verified.
Those prerequisites apply to an optional comparative experiment, not delivery
of the [operational workstation profile](workstation-policy.md).
