# Implementation upgrade: AMC aligned with RFC 0.4

Upgrade AMC incrementally. Implement and test the changes, rather than returning another plan. Preserve useful code, repository conventions, and unrelated work. Do not assume the earlier build prompt was fully implemented.

## Authority and outcome

Read `rfc-application-memory-policy-v0.4.md` and `rfc-v0.4-review-and-migration.md` in the repository or attachments. RFC 0.4 supersedes conflicting requirements in `amc-v0.1-build-prompt.md` and RFC 0.3. Record unresolved discrepancies; do not silently choose old behavior. RFC 0.4 does not require a matching software version.

Deliver a small Rust/NixOS integration and evidence harness using existing systemd policy and lifecycle ownership. Users should configure once, then launch normally. Native configuration, diagnostics, and regression tests may suffice without a new launcher abstraction.

Do not add a resident daemon, kernel patches, public contract language, capability metadata, portal, automatic sizing, global process classifier, or second launch backend. Keep an existing `systemd-run` backend unless a demonstrated blocker justifies replacing it; do not build both backends.

## 1. Inspect before editing

Read repository instructions, current code, tests, flake inputs, NixOS module, generated configuration, and prior proof artifacts. Preserve the working tree and pinned dependencies. Do not update nixpkgs/systemd merely to obtain newer interfaces.

Map each RFC change to affected code, tests, and status in a short migration checklist. Mark missing features absent; do not build obsolete features just to migrate them.

Start with read-only inspection: kernel/systemd/Nix-or-Lix versions, cgroup mounts/controllers, user-manager availability, configured swap/zram, active OOM managers, relevant ancestors, and the actual OpenCode launch/backend arrangement. An unrelated degraded service does not establish that resource controls are unsupported. Use narrowly scoped harmless probes if needed; clean up only those probes.

## 2. Correct resource and failure semantics first

Remove `MemoryOOMGroup` from required probes, generated arguments, configuration examples, and positive tests. Retain its spelling only in migration guidance or a regression test that prevents its emission. For a disposable group-OOM fixture, use native `OOMPolicy=kill` and verify `memory.oom.group=1`. Do not write that file directly in a systemd-owned cgroup. [U1][U3]

Remove any existing blanket application of fixture settings to real applications. Preserve `OOMPolicy`, `KillMode`, `Restart`, `Delegate`, service type, and dependencies unless you explicitly select a narrow change. `KillMode` governs shutdown; it does not replace OOM policy. Do not introduce automatic restarts on new fixtures or unknown command-replay paths. Do not replay agent actions or package operations after failure.

Allow `MemoryHigh` and remove any `PressureAware` metadata requirement. Support native high/max settings without a new policy vocabulary. Keep max-only to isolate hard-limit behavior; compare high/max separately. Neither fixture defines production defaults. [U2]

Use this fragment only in a disposable test; let the fixture supply execution details:

```ini
[Service]
MemoryAccounting=yes
MemoryMax=256M
MemorySwapMax=0
OOMPolicy=kill
Restart=no
```

Do not apply these numbers or zero-swap behavior to OpenCode, the session, or a shared daemon. Do not disable host oomd, change swappiness, resize swap, or introduce universal protection floors. Preserve existing pressure handling; identify which mechanism acts in each test.

Correct claims throughout: limits neither preallocate RAM nor promise exact instantaneous physical-memory ceilings, completion, or fixed-time termination/recovery. Distinguish allocation failure, reclaim, kernel OOM, userspace OOM action, external termination, and service recovery. A timeout is an observation or test failure, not proof of a kernel contract violation. [U2][U7]

## 3. Make native configuration authoritative

First find the chosen application's execution owner: unit or launcher. Prefer a targeted native unit/drop-in change. Reuse names, aliases, slices, and activation. Wrapping an activation client does not govern the server. Do not create a second lifecycle owner. [U4][U5]

Use the pinned NixOS module's supported merge/drop-in mechanism. Inspect the resulting unit content and verify that adding policy preserves the package unit's executable, dependencies, sandboxing, and service type. Reuse `systemd.user.services`, `systemd.user.slices`, or corresponding system-unit settings where appropriate. Avoid a second `programs.amc` hierarchy that merely duplicates native options.

Keep an existing private TOML parser, profile selector, and CLI if useful for fixtures or compatibility. Avoid gratuitous rewrites and do not present them as an upstream standard. New normal-launch documentation should use native settings. Route legacy settings through explicit compatibility/test paths with provenance, never silent native-policy overrides. Do not make the old byte parser validate every legal native systemd value.

Test actual precedence on the pinned version, including applicable drop-ins versus explicitly submitted transient properties. Do not invent a vendor/admin/user merge engine or treat source files as effective settings. Avoid optional defaults or `infinity` values that mask existing configuration. Separate fixture and real application policy. [U6]

Do not install global `app-`/`service.d` overrides, globally replace commands on PATH, or reparent existing applications just to simplify testing. Existing test slices may remain test-specific.

Application IDs label configuration; they do not authenticate. Document that same-user integration addresses accidental overload, not sandboxing against equivalent user-manager privileges. Respect explicit delegation and single-writer ownership. Report relevant administrator-owned ancestors without automatically changing them. [U3]

## 4. Preserve launch and activation behavior

For explicit transient-service fixtures, retain literal argv handling, disabled systemd argument environment expansion, safe unit names, and controlled environment transfer. Use `Type=exec` only where appropriate for a newly owned service; do not replace existing `Type=dbus` or `Type=notify`. [U1][U8]

For native or D-Bus activation, preserve the registered server, singleton behavior, files/URIs, desktop actions, activation tokens, sandbox, and lifecycle. Reopening a running application must neither duplicate it nor silently change its limits. Activating an existing instance does not reapply pre-execution policy. [U5]

Remove claims that scopes are inherently racy. Release target workload code only after configured placement. Keep the current service path if adequate; do not add scope synchronization for completeness. Any supported scope path must demonstrate waiting, successful registration, release, cancellation, and cleanup. Review source ordering and require early target/descendant reports; `main()` alone cannot prove earlier behavior.

Allocate one unit identity per transient launch attempt. On timeout or ambiguous submission, reconcile that unit/job before acting. Do not retry under a new name or launch an unrestricted fallback. If reconciliation remains unavailable, report the unknown outcome and recovery steps; do not claim nothing started. Clean up only units the attempt owns. Use fault injection to test accepted-request/lost-response and cancellation.

Strict test launches must reject missing required enforcement before releasing the workload. Prove that ordering on the supported launch path; post-start detection does not suffice. Mark paths that cannot meet it unsupported for strict launch. Optional integration may launch normally without containment only if it reports that explicitly.

Do not present PTY/pipe support as transparent shell integration. Test the terminal paths actually supported: literal arguments, cwd, environment, stdin/stdout/stderr, exit status, Ctrl-C, resize, cancellation, and relevant job-control/pipeline behavior. Record unsupported behavior; do not redesign the shell to hide it.

## 5. Upgrade diagnostics and evidence handling

Adapt existing `doctor`, `explain`, and `inspect` rather than adding another management service.

Separate requested, manager-reported, and kernel-observed settings. Before a unit exists, label effective values unverified. Report unit identity/aliases, fragment and drop-in paths, manager context, cgroup path, and relevant ancestry. Include available high/max/swap/low/min, OOM grouping and policy, delegation, and relevant pressure-manager settings. Distinguish required capabilities from optional counters or convenience APIs.

Read available memory/swap usage, peaks, event counters and PSI with timestamps. Mark absent, inaccessible, stale, or disappeared data as unknown with a reason—not zero or pass. Inspect ancestors as well as leaves; do not reduce hierarchical reclaim protection to a simplistic minimum-of-values formula.

During execution, collect and retain the last readable snapshots outside the tested failure domain, along with counter deltas, exit/result information, and relevant journal evidence before cleanup. Do not assume `--retain-unit` or omitting `--collect` preserves cgroup counters; correct that option's docs and tests. Exit status 137 alone does not prove OOM. Do not require an optional recent systemd counter if another supported evidence source exists.

Do not log full argv, environment values, API credentials, or command contents by default. Use bounded/redacted reports. Any optional raw logs must be private, clearly marked, and excluded from automatic publication.

Explicitly report known execution domains and which are external: OpenCode frontend/backend, ordinary descendants, and external work such as Nix/Lix builders, containers, and remote jobs where identified. Do not attempt generic IPC causality tracking. Configuring a Nix client does not prove its daemon's builders are configured. Keep any existing whole-pool experiment separately opt-in; do not implement or enable it as an upgrade prerequisite. [U9]

## 6. Rebuild the proof around comparisons, not a single kill

Keep routine cargo tests and project flake checks cheap. Give VM and stress experiments separate explicit targets. Reuse pinned tools and fixtures, limit build/test concurrency, and start with targeted checks rather than a workstation-wide flake evaluation.

Use one modest disposable NixOS VM with explicit RAM/swap limits, finite workloads, an outer timeout, cleanup, and a watchdog outside the tested cgroup. Budget VM/build overhead on the host. Default destructive pressure tests to VM-only and local proof to inspection/harmless checks. Any retained host stress mode requires explicit opt-in and verified bounds. Do not disable host protections to pass tests.

Implement these three arms:

- A: the unchanged launch path in the disposable VM, with finite workload and outer safety bounds.
- B: native unit configuration or direct `systemd-run`, independent of AMC launch/resolver code.
- C: the proposed integration with the same workload and effective policy as B.

Before interpreting results, compare B/C limits, ancestors, lifecycle settings, and swap configuration. Treat max-only versus high/max as a separate factor. Do not silently give C stronger limits. If C is native configuration plus diagnostics, say so; it does not need a custom launcher.

Required regression coverage, scoped to the implemented paths:

1. Unsupported-property removal; correct fixture group behavior; preservation of native lifecycle and delegation.
2. Literal argv/environment behavior and legacy configuration compatibility.
3. Targeted drop-in precedence and requested/effective mismatch reporting.
4. Missing required facilities, unavailable optional telemetry, start uncertainty, and cancellation without duplicate launch.
5. Actual touched allocation, early target/descendant membership, correct multi-process failure domain, and survival of unrelated units.
6. Several individually bounded workloads competing under an aggregate test ancestor; record ancestor pressure and victims.
7. Evidence collection when cgroups disappear and cleanup after success, failure, or interruption.
8. Native activation/server reuse for the selected integration. Exercise relevant D-Bus or terminal semantics only where supported; clearly label untested paths.
9. Known brokered-work coverage limits. Reuse a small existing Nix boundary fixture; do not make shared-daemon killing a required test.

Use nontrivial touched working sets, including data that does not trivially compress when compression is relevant. Include a finite healthy task with useful-work/completion accounting, not only a runaway allocator and heartbeat. Record latency samples, p95/p99/worst pauses, throughput/completions, termination/lost-work counts, pressure, and incremental helper/forwarding overhead.

Set fixture-specific thresholds, repetitions, and workloads before running. Keep raw results; do not retune thresholds to manufacture a pass. An unchanged baseline with no pressure cannot show improvement under pressure. A surviving heartbeat is a mechanism smoke test, not proof of OpenCode responsiveness.

## 7. Demonstrate normal launching without disrupting the host

Prepare one targeted NixOS integration for the owner you discover. Prefer OpenCode without assuming its installation or client/server layout. Show exact configuration, the unchanged launch command or desktop action, observed owning unit, and execution coverage.

Validate in isolation first. Do not restart the active coding-agent session, live OpenCode backend, user manager, compositor, or Nix daemon. Do not run `nixos-rebuild switch/test`, activate Home Manager changes, or change live limits automatically. Give the user the reviewed diff and explicit activation and rollback steps.

Prepare a bounded application trial in a disposable session, with no paid requests or state-changing agent tasks. Record observable latency and completed work. If access or instrumentation is unavailable, finish independent migration/tests and mark real-workload benefit NOT MEASURED. Synthetic results cannot fill that gap.

## Deliverables and stopping rule

Return focused implementation changes, migration notes, exact commands, a list of changed files, pinned versions/capabilities, test artifacts, and NixOS enable/rollback steps. Preserve old reports as historical evidence, not records of the new revision.

Report separately:

- mechanism correctness;
- configuration/activation compatibility;
- ordinary-launch coverage;
- measured responsiveness and useful work;
- incremental resource cost;
- brokered-work limitations.

Use PASS, FAIL, SKIPPED, or NOT MEASURED with reasons. Distinguish implemented, executed, and demonstrated. A skipped VM run is not a pass. Do not claim upstream approval, OOM impossibility, zero overhead, or that OpenCode is fixed without relevant measurements.

Complete the smallest coherent upgrade. If native configuration suffices, document it and stop adding architecture. For a blocked path, finish independent fixes and report the reproducer and smallest next patch. Do not add a daemon, global wrapper, heuristic killer, broad compatibility layer, or repository rewrite to work around a blocker.

## Primary references

Verify behavior against the locally pinned versions; these references are not instructions to upgrade dependencies.

- [U1] Service semantics and OOMPolicy: https://www.freedesktop.org/software/systemd/man/systemd.service.html
- [U2] Resource controls: https://www.freedesktop.org/software/systemd/man/systemd.resource-control.html
- [U3] Cgroup ownership/delegation: https://systemd.io/CONTROL_GROUP_INTERFACE/
- [U4] Desktop integration guidance: https://systemd.io/DESKTOP_ENVIRONMENTS/
- [U5] D-Bus activation: https://specifications.freedesktop.org/desktop-entry/latest/dbus.html
- [U6] Unit lookup and drop-ins: https://www.freedesktop.org/software/systemd/man/systemd.unit.html
- [U7] Kernel memory-controller semantics: https://docs.kernel.org/admin-guide/cgroup-v2.html
- [U8] systemd-run behavior: https://www.freedesktop.org/software/systemd/man/systemd-run.html
- [U9] Nix cgroup boundary fixture: https://github.com/NixOS/nix/blob/master/tests/nixos/cgroups/default.nix
