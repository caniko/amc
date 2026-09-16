# Current Mechanism Checklist

Current verdict: **MECHANISM GATE PASSED 2026-09-11** (disposable VM,
both a manual-drop-in run and a declarative-integration rerun).

Artifacts below are under `proof-artifacts/mechanism-20260911T053209Z/`
(first pass) and `proof-artifacts/mechanism-20260911T090621Z/`
(declarative rerun); the 2026-09-08 denial record is preserved in
`proof-artifacts/mechanism-20260908T092416Z/`.
`checks.json` is a normalized tool-outcome record, not VM evidence. Guest
reports for executed rows live in the immutable outputs linked from
`proof-results.md`; names for still-pending items must not be interpreted
as existing files.
Historical ledgers are archived in `history/` rather than mixed into current rows.

| Implementation / exact test | Current execution outcome | Artifact | Remaining limitation |
|---|---|---|---|
| `vm_assertions_use_properties_and_explicit_work_reports`; Python extraction of `emitted_properties` and work-file reads | Passed in isolation | `checks.json` | No VM settings/work execution |
| `diagnostic_properties_omit_free_form_text`; `errors_never_include_subprocess_text`; `inspection_never_reads_journals_or_reprints_error_streams` | Passed in isolation | `checks.json` | Does not attest all real-manager responses |
| `diagnostic_time_and_bytes_are_bounded` | Passed in isolation | `checks.json` | Kernel-uninterruptible operations and SIGKILL are not cleanup guarantees |
| `failed_client_spawn_publishes_identity_without_submission` | Passed in isolation | `checks.json` | Identity alone does not prove what an accepted workload did |
| `rejection_lost_reply_workload_failure_and_detach_do_not_resubmit` | Passed in isolation | `checks.json` | Simulated transport failures, not exhaustive real-manager coverage |
| `cancellation_and_hung_reconciliation_are_bounded` | Passed in isolation | `checks.json` | Real startup cancellation additionally demonstrated in the VM gate |
| `acknowledged_run_has_no_startup_runtime_deadline` | Passed in isolation | `checks.json` | Existing `amc run` runtime remains separate from fixture `RuntimeMaxSec` |
| `doctor_helpers_detect_missing_capabilities` | Passed in isolation | `checks.json` | Rejected-setting case additionally demonstrated in the VM gate; no universal strict pre-exec claim |
| `unknown_is_not_zero_and_malformed_text_is_not_reprinted`; Python observer regressions | Passed in isolation | `checks.json` | Live cgroup disappearance additionally observed in the VM gate (`disappeared.json`) |
| `finalize_run` and the actual outer `finally`, exercised by `scripts/check-fixtures.py` | Passed with original assertion failure, missing artifacts, cleanup exception and unavailable/hung guest doubles | `checks.json` | Driver export/teardown additionally demonstrated in both VM runs (`complete: true`, 48/48 `EXPORTED`) |
| `fixture-properties-and-doctor`, `helper-entry-settings`, `rejected-native-setting-does-not-run-target` | Executed in disposable guest | Passing driver output; guest reports exported | Helper entry is not loader/constructor attestation |
| `pinned-nixos-dropin-preserves-package-unit` | Executed: package fragment at `lib/systemd/user` via `systemd.packages`, declarative `asDropin` merge confirmed (`overrides.conf`, effective 256MiB), preservation booleans all true both directions | Passing driver output; `native.json`, `native-user.json` | Per-user main-unit symlink tests lookup, not Home Manager modules |
| `transient-versus-targeted-dropin-precedence` | Executed both directions; drop-in won twice | `precedence-one/two/winner.json` | Winner observed on pinned systemd 261.2 only |
| `real-manager-cancellation-before-acknowledgment` | Executed: controller SIGTERM, target never ran, unrelated unit survived | `cancellation.json`, before/after snapshots | One cancellation path, not every uncertain-start scenario |
| `attributed-memcg-oom-and-disappearing-evidence` | Executed: observer delta `oom_kill: 1`, `Result=oom-kill`, zero swap, group confirmed empty, unrelated survived | `oom-before/result.json`, `summary.json` | Single OOM shape on one kernel; not a general OOM taxonomy |
| `nix-broker-boundary` | Executed: client in `app-amc-*`, builder in `nix-daemon.service` | `broker.json` | Client placement never implies builder placement; daemon policy untouched |
| Formatting, Rust/CLI tests, Clippy, Python regressions and shell/Python syntax | Executed and passed | `checks.json` | Cheap checks are not VM execution |
| Host resource/KVM preflight | Executed read-only; sufficient at sample time | `preflight.json` | Not reserved capacity; builder KVM access and hidden limits unproven |
| Exact generic VM build | PASS via `canix cache build` (both runs), published + verified on `canix-fleet` | `build-attempt.json`, `source-identity.json`, `source.diff`, retained output links | Observed guest 6.18.48 / systemd 261.2; finalization complete, no primary failure |
| `abc-useful-work-smoke-not-a-pressure-comparison`; `high-max-settings-smoke-not-pressure` | Executed in disposable guest | Work/high reports exported | Smoke tests only; no responsiveness or pressure claim |
| Pressure/high crossing, aggregate parent pressure, incremental overhead | Deferred as instructed | None | Must follow a demonstrated mechanism gate |
| OpenCode trial, production readiness, freeze resolution | Not performed or claimed | None | No application integration or live activation in this milestone |

Lockfiles and the working local direnv configuration are preserved. Native
applications keep their lifecycle owner; no second backend, policy language,
automatic sizing, general pre-exec supervisor or Home Manager dependency is added.
