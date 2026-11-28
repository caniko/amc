# RFC: Memory resource-control integration for desktop applications

## Consistent launch-time policy using existing service managers

**Revision:** 0.4  
**Date:** 2026-09-06  
**Status:** Discussion draft; documentation and integration proposal, not a new standard  
**Supersedes:** “Application Memory Contracts for Linux desktop sessions”, revision 0.3  
**Initial implementation scope:** Linux with cgroup v2 and systemd user services  
**Evidence status:** Interface and design review only. No prototype results or upstream endorsement are claimed.

## Abstract

Make existing memory controls work through ordinary application launches. Memory-intensive work can make an interactive session unusable, yet configuring controls requires knowing how the work started and which service owns it.

First, document launch ownership, preserve activation semantics, apply selected policy before execution, and provide reproducible tests and diagnostics. On systemd, keep units, drop-ins, slices, resource-control properties, and existing pressure and lifecycle interfaces for configuration and enforcement. No new application manifest, policy language, service manager, or desktop-wide launch broker.

The intended experience: configure or install policy once, then launch normally. The first experiment will identify working paths, fix specific gaps, and measure responsiveness and useful work. Standardize broader metadata only after showing a requirement existing interfaces cannot express.

## 1. Problem and proposed contribution

The original report describes OpenCode and other applications becoming unresponsive as memory and swap use rise. That motivates the experiment but establishes no cause. Reproduction must distinguish memory reclaim, storage contention, application deadlock, CPU contention, and work done by external services. PSI shows resource stalls, not every cause of unresponsiveness. [1]

The proposed contribution is an integration practice with tests:

> For a supported launch path with an explicitly selected resource policy, the component responsible for starting the workload applies that policy before releasing the workload to execute, preserves its activation and lifetime semantics, and makes the effective resource domain inspectable.

This guarantees neither successful allocations, application completion, nor a real-time recovery bound from memory limits. Responsiveness is a measured outcome, not a guarantee.

Three outcomes are acceptable: existing unit configuration suffices, a small launcher or service-manager fix is needed, or the experiment demonstrates a missing interface. Do not treat creating a new framework as the criterion for success.

## 2. Scope and existing interfaces

Start with systemd-managed Linux sessions. Choosing this target imposes no freedesktop requirement that all desktops or operating systems use systemd.

Systemd's desktop guidance describes application unit naming and the `app.slice`, `background.slice`, and `session.slice` groupings. It encourages service-based launches but explicitly remains a work in progress. This RFC builds on that guidance, not a finished cross-desktop standard. [2]

Use existing interfaces directly where applicable: systemd resource-control and service settings for memory limits and protection, OOM handling, and restart policy; the pressure protocol for an application's memory-pressure endpoint and configuration. [3][4][5]

This revision requests no kernel ABI, new D-Bus method, desktop-entry extension, universal resource class, mandatory library, or process-name classifier. CPU, I/O, GPU, and energy policy are outside the initial configuration proposal; record their effects when they confound the experiment.

Earlier portal work is prior art for possible component extensions, not an implemented contract standard or an experimental prerequisite. [17]

Keep `amc` as a local prototype name if useful, not a proposed upstream API or namespace.

## 3. Launch ownership and placement

### 3.1 Use the existing lifecycle owner

If a native service already starts the application, apply selected policy there. Putting its activation client in a second service does not contain the server.

If a desktop launcher creates transient services, check its naming and configuration before adding a resolver. Preserve names and aliases targeted by existing user configuration. If isolation is missing or inadequate, make the smallest needed change in the component that starts the workload.

A service delegates process lifetime to the manager; a scope represents externally created processes; a slice groups units. Choose by ownership rather than imposing one unit type on all applications. [6]

### 3.2 Define ordering, not a universal syscall requirement

Submit execution information and required resource properties before starting a new transient service. For native services, use ordinary unit configuration. Where compatible, `Type=exec` can better report executable startup failure; it does not signal application readiness, and do not replace an existing `Type=dbus` or `Type=notify` contract. [4][7]

A scope need not race: for example, a launcher can hold a trusted child until the manager places it in the configured scope, then release it to execute the target. Handle cancellation, registration failure, and child cleanup. Letting arbitrary workload code run and fork before migration violates the required ordering.

Post-start migration does not transfer all existing memory charges. Report pre-execution containment separately from best-effort attachment to a running process. [8]

“Before execution” means before releasing the target, not before any allocation by the launcher, manager, or trusted setup child. This neither claims nor requires a new atomic kernel operation.

### 3.3 Handle failed and uncertain starts

Do not report strict, bounded launch success if a required controller or limit is unavailable. Verify both capabilities and effective configuration.

If optional integration is unavailable, ordinary desktop launches may behave as before but must not claim containment. Strict test or administrative launches should fail before releasing the target. Keep these modes distinct; never silently downgrade.

A submitted request can start despite a timeout. Reconcile the requested unit's state before retrying; no duplicate instance or unrestricted fallback after an ambiguous result.

## 4. Configuration: use units, not a parallel contract language

### 4.1 Represent the policy with existing settings

Use native units and drop-ins first. If transient properties are needed, use existing resource-control names and meanings, not a second public vocabulary like `hard_boundary`, `limit_action`, or `restart_action`.

A NixOS module may generate units and settings declaratively. Its schema is distribution-specific, not a cross-desktop interchange format. A local helper may validate or explain generated settings, not define independent policy.

Packages may ship justified unit defaults, including numeric limits. Operators and users may change them through supported configuration. Distinguish configurable defaults from enforced administrative constraints.

### 4.2 Respect configuration precedence

Keep documented manager lookup and drop-in merging rules, not a new administrator/user/vendor hierarchy. Test submitted transient properties against applicable drop-ins on supported versions. Inspect the effective result, not just the source file. [9]

Launchers should not repeatedly supply optional values that obscure or replace local policy without documenting why. Do not install broad `app-` or `service.d` overrides for prototype convenience.

Application IDs can help select configuration and present diagnostics, not authenticate requests. An executable basename is a label, not a basis for privilege. Preserve desktop file selection and user customization without requiring package attestation.

### 4.3 Remove capability metadata from the initial proposal

Revision 0.3 proposed `PressureAware` and `RestartSafe` metadata. Neither is necessary to test this integration.

Pressure-aware code should use an existing toolkit or service-manager pressure interface. Configuring or receiving notifications does not prove sufficient memory recovery. Leave restarts to the existing lifecycle owner and explicit configuration, not a global boolean certifying safe replay. [5]

Propose a desktop-entry key only with demonstrated consumers, precise semantics, and a reason existing activation or unit information falls short. Use the Desktop Entry Specification's extension process; this RFC reserves no speculative namespace. [10]

## 5. Resource ownership, hierarchy, and authority

On systemd, use manager interfaces for manager-owned cgroups. Explicitly delegated services may manage their subtrees; do not disable legitimate delegation for convenience or add a second writer to a cgroup. [6]

Users normally control their session configuration. This proposal addresses accidental overload and predictable operation, not hostile processes with equivalent user-manager privileges. A user helper's application ID check cannot add a security boundary the session lacks.

If an administrator needs enforceable limits across a user's workloads, set them at the appropriate administrator-owned ancestor using existing privilege and delegation. Same-user per-application settings cannot replace that boundary. [3][6]

Per-unit limits are not aggregate admission control: applications within their limits can still create pressure together. Identify the budget-owning ancestor and, separately, the infrastructure outside its budget. Test the full relevant ancestry, including parent resource settings and any OOM grouping or monitoring.

Do not give every application a fixed share of installed RAM as a universal limit. Start with explicit experimental values; separately measure false-positive termination, lost throughput, and unused capacity costs. Better defaults remain a goal; automatic sizing is unsolved here.

## 6. Preserve the actual resource-control semantics

The kernel and systemd documentation remain authoritative:

| Setting | Meaning relevant to this proposal |
|---|---|
| `MemoryLow=` | Best-effort reclaim protection, subject to the hierarchy; not preallocated memory. |
| `MemoryMin=` | Stronger reclaim protection; not a minimum successful-allocation promise. |
| `MemoryHigh=` | Reclaim and throttling threshold, not an OOM-kill threshold. |
| `MemoryMax=` | Hard limit on accounted memory, subject to documented kernel behavior. |
| `MemorySwapMax=` | Separate limit on charged swap use, not a reservation of extra RAM. |

Do not rename these settings as guaranteed application entitlements. [3]

### 6.1 Keep `MemoryHigh` in its upstream role

Upstream recommends `MemoryHigh=` as the main control and `MemoryMax=` as the last defense. Reclaim can help even without an explicit application pressure callback, so drop revision 0.3's `hard-contained`/`cooperative-contained` capability distinction. [3]

A max-only fixture isolates hard-limit behavior; it does not justify disabling production throttling. Compare it with appropriate high/max settings and any already-deployed pressure management.

Excessive reclaim time is a failure for interactive workloads. Measure it and adjust policy or existing pressure handling; process survival alone is not success.

### 6.2 Avoid guarantees the kernel does not provide

The controller accounts for more than application heap or RSS: it includes page cache and substantial kernel memory. Shared charges do not imply exclusive ownership of physical bytes. Usage can temporarily exceed a hard limit, and some allocation failures do not trigger OOM killing. [8]

This RFC promises neither an exact instantaneous physical-memory ceiling nor a fixed deadline from threshold crossing to death. Killing is not a general recovery protocol. Distinguish resource accounting, reclaim, allocation failure, OOM selection, process exit, and useful service recovery.

This RFC proposes no universal “small swap” policy. zram stores compressed data in RAM; do not add its logical size to physical capacity. Record compression settings and actual consumption; zero-filled allocation alone is not a representative compression stress test. [11]

## 7. Pressure and failure handling

### 7.1 Reuse pressure interfaces

Use the existing memory-pressure protocol or an appropriate toolkit interface. `MemoryPressureWatch=` and `MemoryPressureThresholdSec=` configure notifications, not delivery acknowledgements, byte-shedding guarantees, or automatic termination timeouts. [5]

Applications decide which caches or concurrency they can safely reduce and by how much. Start without a generic “degrade” signal, new portal requirement, or another global pressure watcher.

Audit and preserve existing systemd-oomd policy during ordinary deployment. Its pressure-driven cgroup selection differs from local hard-limit OOM. Tests should identify the mechanism and selected subtree. This RFC requires no newer optional rule interfaces. [12]

### 7.2 Preserve the intended failure domain

Select OOM behavior for the actual service, not indiscriminately for every application or ancestor. For a disposable multi-process fixture, the documented `OOMPolicy=kill` service setting requests group-OOM through `memory.oom.group`. `KillMode=` governs service shutdown, not OOM policy. [4][13]

Do not add the earlier `MemoryOOMGroup=yes` suggestion. Use the documented interface and verify cgroup state.

An application may recover individual components, making whole-unit killing undesirable. Preserve existing `OOMPolicy` and delegation unless explicitly changing and testing them. Never default an entire desktop session or shared broker to one disposable group.

### 7.3 Leave restart with the lifecycle owner

Keep existing restart policy. For experimental applications with unknown replay semantics, leave automatic restart disabled. A service configured for recovery may use existing restart delays and start-rate limits, but `Restart=on-failure` covers more than memory-limit events. [4][9]

Justify retries per application: inputs must be reconstructible, partial writes or external requests handled, and duplicate work avoided. Do not replay terminal commands, package operations, or agent actions automatically just because their process was killed.

If an application must survive an individual worker failure, it and its supervisor must provide that behavior. Top-level resource integration does not isolate internal failures.

## 8. Preserve activation and execution boundaries

### 8.1 Desktop and D-Bus activation

Do not change activation to simplify memory configuration. For `DBusActivatable` applications, preserve activation of the registered application and its existing server, including activation data. Moving the short-lived requester to a new unit does not govern the application. [14]

Preserve file/URI handling, desktop actions, working directory, environment, activation tokens, sandboxing, and singleton semantics. Activation of a running process is not fresh pre-execution policy application. Changing its limits is a separate administrative operation, not a side effect of opening a window.

### 8.2 Terminal applications

An existing terminal application's user service is the simplest first real-world target. A test helper can exercise the mechanism for a direct shell child, but that test alone does not establish transparent shell integration.

`systemd-run` offers service, scope, PTY, and pipe modes, not proof that every terminal invocation can be replaced transparently. [7]

For normal-command integration, verify pipelines, job control, terminal ownership, signals, working directory, file descriptors, exit status, and environment separately. Do not rewrite shell semantics, replace commands globally on `PATH`, or impose a PTY on every program.

### 8.3 Work submitted to another service

Resource policy follows where work runs, not who requested it. Nix's cgroup integration test places daemon-managed builders under the daemon service, not the invoking client's user unit. [15]

Evaluate OpenCode's own process, ordinary descendants, independent backend, and builds requested through Nix or another broker separately. Record what policy covers and what runs outside it; the client PID alone cannot establish whole-application containment.

A broker may have its own aggregate policy, but one client's overload does not make it safe to kill all jobs in the broker. Defer per-request budgeting and context propagation to a broker-owned design. Keep any Nix daemon-pool cap a separate, explicit experiment, not a desktop default.

## 9. Diagnostics and prototype shape

Start with manager status, configuration, accounting, and journal interfaces. Useful evidence includes unit names and aliases, fragment/drop-in paths, effective limits, cgroup path and ancestry, pressure, and termination result. Read cgroup data as needed; do not write manager-owned files to enforce competing policy. [6][9]

Optional `amc explain` or `amc inspect` should separate requested from effective settings, mark unavailable information unknown, and identify known external execution domains. Arbitrary IPC causality need not be traced. Do not log complete argument vectors, environment values, API credentials, or command contents by default.

A small Rust test/helper is fine; Rust is not an upstream requirement. Use `systemd-run` for the proof if it meets the required semantics. Use a direct manager API only if it materially simplifies integration or fixes a measured limitation. Do not build both initially.

The prototype needs no additional resident policy daemon; OS managers still run, and testing, PTY forwarding, kernel accounting, and monitoring still cost resources. Measure added memory, CPU, launch latency, and dependency cost rather than claiming “zero overhead”.

## 10. Evaluation: separate mechanism, integration, and user benefit

### 10.1 Begin with a controlled, inexpensive experiment

Start with one pinned NixOS configuration, one small disposable VM, and one representative application. No multi-machine hardware survey is needed for the first result. Keep costly VM and stress tests separately invocable from routine formatting, unit tests, and ordinary flake checks.

Record kernel and systemd versions, relevant capabilities, VM limits, swap/zram settings, ancestor policy, and active OOM managers. Preserve host policy: isolate OOM mechanisms only in the disposable VM. Avoid unbounded allocation or global swap changes on the working machine. Require finite workloads, a watchdog outside the tested failure domain, cleanup, and a host resource budget.

### 10.2 Compare against the correct baselines

Compare **A**, unchanged launches; **B**, an equivalent native unit or direct `systemd-run` with explicit settings; and **C**, the integration applying those same settings. Test additional high/max or existing pressure-policy settings separately so effects of other changes are not attributed to the helper.

B shows what already works. Justify C only through easier normal launches, correct coverage, clearer diagnostics, or a specific compatibility fix, not by claiming B's enforcement as new.

### 10.3 Required evidence

| Question | Evidence required |
|---|---|
| Is placement established before workload execution? | Launch-path ordering review plus early target/descendant reports of membership and effective limits. A report at `main()` alone is not proof about all earlier execution. |
| Does hard containment work for the fixture? | Actual touched allocation, local event/result evidence, and charged memory/swap data; distinguish allocation failure from OOM termination. |
| Is the failure domain correct? | Multi-process test, appropriate group behavior, inspected ancestors, and survival of unrelated units. |
| Does aggregate policy work? | Multiple individually bounded workloads compete; report ancestor pressure and victims rather than testing only one offender. |
| Does activation remain correct? | Existing-server activation, arguments/files, exit status, cancellation, and no duplicate launch after simulated startup uncertainty. |
| Are limitations visible? | Missing capability and brokered-work cases fail or report reduced coverage without a false success claim. |
| Is the user experience better? | Repeated latency measurements in an unrelated interactive workload, task completion/throughput, and lost-work or unnecessary-termination counts. |

Use realistic touched working sets, not just virtual reservations or a tiny heartbeat. A heartbeat can detect gross scheduling stalls but cannot establish compositor or OpenCode responsiveness. PSI complements but cannot replace application measurements. [1]

Record raw observations, repetitions, relevant percentiles, worst observed pauses, and tested workload sizes. Set experiment-specific acceptance thresholds in advance and publish them. Do not treat them as system-wide guarantees or tune them after seeing results to get a pass.

### 10.4 Capture evidence before cleanup

Transient units and cgroups can disappear after termination. Omitting `--collect` does not guarantee retention of every counter; successful units can still be collected. Capture required state while available and preserve journal evidence before cleanup. [7][9]

Killing a workload does not prove a better user experience. Avoiding global OOM is not enough if useful work is repeatedly lost. Report mechanism conformance, launch compatibility, measured responsiveness, and resource efficiency separately.

## 11. Incremental upstream path

**First submission:** a focused discussion and, where useful, a documentation/test patch on a supported launch path, policy precedence, and observed gaps. Starting this work requires no new public API.

**Integration fixes:** send small patches with a reproducer and regression test to the owner: application service, launcher, manager, or distribution configuration. Keep default-policy experiments opt-in until normal workload regressions are understood.

**Additional metadata or APIs:** propose one only when independent consumers show the same unmet need. Specify owner, lifecycle, authorization, versioning, and fallback. Do not reserve a portal or mandate a common launcher library in advance.

Follow systemd contribution guidance: concrete reproduction and versions for bugs/RFEs, tested changes, and the receiving project's coding and compatibility conventions. A Rust helper does not oblige another project to adopt Rust dependencies or its configuration model. [16]

## 12. Questions for upstream reviewers

1. Can documented unit naming, existing drop-ins, and current activation paths provide the required normal-launch behavior? Which specific path still cannot?
2. Where should an identified placement or policy-precedence defect be fixed, and what regression test would demonstrate the fix without changing application semantics?
3. Is there a demonstrated information gap that requires new application metadata, rather than better use of existing units, pressure handling, or lifecycle ownership?

This revision seeks no approval for universal memory budgets, a new manifest, automatic restart safety, or a new Linux application resource standard.

## Appendix A. Illustrative hard-limit fixture

This resource/lifecycle fragment is for a **disposable test service**, not a complete unit or desktop default:

```ini
[Service]
MemoryAccounting=yes
MemoryMax=256M
MemorySwapMax=0
OOMPolicy=kill
Restart=no
```

Launch the actual fixture executable through the test's existing mechanism. Verify controller availability, effective settings, and ancestor behavior before fault injection. These values isolate hard-limit behavior; they are not proposed limits for OpenCode or other real applications.

Test independently chosen `MemoryHigh=` settings in a separate run. This fixture does not justify disabling swap globally, killing every application as a group, or preferring max-only policy in general.

## References

References document existing behavior; this RFC proposes recommendations and test requirements. The systemd source references below are pinned to inspected commit `9457f81485bfe8e09d45c0376fe02ebce7c15872` where used. That development snapshot sets no minimum runtime. Documentation can change; record deployed versions in every experiment.

1. [Linux Pressure Stall Information](https://docs.kernel.org/accounting/psi.html).
2. [systemd Desktop Environment Integration](https://systemd.io/DESKTOP_ENVIRONMENTS/).
3. [systemd resource-control manual/source](https://github.com/systemd/systemd/blob/9457f81485bfe8e09d45c0376fe02ebce7c15872/man/systemd.resource-control.xml).
4. [systemd service manual/source, including OOMPolicy and Restart](https://github.com/systemd/systemd/blob/9457f81485bfe8e09d45c0376fe02ebce7c15872/man/systemd.service.xml).
5. [systemd Resource Pressure Handling](https://systemd.io/PRESSURE/).
6. [systemd Control Group Interfaces](https://systemd.io/CONTROL_GROUP_INTERFACE/). Use its ownership guidance with the deployed version's current API documentation; some historical examples are older than cgroup v2.
7. [systemd-run manual/source](https://github.com/systemd/systemd/blob/main/man/systemd-run.xml).
8. [Linux cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html), especially memory interfaces and memory ownership.
9. [systemd unit manual/source](https://github.com/systemd/systemd/blob/main/man/systemd.unit.xml).
10. [Desktop Entry Specification: extending the format](https://specifications.freedesktop.org/desktop-entry/latest/extending.html).
11. [Linux zram documentation](https://docs.kernel.org/admin-guide/blockdev/zram.html).
12. [systemd-oomd manual/source](https://github.com/systemd/systemd/blob/main/man/systemd-oomd.service.xml).
13. [systemd kill manual/source](https://github.com/systemd/systemd/blob/main/man/systemd.kill.xml).
14. [Desktop Entry Specification: D-Bus activation](https://specifications.freedesktop.org/desktop-entry/latest/dbus.html).
15. [Nix cgroup integration test](https://github.com/NixOS/nix/blob/master/tests/nixos/cgroups/default.nix). Verify the corresponding test and behavior in the actual Nix or Lix version deployed.
16. [systemd contribution guidance](https://systemd.io/CONTRIBUTING/).
17. [Earlier xdg-desktop-portal cgroup proposal, issue #604](https://github.com/flatpak/xdg-desktop-portal/issues/604). Prior art for possible future component-level work; not a dependency or an implemented standard assumed here.
