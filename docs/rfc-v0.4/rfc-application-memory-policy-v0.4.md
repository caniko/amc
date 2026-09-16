# RFC: Memory resource-control integration for desktop applications

## Consistent launch-time policy using existing service managers

**Revision:** 0.4  
**Date:** 2026-09-06  
**Status:** Discussion draft; documentation and integration proposal, not a new standard  
**Supersedes:** “Application Memory Contracts for Linux desktop sessions”, revision 0.3  
**Initial implementation scope:** Linux with cgroup v2 and systemd user services  
**Evidence status:** Interface and design review only. No prototype results or upstream endorsement are claimed.

## Abstract

This proposal concerns making existing memory resource controls effective through ordinary application launch paths. The motivating problem is an interactive session becoming unusable during memory-intensive work, while configuring the relevant controls requires knowledge of how that work was started and which service actually owns it.

The initial work is to document launch ownership, preserve existing activation semantics, apply selected policy before workload execution, and provide reproducible tests and diagnostics. On systemd systems, units, drop-ins, slices, resource-control properties, and existing pressure and lifecycle interfaces remain the configuration and enforcement model. A new application manifest, policy language, service manager, or desktop-wide launch broker is not proposed.

The intended user experience remains simple: configure or install a policy once, then start an application normally. The first experiment will establish which launch paths already support that experience, fix specific gaps, and measure responsiveness and useful work. Broader metadata standardization is conditional on demonstrating a requirement that existing interfaces cannot express.

## 1. Problem and proposed contribution

The initiating report describes OpenCode and other applications becoming unresponsive as memory and swap use increase. That observation is a motivation, not a diagnosis: reproducing it must distinguish memory reclaim, storage contention, application deadlock, CPU contention, and work executed by external services. PSI provides information about resource stalls, but does not identify every cause of application unresponsiveness. [1]

The proposed contribution is an integration practice with tests:

> For a supported launch path with an explicitly selected resource policy, the component responsible for starting the workload applies that policy before releasing the workload to execute, preserves its activation and lifetime semantics, and makes the effective resource domain inspectable.

This is not a promise that every allocation succeeds, that every application completes, or that memory limits establish a real-time bound on failure recovery. Responsiveness is an experimental outcome to measure.

Three outcomes are acceptable: existing unit configuration proves sufficient; a small launcher or service-manager integration fix is required; or a concrete missing interface is demonstrated. The experiment must not assume that creating a new framework is the successful outcome.

## 2. Scope and existing interfaces

The initial integration target is a systemd-managed Linux session. It is not a freedesktop requirement that all desktops or operating systems use systemd.

Existing systemd desktop guidance describes application unit naming and the `app.slice`, `background.slice`, and `session.slice` groupings. It encourages service-based launching but is explicitly a work in progress. This RFC builds on that guidance rather than presenting those conventions as a newly completed cross-desktop standard. [2]

The systemd resource-control and service interfaces already express memory limits and protection, OOM handling, and service restart policy. The pressure protocol already supplies applications with a memory-pressure endpoint and configuration. Those interfaces should be used directly where applicable. [3][4][5]

No kernel ABI, new D-Bus method, desktop-entry extension, universal resource class, mandatory library, or process-name classifier is requested by this revision. CPU, I/O, GPU, and energy policy remain outside the initial configuration proposal; their effects must nevertheless be recorded when they confound the experiment.

Earlier portal work is relevant prior art for possible component-level extensions, not an implemented contract standard or a prerequisite for this experiment. [17]

The project name `amc` may remain a local prototype name. It does not designate a proposed upstream API or namespace.

## 3. Launch ownership and placement

### 3.1 Use the existing lifecycle owner

For an application already started by a native service, add selected resource policy to that service. Do not put its activation client in a second service and claim that this contains the server.

For an existing desktop launcher that creates transient services, investigate its established naming and configuration path before adding a resolver. Preserve names and aliases that users already target with configuration. For a launch path without appropriate isolation, implement the smallest integration needed in the component that actually starts the workload.

Services, scopes, and slices have different ownership models. A service delegates process lifetime to the service manager; a scope represents externally created processes; a slice groups units. This distinction should guide integration rather than imposing one unit type on every application. [6]

### 3.2 Define ordering, not a universal syscall requirement

For a new transient service, submit execution information and required resource properties before starting the service. For a native service, use its ordinary unit configuration. With a compatible service type, `Type=exec` can improve reporting of executable startup failure; it is not an application-readiness protocol and should not replace an existing `Type=dbus` or `Type=notify` contract. [4][7]

A scope is not inherently racy. A launcher can arrange for a trusted child to wait while the manager places it in the configured scope, then release it to execute the target. Such an implementation must handle cancellation, registration failure, and child cleanup. An implementation that lets arbitrary workload code run and fork before migration does not meet this proposal's ordering requirement.

Post-start migration also does not retroactively transfer all existing memory charges. Therefore report pre-execution containment separately from best-effort attachment of an already-running process. [8]

“Before execution” refers to releasing the target workload, not to eliminating all allocations by the launcher, the manager, or a trusted setup child. No new atomic kernel operation is claimed or required.

### 3.3 Handle failed and uncertain starts

The integration must not report a strict, bounded launch as successful when a required controller or limit is unavailable. Capability checks and actual effective configuration both belong in its verification.

An ordinary desktop path may retain its existing behavior when an optional integration is unavailable, provided it does not label that launch as contained. A deliberately strict test or administrative launch should fail before releasing the target instead. These are separate operating modes, not a silent downgrade.

A timeout after submitting a launch request does not establish that nothing started. Reconcile the requested unit's state before retrying; do not create a duplicate instance or launch an unrestricted fallback after an ambiguous result.

## 4. Configuration: use units, not a parallel contract language

### 4.1 Represent the policy with existing settings

Use native units and drop-ins first. Where transient properties are necessary, use the existing resource-control names and meanings. Do not translate them into a second public vocabulary such as `hard_boundary`, `limit_action`, or `restart_action`.

A NixOS module may generate these units and settings declaratively. Its configuration schema is a distribution integration detail, not a cross-desktop interchange format. A local helper may validate or explain generated settings, but should not become an independent source of policy truth.

Application packages may ship justified unit defaults. Operators and users may change defaults through normal supported configuration. There is no blanket prohibition on package-supplied numeric limits: the relevant distinction is between a configurable default and an enforced administrative constraint.

### 4.2 Respect configuration precedence

Preserve the service manager's documented unit lookup and drop-in merging rules instead of inventing a new administrator/user/vendor precedence hierarchy. For transient units, test the interaction between submitted properties and applicable drop-ins on supported versions. Inspect the effective result rather than inferring it from the source file alone. [9]

A launcher should avoid repeatedly supplying optional values that obscure or replace existing local policy without a documented reason. Do not install broad `app-` or `service.d` overrides merely to simplify the prototype.

An application ID is useful for selecting configuration and presenting diagnostics, but is not authentication. An executable basename is a label, not a trustworthy basis for privileged treatment. Existing desktop file selection and user customization must continue to work without introducing package attestation as a prerequisite.

### 4.3 Remove capability metadata from the initial proposal

Revision 0.3 proposed `PressureAware` and `RestartSafe` metadata. Neither is necessary to test this integration.

Pressure-aware code should consume an existing toolkit or service-manager pressure interface. Configuring or receiving notifications does not prove that an application can recover enough memory. Restart behavior belongs to the application's existing lifecycle owner and its explicit configuration, not to a global boolean intended to certify safe replay. [5]

A new desktop-entry key should be proposed only with demonstrated consumers, precise semantics, and an explanation of why existing activation or unit information is insufficient. The Desktop Entry Specification already provides a process for extensions; this RFC does not allocate an experimental namespace simply in anticipation of needing one. [10]

## 5. Resource ownership, hierarchy, and authority

On systemd systems, use manager interfaces for manager-owned cgroups. A service that has received explicit delegation may manage its delegated subtree; delegation is legitimate and should not be disabled merely to simplify this project. Do not run a second writer against the same cgroup. [6]

User-session configuration is normally user-controlled. This proposal targets accidental overload and predictable operation, not containment of a hostile process with equivalent user-manager privileges. An application ID check inside a user helper cannot create a security boundary absent from the surrounding session.

Where an administrator requires an enforceable limit across a user's workloads, place that constraint in the appropriate administrator-owned ancestor and use the existing privilege and delegation model. A same-user per-application setting is not a substitute for that boundary. [3][6]

Per-unit limits are also not aggregate admission control. Several applications can each comply with their limit and still create pressure together. The deployment must identify which ancestor handles their aggregate budget and which infrastructure is outside it. Tests must inspect the entire relevant ancestry, including parent resource settings and any OOM grouping or monitoring.

Do not derive universal limits by assigning every application a fixed fraction of installed RAM. Begin with explicit experimental values and separately evaluate the cost of false-positive termination, lost throughput, and unused capacity. The long-term goal of better defaults remains open; this revision does not claim to have solved automatic sizing.

## 6. Preserve the actual resource-control semantics

The authoritative meanings remain those in the kernel and systemd documentation. In particular:

| Setting | Meaning relevant to this proposal |
|---|---|
| `MemoryLow=` | Best-effort reclaim protection, subject to the hierarchy; not preallocated memory. |
| `MemoryMin=` | Stronger reclaim protection; not a minimum successful-allocation promise. |
| `MemoryHigh=` | Reclaim and throttling threshold, not an OOM-kill threshold. |
| `MemoryMax=` | Hard limit on accounted memory, subject to documented kernel behavior. |
| `MemorySwapMax=` | Separate limit on charged swap use, not a reservation of extra RAM. |

These settings should not be renamed as guaranteed application entitlements. [3]

### 6.1 Keep `MemoryHigh` in its upstream role

Upstream recommends `MemoryHigh=` as the principal control and `MemoryMax=` as a final defense. Reclamation can benefit a workload even when the application has no explicit pressure callback. Consequently, the `hard-contained`/`cooperative-contained` capability distinction in revision 0.3 is removed. [3]

A max-only test remains useful to isolate hard-limit behavior, but is not a recommendation to disable throttling in production. The evaluation should compare that fixture with an appropriate high/max configuration and, where already deployed, existing pressure management.

For interactive workloads, excessive time spent reclaiming is an important failure outcome. Measure it and adjust policy or existing pressure handling; do not declare a configuration successful merely because the affected process remains alive.

### 6.2 Avoid guarantees the kernel does not provide

The memory controller accounts more than application heap or RSS, including page cache and substantial kernel memory. Shared charges are not equivalent to ownership of exclusive physical bytes. A hard limit may be temporarily exceeded, and some failed allocations do not invoke OOM killing. [8]

Accordingly, this RFC promises neither an exact instantaneous physical-memory ceiling nor a fixed deadline from crossing a threshold to death. Killing a process also is not a general application recovery protocol. The experiment must distinguish resource accounting, reclaim, allocation failure, OOM selection, process exit, and useful service recovery.

No universal “small swap” policy is proposed. zram stores compressed data in RAM, so its logical size must not be added to physical capacity as if it were independent memory. Record compression configuration and actual consumption; zero-filled allocation alone is not a representative compression stress test. [11]

## 7. Pressure and failure handling

### 7.1 Reuse pressure interfaces

Use the existing memory-pressure protocol or an appropriate toolkit interface. `MemoryPressureWatch=` and `MemoryPressureThresholdSec=` configure notification; they are not a delivery acknowledgement, a promise to shed a particular number of bytes, or a timeout after which termination automatically follows. [5]

Applications decide which caches or concurrency can safely be reduced. Initial integration should not invent a generic “degrade” signal, require a new portal, or introduce another global pressure watcher.

Existing systemd-oomd policy should be audited and preserved during ordinary deployment. Its pressure-driven cgroup selection is a different mechanism from a local hard-limit OOM. Tests should identify which mechanism acted and which subtree it selected. Newer optional rule interfaces are not required by this RFC. [12]

### 7.2 Preserve the intended failure domain

Select OOM behavior for the actual service, not indiscriminately for every application or ancestor. For a disposable multi-process fixture, `OOMPolicy=kill` is the documented service setting that requests group-OOM behavior through `memory.oom.group`. `KillMode=` describes service shutdown handling and is not a replacement for that OOM policy. [4][13]

Do not add the previously suggested `MemoryOOMGroup=yes` property to the prototype. Use the documented interface and verify the resulting cgroup state.

An application's own component recovery may make whole-unit killing undesirable. Respect existing `OOMPolicy` and delegation decisions unless an explicit, tested policy change is intended. In particular, do not mark an entire desktop session or shared broker as one disposable group by default.

### 7.3 Leave restart with the lifecycle owner

Preserve an existing restart policy. For an experimental application with unknown replay semantics, leave automatic restart disabled. A service configured to recover may use existing restart delay and start-rate limiting, but `Restart=on-failure` is broader than “restart after a memory-limit event”. [4][9]

Retries need application-specific justification: reconstructible inputs, handling of partially completed writes or external requests, and avoidance of duplicate work. Do not automatically replay terminal commands, package operations, or agent actions solely because their process was killed.

If an application should survive an individual worker failure, the application and its supervisor must provide that behavior. Top-level resource integration does not manufacture internal failure isolation.

## 8. Preserve activation and execution boundaries

### 8.1 Desktop and D-Bus activation

Do not change an application's activation model to make memory configuration easier. For `DBusActivatable` applications, preserve activation of the registered application and its existing server instance, including activation data. Placing the short-lived requesting process in a new unit is not equivalent to governing the application. [14]

Keep file/URI handling, desktop actions, working directory, environment, activation tokens, sandboxing, and singleton semantics intact. For a process that is already running, activation should not be misreported as a fresh pre-execution policy application. Changing its limits is a distinct administrative operation, not an automatic side effect of opening another window.

### 8.2 Terminal applications

An existing terminal application's user service is the simplest initial real-world target. When an application is instead a direct shell child, a test helper can exercise the mechanism, but that does not establish transparent shell integration.

`systemd-run` provides service, scope, PTY, and pipe modes. These are useful existing mechanisms, not proof that every terminal invocation can be replaced without observable differences. [7]

A normal-command integration must separately verify pipelines, job control, terminal ownership, signals, working directory, file descriptors, exit status, and environment. Do not rewrite shell semantics, replace commands globally on `PATH`, or impose a new PTY on all programs as part of this RFC.

### 8.3 Work submitted to another service

Resource policy follows the execution domain, not the causal chain of user requests. Nix's cgroup integration test, for example, places daemon-managed builders beneath the daemon service rather than the invoking client's user unit. [15]

A real OpenCode evaluation should therefore distinguish its own process, ordinary descendants, an independently running backend, and builds requested through Nix or another broker. Record which part is governed and which is not; do not infer whole-application containment from the client PID alone.

A broker may have its own aggregate policy. It does not automatically become safe to kill every job in that broker when one client overloads it. Per-request budgeting or context propagation needs a broker-owned design and is deferred. Any Nix daemon-pool cap should remain a separate, explicit experiment, not part of the desktop default.

## 9. Diagnostics and prototype shape

Start with the existing service manager's status, configuration, accounting, and journal interfaces. Useful evidence includes unit names and aliases, fragment/drop-in paths, effective limits, cgroup path and ancestry, pressure, and termination result. Read cgroup information when necessary; do not write manager-owned files to reconcile a competing policy model. [6][9]

An optional `amc explain` or `amc inspect` should distinguish requested settings from effective settings, present unavailable information as unknown, and identify known external execution domains. It need not discover arbitrary IPC causality. Do not log complete argument vectors, environment values, API credentials, or command contents by default.

A small Rust test/helper program is acceptable. Rust is an implementation choice, not an upstream requirement. Reuse `systemd-run` for a proof where it meets the needed semantics; use a direct manager API only when it materially simplifies the integration or fixes a measured limitation. Do not build both backends initially.

The prototype needs no additional resident policy daemon. This does not mean the operating system has no resident managers, or that testing, PTY forwarding, kernel accounting, and existing monitoring have zero cost. Measure added memory, CPU, launch latency, and dependency cost instead of claiming “zero overhead”.

## 10. Evaluation: separate mechanism, integration, and user benefit

### 10.1 Begin with a controlled, inexpensive experiment

Use one pinned NixOS configuration, a small disposable VM, and one representative application. A multi-machine hardware survey is not required before the first result. Keep costly VM and stress runs separately invocable from routine formatting, unit tests, and ordinary flake checks.

Record kernel and systemd versions, relevant capabilities, VM limits, swap/zram configuration, ancestor policy, and active OOM managers. Preserve host policy; any controlled isolation of an OOM mechanism belongs inside the disposable VM. Avoid unbounded allocation or global swap changes on the working machine. The harness needs finite workloads, a watchdog outside the tested failure domain, cleanup, and a host resource budget.

### 10.2 Compare against the correct baselines

Compare **A**, the unchanged launch path; **B**, an equivalent native unit or direct `systemd-run` invocation with explicit settings; and **C**, the proposed integration applying the same settings. Test additional high/max or existing pressure-policy configurations separately rather than changing multiple variables and attributing the result to the helper.

B establishes what existing mechanisms already achieve. C is justified only by easier normal launching, correct coverage, clearer diagnostics, or a specific compatibility fix—not by claiming to invent B's enforcement.

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

Use realistic touched working sets, not only virtually reserved address space or a tiny heartbeat. A heartbeat can detect gross scheduling stalls but cannot establish compositor or OpenCode responsiveness. PSI complements application measurements rather than replacing them. [1]

Record raw observations, repetitions, relevant percentiles and worst observed pauses, and the tested workload sizes. Predetermine experiment-specific acceptance thresholds and publish them; do not promote a threshold into a system-wide guarantee or tune it after seeing the results to obtain a pass.

### 10.4 Capture evidence before cleanup

Transient units and their cgroups may be removed after termination. Omitting `--collect` does not guarantee retention of every cgroup counter, and normal successful units may still be collected. Capture required state while available and preserve journal evidence before cleanup. [7][9]

A killed workload is not, by itself, a successful user-experience test. Nor is the absence of global OOM sufficient if useful work is repeatedly discarded. Report mechanism conformance, launch compatibility, measured responsiveness, and resource efficiency as separate results.

## 11. Incremental upstream path

**First submission:** a focused discussion and, where useful, a documentation/test patch describing an actual supported launch path, its policy precedence, and observed gaps. No new public API is needed to begin this work.

**Integration fixes:** send small patches to the component that owns the defect—application service, launcher, service manager, or distribution configuration. Each should include a reproducer and a regression test. Keep default-policy experiments opt-in until normal workload regressions are understood.

**Additional metadata or APIs:** propose one only after independent consumers demonstrate the same unmet requirement. Specify the owner, lifecycle, authorization, versioning, and fallback behavior. Do not reserve a future portal or require all launchers to adopt a common library in advance.

For systemd, use its contribution guidance: concrete reproduction and versions for bugs/RFEs, tested changes, and the receiving project's coding and compatibility conventions. A Rust helper does not obligate another project to accept Rust dependencies or its configuration model. [16]

## 12. Questions for upstream reviewers

1. Can documented unit naming, existing drop-ins, and current activation paths provide the required normal-launch behavior? Which specific path still cannot?
2. Where should an identified placement or policy-precedence defect be fixed, and what regression test would demonstrate the fix without changing application semantics?
3. Is there a demonstrated information gap that requires new application metadata, rather than better use of existing units, pressure handling, or lifecycle ownership?

There is deliberately no request to approve universal memory budgets, a new manifest, automatic restart safety, or a new Linux application resource standard in this revision.

## Appendix A. Illustrative hard-limit fixture

The following is a resource/lifecycle fragment for a **disposable test service**, not a complete unit or desktop default:

```ini
[Service]
MemoryAccounting=yes
MemoryMax=256M
MemorySwapMax=0
OOMPolicy=kill
Restart=no
```

Supply the actual fixture executable through the test's existing launch mechanism. Verify controller availability, effective settings, and ancestor behavior before fault injection. The figures isolate a hard-limit test and are not proposed limits for OpenCode or any other real application.

Test an independently selected `MemoryHigh=` configuration in a separate run. Do not conclude from this fixture that swap should be disabled globally, that every application should be killed as a group, or that a max-only policy is generally optimal.

## References

References document existing behavior; recommendations and test requirements in this RFC are proposals. systemd source references below are pinned to inspected commit `9457f81485bfe8e09d45c0376fe02ebce7c15872` where used. That development snapshot is not a minimum runtime requirement. Documentation pages can change; record actual deployed versions in every experiment.

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
