# Review of RFC 0.3 and migration to revision 0.4

**Reviewed:** `rfc-application-memory-contracts-v0.3-full.md` and the related `amc-v0.1-build-prompt.md`.  
**Replacement:** `rfc-application-memory-policy-v0.4.md`.  
**Review type:** Design and interface review against primary upstream documentation and selected source. No local NixOS tests, performance measurements, or maintainer review.

## Recommendation

Keep ordinary launches and predictable memory policy as the goal. Replace the proposed cross-desktop contract standard with a narrow integration RFC: use existing units and activation, demonstrate specific gaps, and submit small fixes to the upstream projects that own them.

Change the proposed public abstraction without rejecting the project or requiring removal of working code. A Rust tool and NixOS module can still help test, generate configuration, and diagnose problems. Evaluate them against equivalent native systemd configuration, not just an unlimited workload.

Document review alone cannot establish that the approach fixes the reported OpenCode freeze. That requires measuring the actual workload and execution domains.

## Findings addressed

| Finding in v0.3 or its build prompt | Revision 0.4 change |
|---|---|
| Abstract claims a missing common model and immediately proposes standardization. | Treat the gap as a hypothesis. Start with documented launch behavior, test evidence, and targeted integration fixes. |
| A parallel contract vocabulary maps back onto existing unit properties. | Use native property names and units as the policy representation; no public TOML schema or new precedence hierarchy. |
| Proposes `PressureAware` and `RestartSafe` without demonstrating consumers. | Drop them from the initial proposal. Use existing pressure interfaces and application-specific lifecycle settings. |
| Rejects all package-supplied numeric settings. | Distinguish configurable vendor defaults from enforceable administrator constraints; allow useful native unit defaults. |
| Treats application ID provenance as a route to privileged policy. | Use identity for configuration selection and diagnostics, not authentication. Do not invent package attestation. |
| Expects a user helper to enforce an administrator policy envelope. | State the same-user threat boundary; use administrator-owned ancestors when enforcement is required. |
| Treats services as universal and treats too many scope paths as racy. | Prefer manager-started services where appropriate; allow synchronized scopes and preserve lifecycle ownership. |
| `MemoryHigh` is effectively gated on application cooperation. | Restore upstream's normal high/max relationship; retain max-only as an isolated experiment rather than a universal default. |
| Violation implies bounded failure or recovery. | Limit the claim to configured resource semantics; measure latency and distinguish allocation failure, reclaim, OOM, exit, and recovery. |
| `on-resource-failure` implies a generic restart mode. | Remove the invented mode. Explain that `Restart=on-failure` is broader and that retries require application-specific semantics. |
| Placement in a fresh service could bypass D-Bus or singleton activation. | Preserve activation and configure the actual server owner, not the short-lived activation client. |
| “No new daemon” becomes “no steady-state overhead.” | Measure the helper, forwarding, cgroup, and monitoring costs; do not make zero-cost claims. |
| Large hardware matrix required before proving the first mechanism. | Begin with a pinned small VM and one real application; extend coverage after initial evidence. |
| Killing a memory hog and preserving a heartbeat appear sufficient for success. | Separate mechanism correctness, launch compatibility, real application responsiveness, useful work, and overhead. |
| Build prompt requires `MemoryOOMGroup=yes`. | Remove it. Use documented `OOMPolicy=kill` where group OOM is intended, and inspect `memory.oom.group`. |
| Retaining a failed unit is assumed to retain its kernel counters. | Capture evidence while available; unit state, journal records, and cgroup lifetime are distinct. |
| Nix broker-pool containment risks becoming part of the desktop solution. | Retain the external-domain limitation and make whole-pool experiments separate and explicitly opt-in. |

The replacement RFC links each governing upstream reference: desktop integration, cgroup ownership, resource control, services, units, the pressure protocol, and the Desktop Entry Specification.

## Why these changes matter

### Native configuration is a stronger starting point

Unit files and drop-ins already express resource policy. A new resolver should fix a demonstrated selection, launch, or diagnostic problem, not recreate settings and precedence. Transient or launcher-specific paths may still have gaps; the revised RFC asks for a failing case before choosing a new abstraction.

A launcher fix and regression test may suffice, without a new service or metadata standard.

### `MemoryHigh` and `MemoryMax` answer different questions

Upstream treats `MemoryHigh` as the main control and `MemoryMax` as a final defense. Excessive throttling can hurt interactivity, but does not justify requiring a capability declaration or rejecting reclaim for applications without a pressure handler. The revised evaluation calls for measuring responsiveness and useful work under several policies rather than assuming earlier termination is always better. [R1]

### Group OOM is configured through the service's OOM policy

The service documentation maps `OOMPolicy=kill` to `memory.oom.group=1`. The upstream code search did not find `MemoryOOMGroup`; do not require it. Verify actual cgroup state. [R2]

Do not assume whole-group killing suits every application. Delegated services may already have their own recovery model. In particular, do not make an entire session or shared build daemon disposable by default without review.

### Launch correctness includes compatibility

Moving arbitrary workloads after they start loses the required ordering. A scope can work if a controlled setup child waits for configured placement. Judge services and scopes by their actual launch sequence and ownership. [R3]

D-Bus activation adds a separate constraint: opening another window must not start a second server just because a memory-policy helper prefers a fresh transient service. [R4]

### Same-user policy is not a new sandbox

A local application ID or executable name does not authenticate requests. Where users can control their user manager, the helper mainly helps avoid accidental overload. Put enforceable cross-user/system limits in the existing privileged hierarchy. Keep supporting explicit delegation and one writer per owned cgroup. [R3]

### Earlier killing is not automatically more efficient

Report useful work, failure rate, and interactive latency. Repeatedly destroying work can look like a memory saving yet perform worse than judicious reclaim or swap. A heartbeat can serve as a watchdog, not as a substitute for measuring the application reported to freeze.

## Required changes to the local implementation brief

Apply these corrections before relying on the previous build prompt; they do not call for a wholesale rewrite of the implementation.

1. Remove every required probe and launch argument for `MemoryOOMGroup`. For the disposable group-OOM test, use `OOMPolicy=kill` and verify `memory.oom.group`.
2. Keep explicit hard limits in the first fixture and label it an isolated test. These settings do not recommend omitting `MemoryHigh`, disabling swap, or disabling existing oomd policy in production.
3. Keep automatic restart off for unknown applications and agent actions. Reuse an existing native service and its lifecycle rather than wrapping an activation client.
4. Leave any existing private TOML schema or Rust resolver private; avoid unnecessary changes. Do not present it as an upstream format or add the proposed capability metadata.
5. Compare the helper with both normal launching and native systemd invocation with identical settings. Show what integration adds beyond existing cgroup enforcement.
6. Audit effective settings and ancestors, not just submitted properties. Report missing facilities explicitly. Never silently downgrade a strict containment test.
7. Reconcile an uncertain start before retrying. Do not duplicate an application after a timeout or fall back to an unrestricted launch whose predecessor may be running.
8. Capture counters and termination evidence while they exist. Do not assume that omitting `--collect` preserves the cgroup after exit.
9. Keep destructive pressure tests VM-only by default and separate from cheap routine checks. Preserve host units, desktop configuration, swap, and OOM policies unless explicitly applying a reviewed opt-in change.
10. Treat Nix/Lix and other brokers as separate execution domains. Do not enable whole-daemon group killing or restart live shared services as part of the generic proof.
11. Verify the normal-command or normal-desktop-launch path separately. A test helper is not transparent shell integration.
12. End the implementation report with actual test results, unsupported cases, and the smallest justified next patch. A planned test or a skipped VM run is not a pass.

## Proposed initial upstream cover note

**Subject:** RFC: memory resource-control integration in desktop launch paths

I am investigating session stalls during memory-intensive application and build workloads. I want to see how ordinary launch paths can consistently use existing application units, drop-ins, and pressure handling without a new memory-contract format or OOM daemon.

The attached draft separates launch ordering from the service-or-scope choice, preserves activation and restart ownership, and treats brokered work as a separate resource domain. It proposes a controlled comparison of existing defaults, native unit configuration, and a minimal integration with the same settings.

I am not proposing universal limits, new desktop-entry keys, a portal, or a service-manager API at this stage. Which launcher/configuration paths already meet these requirements? What reproducer should accompany an integration proposal?

This draft claims no benchmark results or completed implementation.

## Review references

- **R1:** [systemd resource-control documentation](https://www.freedesktop.org/software/systemd/man/systemd.resource-control.html); [inspected upstream source snapshot](https://github.com/systemd/systemd/blob/9457f81485bfe8e09d45c0376fe02ebce7c15872/man/systemd.resource-control.xml).
- **R2:** [systemd service source, OOMPolicy section](https://github.com/systemd/systemd/blob/9457f81485bfe8e09d45c0376fe02ebce7c15872/man/systemd.service.xml#L1398-L1437).
- **R3:** [systemd Control Group Interfaces](https://systemd.io/CONTROL_GROUP_INTERFACE/) and [Desktop Environment Integration](https://systemd.io/DESKTOP_ENVIRONMENTS/).
- **R4:** [Desktop Entry Specification: D-Bus activation](https://specifications.freedesktop.org/desktop-entry/latest/dbus.html).
- **R5:** [systemd Resource Pressure Handling](https://systemd.io/PRESSURE/).
- **R6:** [systemd unit configuration](https://github.com/systemd/systemd/blob/main/man/systemd.unit.xml).
- **R7:** [Nix cgroup test](https://github.com/NixOS/nix/blob/master/tests/nixos/cgroups/default.nix).

The primary sources support the interface claims. This review's scope and submission recommendations do not imply maintainer agreement.
