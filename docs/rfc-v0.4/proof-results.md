# Mechanism Validation Results

**Verdict: MECHANISM GATE PASSED 2026-09-11 (disposable VM evidence below).**
Earlier attempts failed/blocked on permissions and unrealized fixture defects.
This result supersedes them for the mechanism claim only.

Attempt: `proof-artifacts/mechanism-20260908T092416Z/`.
Tool policy denied the exact requested build **before execution**:

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

No process exit status, VM execution, observed guest version, realized test
output, cache result reuse, or driver log exists for this attempt. The attempt
did not bypass the denial via direnv, scripts, another executable path, or
permission changes. The generic test remains outside ordinary flake checks.

## Changes In This Attempt

- Finalization covers boot and test assertions. The harness registers identity
  files before submission, including for short-lived `amc run` fixtures.
- `finalize_run` captures available allowlisted files and live default snapshots
  before teardown on success or failure. Collector bounds: 20-second export,
  35 seconds for cleanup, at most 5 seconds per guest/control wait, 4 MiB per
  file, and 16 MiB total payload. These collector bounds neither impose
  workload-runtime limits nor relax mechanism acceptance thresholds.
- Host-side deadlines cover a dead guest control channel. The harness does not
  reconnect if the guest is unavailable or reuse a timed-out/desynchronized
  channel to interpret later results.
- Each owned identity gets a cleanup record: stop/reset/query exit codes,
  available final state, and a cgroup-empty observation. Missing evidence or
  unconfirmed cleanup is UNKNOWN. The original assertion remains the primary
  failure; the record also captures secondary failures.
- Artifacts go to a unique `mechanism-*` directory under the test-driver output.
  Bounded `AMC_ARTIFACT` and `AMC_FINALIZATION` records also preserve allowlisted
  data in the driver log if a failed Nix output is discarded. No raw application
  journal, environment or arbitrary diagnostic error stream is exported.
- Added two not-yet-run tests: VM-only startup cancellation with a targeted
  fixture template, one identity and an unrelated service; and boolean-only
  comparisons of native executable, argv and configured environment.

## Current Checks

Commands used working direnv without truncated pipes or hidden exit statuses.
`checks.json` records normalized tool outcomes.

| Exact command | Outcome |
|---|---|
| `direnv exec . cargo test --all-targets --locked -j 2` | Exit 0: 18 unit + 4 isolated CLI tests passed |
| `direnv exec . python3 scripts/check-fixtures.py` | Exit 0: property scope, explicit reports, unknown telemetry, failure export/cleanup, original-failure preservation and host deadline regressions passed |
| `direnv exec . cargo fmt --all --check` | Exit 0 |
| `direnv exec . cargo clippy --all-targets --locked -j 2 -- -D warnings` | Exit 0 |
| `git diff --check` | Exit 0 |
| `direnv exec . python3 -m py_compile nix/vm-test.py scripts/watch-cgroup.py scripts/check-fixtures.py` | Exit 0 |
| `direnv exec . bash -n .envrc scripts/prove-local.sh` | Exit 0 |
| `nix eval --json --no-write-lock-file .#nixosTests.x86_64-linux.generic --apply 'test: { drvPath = test.drvPath; output = test.outPath; systemd = test.nodes.machine.systemd.package.version; kernel = test.nodes.machine.boot.kernelPackages.kernel.version; }'` | Exit 0: evaluation only |
| Exact `nix build` command above | BLOCKED before execution; exit status not applicable |

## Attempt Identity And Preflight

`source-identity.json` records revision, dirty-diff hash, lock and untracked
input hashes, and the command. `source.diff` preserves the private diff from
the attempted gate, unchanged by later documentation reconciliation.

- Revision: `ff91f5ee24152b9c63de601fae1845aac13e453d`, dirty worktree.
- Dirty-diff SHA-256: `3f41f032cab1ae71e2f408eaf330d8510ff50ce49c64ec854b419b906bb31383`.
- Evaluated derivation: `/nix/store/z9qh57x2mbwsxp120ki0hpdhbjq60814-vm-test-run-amc-generic.drv`.
- Evaluated output path, not realized by this attempt: `/nix/store/hzqkyy4wys22rb67nyam0d2nm46n23br-vm-test-run-amc-generic`.
- Evaluated guest versions: systemd 261.2, kernel 6.18.48. Observed guest versions: none.
- Guest plan: 1024 MiB, 2 vCPUs, no guest swap/zram, existing 600-second driver
  timeout, finite fixture runtime limits and observer-readiness barriers.

`preflight.json` (2026-09-08 09:24:16 UTC) recorded 40,277,053,440
bytes host-available RAM, 2,585,735,168 bytes client-ancestor high/max headroom,
44,172,300,288 bytes broker high/max headroom, global memory PSI avg10 of zero,
KVM access for the current process, and sufficient workspace/store disk space.
Read-only checks left broker and ancestor policy intact. Builder-specific KVM
access remained untested. The snapshot supported a build attempt, not a resource
reservation or proof of safety for future runs.

## Evidence Scope

See `migration-checklist.md` for each claim's implementation/test, outcome,
artifact, and limitation.

- Isolated launcher/diagnostic regressions passed; they do not cover every
  real-manager lost-reply, cancellation or controller failure mode.
- Evaluation covered generated native NixOS drop-in text, not runtime
  lifecycle, environment, or precedence. At this attempt, those runtime checks
  had not run.
- Helper checks are observations at helper entry, not universal pre-exec
  attestation. No general pre-exec supervisor is introduced.
- OOM attribution, fixture-group cleanup, unrelated-unit survival, and the Nix
  broker boundary require VM event/lifecycle evidence, not evaluation, a signal
  result, or host preflight.
- The per-user symlink is a systemd lookup fixture, not Home Manager module
  evaluation/activation. No Home Manager dependency is added for this milestone.
- A/B/C and high/max exercises are smoke tests. Pressure, aggregate parent policy,
  overhead benchmarks and OpenCode usefulness remain deferred and unmeasured.

## Mechanism Attempt (2026-09-10)

Attempt: `proof-artifacts/mechanism-20260910T215606Z/`.
Tool policy denied the exact requested build **before execution**
(verified still in effect; `nix *` deny, no matching real-build permission):

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

No process started, no VM ran, and no driver log exists for this attempt. The
attempt did not bypass denial via wrappers, alternate spellings, or
`canix cache build`. Fresh workspace checks passed on this tree (88 tests:
amc 15, CLI 4, amc-runner 69; fixtures, fmt, clippy, diff-check, py-compile,
bash -n — all exit 0; see this attempt's `checks.json`). Allowed
evaluation-only commands succeeded: drv
`/nix/store/8fcpjh3sssxnvjb55h4gnqgr34h11wby-vm-test-run-amc-generic.drv`
(planned guest systemd 261.2, kernel 6.18.48; observed: none), plus a
`--dry-run` listing planned realizations. Source: revision
`ff91f5ee24152b9c63de601fae1845aac13e453d`, dirty, diff-sha
`4e64eb9c016c5d0ebc284776ae437847a65cd2235901f9640e6322927aad6a12`
(excludes `proof-artifacts/`). Preflight (43 GB host RAM available,
206 GB store disk free, KVM present, eval lock free) supported an attempt,
not a reservation. **Verdict unchanged: MECHANISM GATE
FAILED/BLOCKED (permission, not a demonstrated mechanism failure).**
The execution instructions for this attempt were for the operator to run the
exact command above from the AMC checkout and report the driver outcome, with
success requiring the finalization record to show `complete: true`, all exports
`EXPORTED`, and all owned fixtures `CONFIRMED_EMPTY`.

## Mechanism Attempt (2026-09-11) — PASS

Attempt: `proof-artifacts/mechanism-20260911T053209Z/`.
Ran via the approved cache workflow, without a raw-`nix-build` bypass:

```sh
canix cache build .#nixosTests.x86_64-linux.generic --max-jobs 1 --cores 2 --output json
```

- Result `ok: true`; 127 paths published to `canix-fleet`, verified 127/127.
- drv `/nix/store/3y7zc59s39prf0mp3pszcsiyifc9lz8q-vm-test-run-amc-generic.drv`,
  out `/nix/store/nxv5vs6bg9541cs34nvhpsg0y3a59a5b-vm-test-run-amc-generic`,
  retained `.nix-results/cache-nixosTests.x86_64-linux.generic-dbb08d39d8629f4d`.
- Observed guest versions: kernel 6.18.48, systemd 261.2, nix 2.34.8
  (match the evaluated plan). Zero driver tracebacks.
- All 10 subtests executed: properties/doctor, entry settings,
  rejected-setting, A/B/C + high smoke, startup cancellation, native
  drop-in preservation both directions, precedence, broker boundary,
  attributed OOM with disappearing evidence.
- Finalization: `primaryFailure: null`, `complete: true`, 48/48 exports
  `EXPORTED`, 16/16 owned fixtures `CONFIRMED_EMPTY`.
- Source: revision `ff91f5ee24152b9c63de601fae1845aac13e453d`, dirty,
  diff-sha `30ee23296dbcdaeef6bda0b291968b406dc85c383c57c395a4bb48e3c76a1a3c`
  (refreshed after the in-attempt fixes below; this is the passing tree).

Fixture fixes changed mechanism inputs without weakening assertions: ty-Never
export-list annotation, removed harness-colliding global aliases (ruff F811),
`machine.shell` None guard, removed unused `import os`, and moved the native
fixture to full `systemd.user.units` text plus a per-user drop-in. Stepwise
evidence: `systemd.packages` never propagated the user unit (start failed "not
found"), the etc file/dir collided in make-etc, and derivation-output readFile
is IFD-disabled. Each fix passed `py_compile` and fixture regressions before
retry. Nothing was published before the passing run.

The disposable guest demonstrated OOM attribution with observer deltas,
fixture-group cleanup with cgroup-empty confirmation, unrelated-unit survival,
per-user lookup precedence, native executable/argv/environment preservation,
and the Nix-daemon builder boundary. Pressure comparisons, aggregate parent
policy, overhead benchmarks, OpenCode usefulness, and production limits
remain deferred and unmeasured. Home Manager evaluation of an actual owner unit
remains unproven.

## Mechanism Rerun (2026-09-11) — declarative integration PASS

Attempt: `proof-artifacts/mechanism-20260911T090621Z/`.
The earlier pass used a driver-written per-user drop-in instead of declarative
NixOS `asDropin`, leaving the documented integration untested. This rerun
registers the fixture via `systemd.packages`, puts its fragment at
`lib/systemd/user` (the generator scans only those paths in packages),
and sets limits through `overrideStrategy = "asDropin"`. The passing guest reports
`FragmentPath: /etc/systemd/user/amc-native.service` with
`DropInPaths: ...-user-units/amc-native.service.d/overrides.conf` and
effective `MemoryMax: 268435456`.

- Result `ok: true`; 139 selected paths (19 missing pushed, 343 upstream
  omitted), verified 139/139 on `canix-fleet`; retained link replaced.
- drv `/nix/store/1s4gq1dbfl6ldnfihhl52ybzilsycb6f-vm-test-run-amc-generic.drv`,
  out `/nix/store/r9s23i0zy2l4x2yi468pn4nq741vwczs-vm-test-run-amc-generic`.
- Same guest versions (6.18.48 / 261.2), zero tracebacks, all 10 subtests,
  finalization `complete: true` (48/48 `EXPORTED`, 16/16 `CONFIRMED_EMPTY`).
- Supporting fix: `nix/package.nix` filters sources to Cargo inputs
  plus `nix/vm-test.py` (embedded by `src/systemd.rs` via `include_str!`),
  so docs, standalone scripts, and other Nix files no longer trigger package
  rebuilds, but `nix/vm-test.py` still does. The missing-file failure showed the
  prior filter was too narrow, not that the new one is exact.
- `source-identity.json` preserves the failed-filter attempt's pre-execution
  identity (package.nix `ff922ff5…`); `build-attempt.json` attributes the
  passing retry (package.nix `6c510075…`) by immutable derivation. Guest reports:
  `/nix/store/r9s23i0zy2l4x2yi468pn4nq741vwczs-vm-test-run-amc-generic/mechanism-uk2xa9ry/`;
  the prior manual-drop-in run remains preserved as pattern evidence, not
  declarative coverage.

## Package/Cache Attempt (2026-09-09)

Historical realization: no fingerprint recorded at realization; do not reuse as
current identity.
CLI
`/nix/store/kcbfb1fwkrrb3mcgy35v2zxir2bv1p37-canix-admin-0.1.0/bin/canix`
via canix direnv, cwd AMC, revision
`ff91f5ee24152b9c63de601fae1845aac13e453d`, dirty.
`nix/package.nix` builds `-p amc` (CLI package scope, not a workspace-repair).

```sh
direnv exec /data/nvme0/can/canix bash -c 'cd /data/nvme0/can/canix/projects/repos/owned/amc && canix cache build .#default --max-jobs 1 --cores 2 --no-push'
```

- drv: `/nix/store/vnclmwpjv0d1h2im9wn5wzlzdji5n9m1-amc-0.1.0.drv`
- out: `/nix/store/v98c0z4m2z8gfhmf5lh8nv5hacinlb00-amc-0.1.0`
- retained: `.nix-results/cache-default-3afe8b28d0fa6f73`
- `--no-push` is diagnostic only; not cache-milestone completion.

The 2026-09-09 workspace passes lacked fingerprints, so they establish nothing
about any identified source. The 2026-09-10 run supersedes them.

Fresh workspace run (2026-09-10, via AMC direnv cargo). The recorded
fingerprints matched before and after the run: HEAD
`ff91f5ee24152b9c63de601fae1845aac13e453d`, diff-sha
`8ba4a2d4c2b7b21f67e3f61fe8352d4b76bdd44430a5f21bd29c65215a0f95d8`,
Cargo.lock `7c77ac4578bd656584cddb22040e1009ade6b1cf145b729bfd434a663cc634b3`.
All exit 0: `cargo test --workspace --all-targets --locked -j 2` (amc 18+4,
amc-runner 45), `cargo clippy --workspace --all-targets --locked -j 2 --
-D warnings`, `cargo fmt --all --check`, `python3 scripts/check-fixtures.py`,
`git diff --check`, `python3 -m py_compile` on fixture scripts. Per-check
labels and exits in `/data/scratch/tmp/opencode/amc-checks-20260910.status`.

Inspect classifier (committed upstream in canix `67b599672`/`a7af2c2a5`)
reads only the leading identifier of `Error:`-prefixed Attic records
(`NoSuchCache`; `Unauthorized`/`Forbidden`; `HTTP` head with positional
401/403 status). URL/path tails, port digits, non-positional status words,
contradictory records, and generic "not found" stay unknown. Verified
2026-09-11 in the real canix shell:
`cargo test --manifest-path cli/Cargo.toml -p canix-ops --features admin
inspect_failure --locked` (exit 0) and the full `cache::` set (66 passed,
exit 0); `rustfmt --edition 2024 --check` clean on `native.rs` and
`health/mod.rs`. The standalone mirror is superseded; reconcile/delete it
when convenient.
`canix cache health --cache canix-fleet` reported `NoSuchCache` from local
`attic cache info canix-fleet`: this login's result, not proof of global
absence. No `canix-fleet:` key in `attic.nix`. Cache creation, isolated-store
restore, and the generic VM run were not attempted.

Rechecked 2026-09-11: AMC diff-sha unchanged (`8ba4a2d4...`), retained output
still valid in store, and unprivileged health again reports `NoSuchCache`
for this login. Correction: a canix-admin rebuild WAS attempted and failed.
The subsequent health check used the old profile CLI
(`a36ny518...-canix-admin-hm`), not a verified dev-shell build. Treat that
health result as provisional until a verified executable repeats it.
Subsequently, the concurrent owner staged `hosts/omniroute_accept.rs`, the
canix shell builds again, and real in-crate gates pass (see above). Separate
2026-09-11 fix: `cache health` never registered `Sudo`, leaving `can_sudo`
false and silently using the unprivileged path every time. This was canix
wiring, not proof `sudo -v` failed. The suite now registers `Sudo::default()`
before cache inspection. Privileged inspection, publication, isolated-store
restoration, and the VM remain blocked and unrun.

## Cache Milestone (2026-09-11)

`canix-fleet` already existed. The rollout owner had committed its signing key
(`b6d0f9ed2`, `canix-fleet:OGrxH9eDSB1HuKEd920MKUK5GDpqI16q5FqBPMKc68E=`). That
key matches the live cache (`Public: false`, store `/nix/store`, priority 41,
upstream `["cache.nixos.org-1"]`). The earlier `NoSuchCache` followed a dead operator
token. JWT-secret rotation since mint is plausible but unproven: the server
returns not-found, not unauthorized, for the private cache either way. The
operator refreshed the login with a new 2y admin token; unprivileged
`attic cache info canix-fleet` now succeeds.

First published without rebuilding, then with one command (profile `canix`,
pre-Sudo-fix build; successful inspection made the classifier version irrelevant):

- Sep 9 artifact `/nix/store/v98c0z4m2z8gfhmf5lh8nv5hacinlb00-amc-0.1.0`
  (drv `vnclmwpj…`): `cache push --cache canix-fleet` → 1 missing
  (2141952 B), 6 upstream omitted, narinfo presence confirmed 1/1.
- Fresh `cache build .#default --max-jobs 1 --cores 2` (publication
  enabled): new drv `k7ddj5p6…`, new output
  `/nix/store/w23n8qkry79vqyqrx8il0b3pvci53l7z-amc-0.1.0` (`amc` now depends
  on `amc-runner`; only three new untracked `systemd/` files were staged,
  no content touched), published (1 missing, 2181928 B, 6 upstream omitted),
  presence confirmed, retained link replaced. Nix-internal `cargo test -p amc`
  passed as part of the build.

Canix `verified: true` checks narinfo presence only. `verify.rs` issues
HEAD/Range-GET probes without downloading archives or checking signatures,
so this does not establish signed substitutability.

Byte-level match only: an out-of-band download of the canix-fleet NAR (zstd,
836605 B) decompressed to exactly 2181928 bytes. Its sha256 (`bea6105e…cfef54`)
matches the production store record
(`sha256-vqYQXlwZNA5M3Ab7hlZTxYJrClqEYtYxKH7co6TP71Q=`), proving the served
bytes match the trusted artifact, not that they were restored to a store.

Corrections to prior attempts (preserved, not deleted):

- `nix path-info --store <iso>` only queried, never imported: `null` /
  `not valid` proved nothing about downloads, keys, or sudo. The
  "isolated stores cannot import NARs here" conclusion and the confounded
  negative control are withdrawn.
- The invalid custom Ed25519/fingerprint experiment is retired from acceptance. It
  hashed received text rather than canonicalizing to algorithm-prefixed
  Nix32, and the script has an incorrect scalar bound. It demonstrates no
  Attic signature defect; the substituter-vs-vault framing is withdrawn.
  `verify_nar.py` and `fp_variants.py` remain only for audit.

Restore results (2026-09-11, operator-run; agent shell denies real
`nix build`/import). Stores `OK=/tmp/iso-fleet-ok-XmvnWT`,
`BAD=/tmp/iso-fleet-bad-ZR0mpn`, fresh user-owned, `--max-jobs 0`
(no compilation), signatures enforced, Bearer via staged netrc:

- Step 0, upstream ref: `ref=0` (auth + upstream retrieval work).
- Step 1, wrong fleet key: fails specifically on trust —
  `ignoring substitute ... as it's not signed by any of the keys in
  'trusted-public-keys'`, `neg=1`. Meaningful negative control.
- Step 2, correct key: `pos=0`, substitution-only import, no build activity.
- Step 3, `path-info --store OK --json --recursive`: full 7-path closure
  registered; toplevel `narHash:
  sha256-vqYQXlwZNA5M3Ab7hlZTxYJrClqEYtYxKH7co6TP71Q=` (matches production),
  `narSize: 2181928`, `signatures:
  [canix-fleet:0+JM6jj0jJgmf7Ppi9ERyFR6vei1dKnrbuJ1M/LSULBfpeSByFUtIIaR1K58kRW06ITtI9QvyXDWozWUqFrtBQ==]`
  recorded by Nix itself; upstream refs carry `cache.nixos.org-1`
  signatures. Agent independently re-read the same record from the ISO
  store (read-only, no production reads).

Stock Nix verified and recorded the attic-issued signature: canix-fleet works
as a Nix substituter through the standard trust path. The custom verifier,
not Attic, was at fault; the defect claim is withdrawn.

Cache milestone CLOSED: one-command local AMC builds (`canix cache build .#default`),
outputs retained under the documented finite policy, verified publication to
private `canix-fleet`, and signed isolated-store restoration with compilation
disabled and a trust-specific negative control. No activation, global GC,
production-limit, or unrelated changes occurred.

## Smallest Next Action

Delete `/tmp/canix-fleet-restore-netrc` and the exact attempt stores
(`OK`/`BAD` above plus the eleven enumerated `/tmp/iso-amc-*` skeletons;
the sudo-created one needs its own `sudo rm -rf`). Then pursue the generic
VM separately, with authorization and fresh preflight. Current evidence
does not justify production activation, live-service restart, host stress,
paid requests, agent replay, broker caps, cache rewrites, or server changes.

This continuation did not activate production configuration, restart live
services, run host doctor/stress, make paid requests, replay agent actions, cap
brokers, or integrate applications. Historical corrective/direnv records remain
in `history/`; v0.1 results remain at `docs/proof-results.md`.
