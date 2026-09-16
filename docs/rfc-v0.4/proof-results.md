# Mechanism Validation Results

**Verdict: MECHANISM GATE PASSED 2026-09-11 (disposable VM evidence below).**
Prior attempts failed/blocked on permissions and unrealized fixture defects;
this record supersedes them for the mechanism claim only.

Attempt: `proof-artifacts/mechanism-20260908T092416Z/`.
The exact requested build was denied by tool policy **before execution**:

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

There is no process exit status, VM execution, observed guest version, realized
test output, cached test result used, or driver log from this attempt. The denial
was not bypassed through direnv, scripts, another executable path or permission
changes. The generic test remains outside ordinary flake checks.

## Changes In This Attempt

- Boot and test assertions are inside the finalization boundary. Identity files
  are registered before submission, including short-lived `amc run` fixtures.
- `finalize_run` captures available allowlisted files and live default snapshots
  before teardown, on failure as well as success. Export has a 20-second budget,
  cleanup 35 seconds, individual guest/control waits at most 5 seconds, files at
  most 4 MiB each and captured payload at most 16 MiB. These are collector bounds,
  not new workload-runtime limits or relaxed mechanism acceptance thresholds.
- Host-side deadlines cover a dead guest control channel; no reconnect is
  attempted when the guest is unavailable. A timed-out/desynchronized channel
  is not reused to interpret a later command's result.
- Every owned identity gets a cleanup record with stop/reset/query exit codes,
  final state where available and a cgroup-empty observation. Missing evidence
  or unconfirmed cleanup is UNKNOWN. The original assertion remains the primary
  failure; secondary failures are recorded without replacing it.
- Artifacts go to a unique `mechanism-*` directory under the test-driver output.
  Bounded `AMC_ARTIFACT` and `AMC_FINALIZATION` records also preserve allowlisted
  data in the driver log if a failed Nix output is discarded. No raw application
  journal, environment or arbitrary diagnostic error stream is exported.
- Added a VM-only startup cancellation case using a targeted fixture template,
  one identity and an unrelated service. Added boolean-only native executable,
  argv and configured-environment comparisons. Neither test has executed yet.

## Current Checks

Each command ran through the working direnv environment without truncating pipes
or hidden exit statuses. `checks.json` records normalized actual tool outcomes.

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

`source-identity.json` records revision, dirty-diff hash, lock hashes, untracked
input hashes and the command. `source.diff` preserves the private source diff at
the attempted gate; later documentation reconciliation does not change that
historical snapshot.

- Revision: `ff91f5ee24152b9c63de601fae1845aac13e453d`, dirty worktree.
- Dirty-diff SHA-256: `3f41f032cab1ae71e2f408eaf330d8510ff50ce49c64ec854b419b906bb31383`.
- Evaluated derivation: `/nix/store/z9qh57x2mbwsxp120ki0hpdhbjq60814-vm-test-run-amc-generic.drv`.
- Evaluated output path, not realized by this attempt: `/nix/store/hzqkyy4wys22rb67nyam0d2nm46n23br-vm-test-run-amc-generic`.
- Evaluated guest versions: systemd 261.2, kernel 6.18.48. Observed guest versions: none.
- Guest plan: 1024 MiB, 2 vCPUs, no guest swap/zram, existing 600-second driver
  timeout, finite fixture runtime limits and observer-readiness barriers.

`preflight.json` was captured at 2026-09-08 09:24:16 UTC. It found 40,277,053,440
bytes host-available RAM, 2,585,735,168 bytes client-ancestor high/max headroom,
44,172,300,288 bytes broker high/max headroom, global memory PSI avg10 of zero,
accessible KVM for the current process and sufficient workspace/store disk space.
Read-only observations did not modify existing broker or ancestor policy.
Builder-specific KVM access remained untested. This snapshot was sufficient to
attempt the build, not a resource reservation or proof that a future run is safe.

## Evidence Scope

See `migration-checklist.md` for implementation/test -> outcome -> artifact ->
limitation for each claim. In particular:

- Isolated launcher/diagnostic regressions passed; they do not cover every
  real-manager lost-reply, cancellation or controller failure mode.
- Generated native NixOS drop-in text has been evaluated; runtime lifecycle,
  environment and precedence checks remain unexecuted.
- Helper checks are observations at helper entry, not universal pre-exec
  attestation. No general pre-exec supervisor is introduced.
- OOM attribution, fixture-group cleanup, unrelated-unit survival and the Nix
  broker boundary require the VM's actual event/lifecycle evidence. They are not
  demonstrated by evaluation, a signal result, or the host preflight.
- The per-user symlink is a systemd lookup fixture, not Home Manager module
  evaluation/activation. No Home Manager dependency is added for this milestone.
- A/B/C and high/max exercises are smoke tests. Pressure, aggregate parent policy,
  overhead benchmarks and OpenCode usefulness remain deferred and unmeasured.

## Mechanism Attempt (2026-09-10)

Attempt: `proof-artifacts/mechanism-20260910T215606Z/`.
The exact requested build was denied by tool policy **before execution**
(verified still in effect; `nix *` deny, no matching real-build permission):

```sh
nix build --no-write-lock-file --max-jobs 1 --cores 2 .#nixosTests.x86_64-linux.generic
```

No process started, no VM executed, no driver log. The denial was not
bypassed via wrappers, alternate spellings, or `canix cache build`.
Fresh workspace checks all passed on this tree (88 tests: amc 15, CLI 4,
amc-runner 69; fixtures, fmt, clippy, diff-check, py-compile, bash -n —
all exit 0; see attempt `checks.json`). Allowed evaluation-only commands
succeeded: current drv
`/nix/store/8fcpjh3sssxnvjb55h4gnqgr34h11wby-vm-test-run-amc-generic.drv`
(planned guest systemd 261.2, kernel 6.18.48; observed: none), plus a
`--dry-run` listing what would be realized. Source identity: revision
`ff91f5ee24152b9c63de601fae1845aac13e453d`, dirty, diff-sha
`4e64eb9c016c5d0ebc284776ae437847a65cd2235901f9640e6322927aad6a12`
(excludes `proof-artifacts/`). Preflight (43 GB host RAM available,
206 GB store disk free, KVM present, eval lock free) supported an attempt;
it is not a reservation. **Verdict unchanged: MECHANISM GATE
FAILED/BLOCKED (permission, not a demonstrated mechanism failure).**
To execute, the operator runs the exact command above from the AMC
checkout and reports the driver outcome; on success the finalization
record must show `complete: true` with all exports `EXPORTED` and all
owned fixtures `CONFIRMED_EMPTY`.

## Mechanism Attempt (2026-09-11) — PASS

Attempt: `proof-artifacts/mechanism-20260911T053209Z/`.
Executed via the approved cache workflow (no raw-`nix-build` bypass):

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

In-attempt fixture fixes (mechanism inputs, not weakened assertions):
ty-Never export-list annotation, removed harness-colliding globals aliases
(ruff F811), `machine.shell` None guard, removed unused `import os`, and the
native fixture moved to `systemd.user.units` full text plus a per-user
drop-in — established stepwise from evidence: `systemd.packages` never
propagated the user unit (start failed "not found"), etc file/dir collided
in make-etc, and derivation-output readFile is IFD-disabled. Each fix was
re-verified (`py_compile`, fixture regressions) before retry; nothing was
published until the passing run.

What this proves — and does not: OOM attribution with observer deltas,
fixture-group cleanup with cgroup-empty confirmation, unrelated-unit
survival, per-user lookup precedence, native executable/argv/environment
preservation, and the Nix-daemon builder boundary are now demonstrated in a
disposable guest. Pressure comparisons, aggregate parent policy, overhead
benchmarks, OpenCode usefulness, and production limits remain deferred and
unmeasured. Home Manager evaluation of an actual owner unit is still
unproven.

## Mechanism Rerun (2026-09-11) — declarative integration PASS

Attempt: `proof-artifacts/mechanism-20260911T090621Z/`.
The earlier passing run had substituted a driver-written per-user drop-in
for the declarative NixOS `asDropin`, leaving the documented integration
uncovered. This rerun restores it: the fixture fragment lives at
`lib/systemd/user` in its package (the only package paths the generator
scans) and is registered via `systemd.packages`, with limits arriving
through declarative `overrideStrategy = "asDropin"`. Evidence, not
assumption: the passing guest reports
`FragmentPath: /etc/systemd/user/amc-native.service` with
`DropInPaths: ...-user-units/amc-native.service.d/overrides.conf` and
effective `MemoryMax: 268435456`.

- Result `ok: true`; 139 selected paths (19 missing pushed, 343 upstream
  omitted), verified 139/139 on `canix-fleet`; retained link replaced.
- drv `/nix/store/1s4gq1dbfl6ldnfihhl52ybzilsycb6f-vm-test-run-amc-generic.drv`,
  out `/nix/store/r9s23i0zy2l4x2yi468pn4nq741vwczs-vm-test-run-amc-generic`.
- Same guest versions (6.18.48 / 261.2), zero tracebacks, all 10 subtests,
  finalization `complete: true` (48/48 `EXPORTED`, 16/16 `CONFIRMED_EMPTY`).
- Supporting fix: `nix/package.nix` now filters sources to Cargo inputs
  plus `nix/vm-test.py` (embedded by `src/systemd.rs` via `include_str!`),
  so docs, standalone scripts, and other Nix files no longer rebuild the
  package — but editing `nix/vm-test.py` still does. The missing-file
  failure showed the filter was too narrow, not that it is exact.
- Pre-execution identity (`source-identity.json`, package.nix `ff922ff5…`)
  is preserved as the failed-filter attempt; the passing retry (package.nix
  `6c510075…`) is attributed separately in `build-attempt.json` via its
  immutable derivation. Guest reports live at
  `/nix/store/r9s23i0zy2l4x2yi468pn4nq741vwczs-vm-test-run-amc-generic/mechanism-uk2xa9ry/`;
  the prior manual-drop-in run remains preserved as pattern evidence,
  distinct from this declarative coverage.

## Package/Cache Attempt (2026-09-09)

Historical realization (source fingerprint not recorded at the time; do not
reuse as current identity). CLI
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

Earlier workspace passes (2026-09-09) are unattributed: no fingerprints were
captured around those runs, so they establish nothing about any identified
source. The 2026-09-10 run below supersedes them.

Fresh workspace run (2026-09-10, via AMC direnv cargo). Fingerprints match
before and after, so no source drift invalidates the run: HEAD
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
`canix cache health --cache canix-fleet` reported local
`attic cache info canix-fleet` as `NoSuchCache`. That is this login's result,
not proof of global absence. No `canix-fleet:` key in `attic.nix`. Create was
not run. Isolated-store restore was not attempted. Generic VM remains
unexecuted.

Re-observed 2026-09-11: AMC diff-sha unchanged (`8ba4a2d4...`), retained
output still valid in store, and unprivileged health again reports
`NoSuchCache` for this login. Correction to the earlier claim: a canix-admin
rebuild WAS attempted and failed at the time, and the health check that
followed ran under the old profile CLI (`a36ny518...-canix-admin-hm`), not a
verified dev-shell build — treat that observation as provisional until a
verified executable repeats it. Since then the concurrent owner staged
`hosts/omniroute_accept.rs`, the canix shell builds again, and the real
in-crate gates pass (see above). Separately fixed 2026-09-11: `cache health`
never registered the existing `Sudo` check, so `can_sudo` was always false
and every run silently took the unprivileged path — this was a canix wiring
bug, not proof that `sudo -v` failed. The suite now registers
`Sudo::default()` before the cache inspection. Privileged inspection,
publication, isolated-store restoration, and the VM remain not run (blocked,
not complete).

## Cache Milestone (2026-09-11)

`canix-fleet` pre-existed — no creation was needed. Its signing key was
already committed by the rollout owner (`b6d0f9ed2`,
`canix-fleet:OGrxH9eDSB1HuKEd920MKUK5GDpqI16q5FqBPMKc68E=`) and matches the
live cache (`Public: false`, store `/nix/store`, priority 41, upstream
`["cache.nixos.org-1"]`). The earlier `NoSuchCache` followed a dead operator
token; JWT-secret rotation since mint is plausible but unproven (the server
returns not-found rather than unauthorized for the private cache either
way). The operator refreshed the login with a new 2y admin token;
unprivileged `attic cache info canix-fleet` now succeeds.

Published without rebuilding, then via one command (profile `canix`,
pre-Sudo-fix build — inspect succeeded so the classifier version is moot):

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

Canix `verified: true` is a narinfo presence check only
(`verify.rs` issues HEAD/Range-GET probes; it downloads no archive and
checks no signature), so it does not establish signed substitutability.

Byte-level content match (valid, narrow): the canix-fleet NAR (zstd,
836605 B) was downloaded out-of-band, decompressed to exactly 2181928 bytes,
and its sha256 (`bea6105e…cfef54`) equals the production store record
(`sha256-vqYQXlwZNA5M3Ab7hlZTxYJrClqEYtYxKH7co6TP71Q=`). This proves the
served bytes are the trusted artifact; it is not a store restoration.

Corrections to prior attempts (preserved, not deleted):

- Every `nix path-info --store <iso>` run was a query, not an import: `null`
  / `not valid` proved nothing about downloads, keys, or sudo. The
  "isolated stores cannot import NARs here" environment conclusion and the
  confounded negative control are withdrawn.
- The custom Ed25519/fingerprint experiment is invalid and retired from
  acceptance testing: it hashed the received text instead of canonicalizing
  to algorithm-prefixed Nix32, and the script carries an incorrect scalar
  bound. No Attic signature defect is demonstrated; the substituter-vs-vault
  decision framing is withdrawn. Scripts remain as audit trail only
  (`verify_nar.py`, `fp_variants.py`).

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

This settles the signature question empirically: stock Nix verified and
recorded the attic-issued signature, so canix-fleet IS usable as a Nix
substituter through the standard trust path. The custom-verifier failure
was the experiment's bug, not an Attic defect — fully withdrawn.

Cache milestone CLOSED: local project builds via one command
(`canix cache build .#default`), outputs retained under the documented
finite policy, publication to private `canix-fleet` verified, and signed
restoration demonstrated in an isolated store with compilation disabled —
including the trust-specific negative control. No activation, global GC,
production-limit, or unrelated changes occurred.

## Smallest Next Action

Delete `/tmp/canix-fleet-restore-netrc` and the exact attempt stores
(`OK`/`BAD` above plus the eleven enumerated `/tmp/iso-amc-*` skeletons;
the sudo-created one needs its own `sudo rm -rf`). Then pursue the generic
VM as a separate authorized gate with fresh preflight. No production
activation, live-service restart, host stress, paid request, agent replay,
broker cap, cache rewrite, or server change is justified by the current
evidence.

No production activation, live-service restart, host doctor/stress, paid request,
agent replay, broker cap or application integration occurred in this continuation.
Historical corrective/direnv records are preserved in `history/`; v0.1 results
remain separately at `docs/proof-results.md`.
