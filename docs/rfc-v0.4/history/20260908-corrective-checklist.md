# Historical Corrective Milestone Checklist

Historical record only. Current status is in `../migration-checklist.md`.

**2026-09-08 direnv follow-up:** the environment no longer blocks cheap checks.
All 18 unit and 4 isolated CLI tests, Python fixture regressions, formatting and
Clippy now pass through `direnv exec .`. Rows naming those tests were
implemented and executed in isolation. Live cgroup observation, real native
precedence, OOM attribution and other VM-only checks did not run. See
`proof-results.md` for exact commands and the follow-up artifact. The table
preserves the initial milestone status. This update supersedes its
environment-blocked entries without claiming a VM pass.

RFC 0.4 supersedes the earlier build brief. The previous "Done" ledger overstated
coverage; this one separates implemented, executed, demonstrated, blocked and
deferred work. The corrective work introduces no new application architecture.

| Defect / requirement | Implementation or exact test | Status |
|---|---|---|
| Property assertions inspect only emitted settings | `emitted_properties`, `vm_assertions_use_properties_and_explicit_work_reports`, `scripts/check-fixtures.py` | Implemented; execution blocked |
| Work results read from explicit files | `abc-useful-work-smoke-not-a-pressure-comparison`, `scripts/check-fixtures.py` | Implemented; execution blocked |
| Default diagnostics omit journals/free-form error streams | `errors_never_include_subprocess_text`, `diagnostic_properties_omit_free_form_text`, `inspection_never_reads_journals_or_reprints_error_streams` | Implemented; execution blocked |
| Diagnostic time/byte bounds | `diagnostic_time_and_bytes_are_bounded` | Implemented; execution blocked |
| Local script cannot run stress or overwrite historical reports | `scripts/prove-local.sh`; doctor is documented as active | Implemented; shell syntax executed |
| One identity available before submission | `--unit-file`, `failed_client_spawn_publishes_identity_without_submission` | Implemented; execution blocked |
| Rejection/lost reply/workload failure/detach | `rejection_lost_reply_workload_failure_and_detach_do_not_resubmit` | Implemented; execution blocked |
| Bounded SIGINT/SIGTERM handling and hung reconciliation | `cancellation_and_hung_reconciliation_are_bounded` | Implemented; execution blocked |
| Startup deadline is not runtime deadline | `acknowledged_run_has_no_startup_runtime_deadline`; optional manager `RuntimeMaxSec` for fixtures | Implemented; execution blocked |
| Missing memory controller / rejected settings | `doctor_helpers_detect_missing_capabilities`, VM `rejected-native-setting-does-not-run-target` | Isolated checks implemented; execution blocked |
| Strong arbitrary pre-exec fail-closed guarantee | No new supervisor; helper checks occur at helper entry before allocation | Unsupported; previous claim withdrawn |
| Visible cgroup ancestry, lifecycle/provenance, timestamps | `telemetry::snapshot`, context-aware ancestor metadata | Implemented; runtime execution blocked |
| Missing/malformed/disappeared measurements | `unknown_is_not_zero_and_malformed_text_is_not_reprinted`, observer regressions | Implemented; execution blocked |
| Observer readiness and finite collection | `watch-cgroup.py`, VM `attributed-memcg-oom-and-disappearing-evidence` | Implemented; execution blocked |
| Native settings, lifecycle and lookup | `pinned-nixos-dropin-preserves-package-unit` | NixOS fragment generation evaluated; runtime blocked |
| Conflicting transient/drop-in settings | `transient-versus-targeted-dropin-precedence` reverses the two values and records which wins | Implemented; precedence not yet measured |
| Actual Home Manager module evaluation | This repository lacks a pinned Home Manager input | Blocked; per-user symlink test is not a substitute |
| Attributed OOM, unrelated-unit survival and owned cleanup | `attributed-memcg-oom-and-disappearing-evidence` uses event delta plus `Result=oom-kill` | Implemented; VM not executed |
| Broker boundary | `nix-broker-boundary`; existing pool remains blocked | Implemented; new run not demonstrated |
| Formatting/tests/Clippy/Python fixture checks | Pinned `nix develop` gate denied by tool policy | Blocked; old test counts do not apply |
| Targeted VM derivation and no-build flake checks | Commands in `proof-results.md` | Executed; evaluation only |
| A/B/C pressure, actual high crossing, aggregate parent pressure, overhead | Labeled existing small exercises smoke tests; removed invalid two-leaf aggregate test | Deferred until mechanism gates pass |
| OpenCode activation/UX benefit | No production activation, restarts or paid/state-changing trial | Deferred; not measured |

The scripts record results per run without rewriting historical proof reports.
They do not treat signal exit codes, missing counters, successful evaluation or
surviving heartbeats as proof of OOM attribution or OpenCode usability.
