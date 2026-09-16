# Implementation upgrade: AMC aligned with RFC 0.4

Upgrade the existing AMC implementation incrementally. Implement and test the changes; do not return only another plan. Preserve useful code, existing repository conventions, and unrelated work. Do not assume the earlier build prompt was fully implemented.

## Authority and outcome

Read `rfc-application-memory-policy-v0.4.md` and `rfc-v0.4-review-and-migration.md`, wherever they are stored in the repository or supplied attachments. RFC 0.4 supersedes conflicting requirements in `amc-v0.1-build-prompt.md` and RFC 0.3. Record unresolved discrepancies rather than silently choosing old behavior. RFC revision 0.4 does not mandate a matching software version number.

Deliver a small Rust/NixOS integration and evidence harness that uses existing systemd policy and lifecycle ownership. The user-facing goal remains: configure once, then launch normally. A successful outcome may be native configuration plus diagnostics and regression tests, without a new launcher abstraction.

Do not add a resident daemon, kernel patches, public contract language, capability metadata, portal, automatic sizing, global process classifier, or second launch backend. Keep an existing `systemd-run` backend unless a demonstrated blocker justifies replacing it; do not build both backends.

## 1. Inspect before editing

Read repository instructions, current code, tests, flake inputs, NixOS module, generated configuration, and prior proof artifacts. Preserve the working tree and pinned dependencies. Do not update nixpkgs/systemd merely to obtain newer interfaces.

Create a short migration checklist mapping each RFC change to affected code, tests, and completion status. Mark absent features as absent; do not build an obsolete feature just to migrate it.

Inspect local capabilities read-only first: kernel/systemd/Nix-or-Lix versions, cgroup mounts/controllers, user-manager availability, configured swap/zram, active OOM managers, relevant ancestors, and the actual OpenCode launch/backend arrangement. Do not infer unsupported resource controls merely because the manager reports an unrelated degraded service. Use narrowly scoped harmless probes where necessary, and clean up only those probes.

## 2. Correct resource and failure semantics first

Remove `MemoryOOMGroup` from required probes, generated arguments, configuration examples, and positive tests. Retain its spelling only in migration guidance or a regression test that prevents its emission. For a disposable group-OOM fixture, use native `OOMPolicy=kill` and verify `memory.oom.group=1`. Do not write that file directly in a systemd-owned cgroup. [U1][U3]

Remove blanket application of fixture settings to real applications. In particular, preserve existing `OOMPolicy`, `KillMode`, `Restart`, `Delegate`, service type, and dependencies unless a narrowly scoped change is explicitly selected. `KillMode` concerns service shutdown, not a substitute for OOM policy. Newly created fixtures and unknown command-replay paths must not acquire automatic restarts. Do not replay agent actions or package operations after failure.

Remove any ban on `MemoryHigh` or requirement for `PressureAware` metadata. Support native high/max configuration without inventing another policy vocabulary. Keep the max-only experiment to isolate hard-limit behavior; compare a high/max experiment separately. Neither fixture defines production defaults. [U2]

Use this only as a disposable test fragment, with execution details supplied by the fixture:

```ini
[Service]
MemoryAccounting=yes
MemoryMax=256M
MemorySwapMax=0
OOMPolicy=kill
Restart=no
```

Do not apply these numbers or zero-swap behavior to OpenCode, the session, or a shared daemon. Do not disable host oomd, change swappiness, resize swap, or introduce universal protection floors. Preserve existing pressure handling; identify which mechanism acts in each test.

Correct claims throughout the project: limits are not preallocated RAM, exact instantaneous physical-memory ceilings, guaranteed completion, or fixed-time termination/recovery. Distinguish allocation failure, reclaim, kernel OOM, userspace OOM action, external termination, and service recovery. A timeout is an observation or test failure, not proof of a kernel contract violation. [U2][U7]

## 3. Make native configuration authoritative

For the chosen real application, first find the unit or launcher that actually owns execution. Prefer a targeted native unit/drop-in change. Use existing names, aliases, slices, and activation. Do not wrap an activation client and claim the server is governed. Do not create a second lifecycle owner. [U4][U5]

Use the pinned NixOS module's supported merge/drop-in mechanism. Verify generated unit content: adding policy to a package-provided unit must not replace its executable, dependencies, sandboxing, or service type. Reuse existing `systemd.user.services`, `systemd.user.slices`, or corresponding system-unit configuration where appropriate. Avoid a second `programs.amc` hierarchy that merely duplicates native options.

Retain an existing private TOML parser, profile selector, and CLI when useful for fixtures or compatibility. Do not rewrite them gratuitously or advertise them as an upstream standard. New normal-launch documentation should use native settings. Legacy settings must enter an explicit compatibility/test path with provenance; they must not silently override native policy. Do not require the old byte parser to validate all legal native systemd values.

Test actual configuration precedence, including applicable drop-ins versus explicitly submitted transient properties on the pinned version. Do not invent a vendor/admin/user merge engine or claim a source file is the effective result. Avoid supplying optional defaults or `infinity` values that mask existing configuration. Keep fixture policy and real application policy separate. [U6]

Do not install global `app-`/`service.d` overrides, globally replace commands on PATH, or reparent existing applications just to simplify testing. Existing test slices may remain test-specific.

Application IDs are configuration labels, not authentication. Document that same-user integration addresses accidental overload; it is not a sandbox against equivalent user-manager privileges. Respect explicit delegation and the single-writer model. Report relevant administrator-owned ancestors without automatically modifying them. [U3]

## 4. Preserve launch and activation behavior

For explicit transient-service fixtures, retain literal argv handling, disabled systemd argument environment expansion, safe unit names, and controlled environment transfer. Use `Type=exec` only where appropriate for a newly owned service; do not replace existing `Type=dbus` or `Type=notify`. [U1][U8]

For native or D-Bus activation, preserve the registered server, singleton behavior, files/URIs, desktop actions, activation tokens, sandbox, and existing lifecycle. Opening an already running application must not create a duplicate or silently change its limits. Existing-instance activation is not a fresh pre-execution policy application. [U5]

Remove claims that scopes are inherently racy. The requirement is that target workload code is not released before configured placement. Retain the current service path if adequate; do not implement scope synchronization merely for completeness. Any supported scope path must demonstrate waiting, successful registration, release, cancellation, and cleanup. Source-ordering review plus early target/descendant reports is required; a report at `main()` alone does not prove what happened earlier.

Make launch uncertainty explicit. Allocate one unit identity per attempted transient launch. If submission times out or its outcome is ambiguous, reconcile that unit/job before taking further action. Do not retry under a new name or launch an unrestricted fallback. If reconciliation remains unavailable, report an unknown outcome and recovery instructions; do not claim nothing started. Clean up only units owned by that attempt. Cover accepted-request/lost-response and cancellation with fault-injection tests.

Strict test launches must reject missing required enforcement before releasing the workload. Establish this on the supported launch path; post-start detection alone is not equivalent. If a path cannot meet that ordering, mark strict launch unsupported. An optional integration may preserve ordinary launching without containment only when explicitly reported as such, never as silent success.

Do not present PTY/pipe support as transparent shell integration. Test the terminal paths actually supported: literal arguments, cwd, environment, stdin/stdout/stderr, exit status, Ctrl-C, resize, cancellation, and relevant job-control/pipeline behavior. Record unsupported behavior; do not redesign the shell to hide it.

## 5. Upgrade diagnostics and evidence handling

Adapt existing `doctor`, `explain`, and `inspect` rather than adding another management service.

Separate requested, manager-reported, and kernel-observed settings. Before a unit exists, label effective values unverified. Report unit identity/aliases, fragment and drop-in paths, manager context, cgroup path, and relevant ancestry. Include available high/max/swap/low/min settings, OOM grouping and policy, delegation, and relevant pressure-manager configuration. Distinguish required capabilities from optional counters or convenience APIs.

Read available memory/swap usage, peaks, event counters and PSI with timestamps. Mark absent, inaccessible, stale, or disappeared data as unknown with a reason—not zero or pass. Inspect ancestors as well as leaves; do not reduce hierarchical reclaim protection to a simplistic minimum-of-values formula.

Collect evidence outside the tested failure domain while the workload runs, retaining the last readable snapshots, counter deltas, exit/result information, and relevant journal evidence before cleanup. Do not assume `--retain-unit` or omitting `--collect` preserves cgroup counters. Correct that option's documentation and tests. Do not infer OOM solely from exit status 137, or require an optional recent systemd counter when another supported evidence source is available.

Do not log full argv, environment values, API credentials, or command contents by default. Use bounded/redacted reports. Any optional raw logs must be private, clearly marked, and excluded from automatic publication.

Expose known external execution domains explicitly: OpenCode frontend/backend, ordinary descendants, Nix/Lix builders, containers, and remote work where identified. Do not attempt generic IPC causality tracking. A configured Nix client is not evidence that its daemon's builders are configured. Leave a pre-existing whole-pool experiment separately opt-in; do not implement or enable it as a prerequisite for this upgrade. [U9]

## 6. Rebuild the proof around comparisons, not a single kill

Keep ordinary cargo tests and project flake checks inexpensive. Put VM and stress experiments behind separate explicit targets. Reuse the pinned toolchain and fixtures, run targeted checks first, and limit build/test concurrency. Do not run a full workstation-wide flake evaluation as the first test.

Use one modest disposable NixOS VM with explicit RAM/swap limits, finite workloads, an outer timeout, cleanup, and a watchdog outside the tested cgroup. Account for VM/build overhead in the host budget. Destructive pressure tests must be VM-only by default. A local proof command should default to inspection/harmless checks; any retained host stress mode requires explicit opt-in and verified bounds. Do not disable host protections to make a test pass.

Implement these three arms:

- A: the unchanged launch path in the disposable VM, with finite workload and outer safety bounds.
- B: native unit configuration or direct `systemd-run`, independent of AMC launch/resolver code.
- C: the proposed integration with the same workload and effective policy as B.

Compare B/C limits, ancestors, lifecycle settings, and swap configuration before interpreting results. Additional max-only versus high/max runs are a separate experimental factor. Do not silently give C stronger limits. If C is simply native configuration plus diagnostics, report that honestly; a custom launcher is not required to make C exist.

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

Predetermine fixture-specific thresholds, repetitions, and workloads before running; preserve raw results. Do not tune thresholds retrospectively to manufacture a pass. An unchanged baseline that experiences no pressure cannot demonstrate an improvement under pressure. A surviving heartbeat is a mechanism smoke test, not proof of OpenCode responsiveness.

## 7. Demonstrate normal launching without disrupting the host

Prepare one targeted NixOS integration for the actual discovered application owner. OpenCode is preferred, but do not assume its installation or client/server layout. Show exact configuration, the user's unchanged launch command or desktop action, observed owning unit, and execution coverage.

Validate this path in isolation first. Do not restart the active coding-agent session, live OpenCode backend, user manager, compositor, or Nix daemon. Do not run `nixos-rebuild switch/test`, activate Home Manager changes, or change live limits automatically. Produce the reviewed diff, explicit activation instructions, and rollback instructions for the user.

Prepare a bounded application-level trial using a disposable session and no paid requests or state-changing agent tasks. Record latency and completed work where observable. If access or instrumentation is unavailable, finish the independent migration/tests and mark real-workload benefit NOT MEASURED. Do not substitute a synthetic result for this missing evidence.

## Deliverables and stopping rule

Return focused implementation changes, migration notes, exact commands, changed-file list, pinned versions/capabilities, test artifacts, and NixOS enable/rollback instructions. Keep old reports as historical evidence; do not overwrite them as though they describe the new revision.

Report separately:

- mechanism correctness;
- configuration/activation compatibility;
- ordinary-launch coverage;
- measured responsiveness and useful work;
- incremental resource cost;
- brokered-work limitations.

Use PASS, FAIL, SKIPPED, or NOT MEASURED with reasons. Distinguish implemented, executed, and demonstrated. A skipped VM run is not a pass. Do not claim upstream approval, OOM impossibility, zero overhead, or that OpenCode is fixed without relevant measurements.

Complete the smallest coherent upgrade. If native configuration is sufficient, stop adding architecture and document that result. If one path is blocked, complete independent fixes and report its reproducer plus the smallest next patch. Do not respond to a blocker by adding a daemon, global wrapper, heuristic killer, broad compatibility layer, or repository rewrite.

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
