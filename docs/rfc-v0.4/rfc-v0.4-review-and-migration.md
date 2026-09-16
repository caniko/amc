# Review of RFC 0.3 and migration to revision 0.4

**Reviewed:** `rfc-application-memory-contracts-v0.3-full.md` and the related `amc-v0.1-build-prompt.md`.  
**Replacement:** `rfc-application-memory-policy-v0.4.md`.  
**Review type:** Design and interface review against primary upstream documentation and selected source. No local NixOS tests, performance measurements, or maintainer review were performed.

## Recommendation

Retain the goal of ordinary application launching with predictable memory policy. Replace the proposed cross-desktop contract standard with a narrowly scoped integration RFC: use existing units and activation, prove the exact gaps, and upstream small fixes where those gaps belong.

This is a change to the proposed public abstraction, not a rejection of the project or a requirement to discard working code. A Rust tool and NixOS module can still be useful as a test harness, configuration generator, and diagnostic aid. Their value must be evaluated against an equivalent native systemd configuration, not merely against an unlimited workload.

No document review establishes that this approach fixes the reported OpenCode freeze. That requires measurements on the actual execution domains and workload.

## Findings addressed

| Finding in v0.3 or its build prompt | Revision 0.4 change |
|---|---|
| Abstract claims a missing common model and immediately proposes standardization. | Treat the gap as a hypothesis. Start with documented launch behavior, test evidence, and targeted integration fixes. |
| A parallel contract vocabulary maps back onto existing unit properties. | Use native property names and units as the policy representation; no public TOML schema or new precedence hierarchy. |
| New `PressureAware` and `RestartSafe` declarations are introduced without demonstrated consumers. | Remove them from the initial proposal. Use existing pressure interfaces and application-specific lifecycle configuration. |
| Package-supplied numeric settings are rejected categorically. | Distinguish configurable vendor defaults from enforceable administrator constraints; do not prohibit useful native unit defaults. |
| Application ID provenance is presented as a route to privileged policy. | Treat identity as configuration selection and diagnostics, not authentication. Do not invent package attestation. |
| A user helper is expected to enforce an administrator policy envelope. | State the same-user threat boundary and use administrator-owned ancestors where enforcement is required. |
| Services are treated as the universal path; scopes are associated too broadly with races. | Prefer manager-started services where appropriate, but permit correctly synchronized scopes and preserve lifecycle ownership. |
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

The governing upstream references are the desktop integration guidance, cgroup ownership guidance, resource-control manual, service manual, unit manual, pressure protocol, and Desktop Entry Specification. These are linked individually in the replacement RFC. Important semantic corrections are detailed below.

## Why these changes matter

### Native configuration is a stronger starting point

Unit files and drop-ins already express resource policy. A new resolver should solve a demonstrated selection, launch, or diagnostic problem rather than reimplement those settings and their precedence. Some transient or launcher-specific paths may still have a real gap; the revised RFC asks for a failing case before selecting a new abstraction.

This also makes a successful outcome smaller: a useful contribution could be a launcher fix and regression test, with no new service or metadata standard.

### `MemoryHigh` and `MemoryMax` answer different questions

Upstream documents `MemoryHigh` as the main control and `MemoryMax` as a final defense. Excessive throttling can harm interactivity, but that is not a reason to require a capability declaration or reject reclaim for applications without a pressure handler. The revised evaluation measures both responsiveness and useful work under several policies rather than assuming that earlier termination is always better. [R1]

### Group OOM is configured through the service's OOM policy

The service documentation explicitly maps `OOMPolicy=kill` to `memory.oom.group=1`. The prior `MemoryOOMGroup` spelling was not found in the inspected upstream code search and should not be used as a required property. The actual cgroup state should still be verified. [R2]

This does not justify whole-group killing for every application. A delegated service may already have a carefully chosen internal recovery model. It is especially inappropriate to change an entire session or shared build daemon to a disposable group as an unreviewed default.

### Launch correctness includes compatibility

Moving arbitrary running workloads after the fact loses the intended start ordering. That does not make all scopes wrong: a controlled setup child can wait for configured placement. Services and scopes must be assessed using their actual launch sequence and ownership semantics. [R3]

D-Bus activation is a separate correctness constraint. Opening another application window must not create a second server merely because a memory-policy helper prefers a fresh transient service. [R4]

### Same-user policy is not a new sandbox

A local application ID or executable name does not authenticate a request. Where a user can control their user manager, the helper is principally a way to avoid accidental overload. Enforceable cross-user/system limits belong in the existing privileged hierarchy. Explicit delegation remains supported, with one writer for each owned cgroup. [R3]

### Earlier killing is not automatically more efficient

The proof needs to report useful work, failure rate, and interactive latency. A configuration that saves apparent memory by repeatedly destroying work can be worse than one that reclaims or swaps judiciously. A heartbeat is a useful watchdog, but not a substitute for measurement of the application that was reported as freezing.

## Required changes to the local implementation brief

Apply these before relying on the previous build prompt. They are a correction list, not instructions to rewrite an implementation wholesale.

1. Remove every required probe and launch argument for `MemoryOOMGroup`. For the disposable group-OOM test, use `OOMPolicy=kill` and verify `memory.oom.group`.
2. Keep explicit hard-limit values in the first fixture, but label the fixture as an isolated test. Do not infer a production recommendation to omit `MemoryHigh`, disable swap, or disable existing oomd policy.
3. Keep automatic restart off for unknown applications and agent actions. Reuse an existing native service and its lifecycle rather than wrapping an activation client.
4. If a private TOML schema or Rust resolver already exists, keep it private and avoid unnecessary churn. Do not advertise it as an upstream format or introduce the proposed capability metadata.
5. Compare the helper against both normal launching and a native systemd invocation with identical settings. A helper should demonstrate integration value beyond cgroup enforcement that already exists.
6. Audit effective settings and ancestors, not just submitted properties. Report missing facilities explicitly. Never silently downgrade a strict containment test.
7. Reconcile an uncertain start before retrying. Do not duplicate an application after a timeout or fall back to an unrestricted launch whose predecessor may be running.
8. Capture counters and termination evidence while they exist. Do not assume that omitting `--collect` preserves the cgroup after exit.
9. Keep destructive pressure tests VM-only by default and separate from cheap routine checks. Preserve host units, desktop configuration, swap, and OOM policies unless explicitly applying a reviewed opt-in change.
10. Treat Nix/Lix and other brokers as separate execution domains. Do not enable whole-daemon group killing or restart live shared services as part of the generic proof.
11. Verify the normal-command or normal-desktop-launch path separately. A test helper is not transparent shell integration.
12. End the implementation report with actual test results, unsupported cases, and the smallest justified next patch. A planned test or a skipped VM run is not a pass.

## Proposed initial upstream cover note

**Subject:** RFC: memory resource-control integration in desktop launch paths

I am investigating session stalls during memory-intensive application and build workloads. Rather than adding a memory-contract format or another OOM daemon, I would like to establish how existing application units, drop-ins, and pressure handling can be used consistently through ordinary launch paths.

The attached draft separates launch ordering from the choice of service versus scope, preserves existing activation and restart ownership, and treats brokered work as a separate resource domain. It proposes a controlled comparison between existing defaults, native unit configuration, and a minimal integration using the same settings.

At this stage I am not proposing universal limits, new desktop-entry keys, a portal, or a service-manager API. I would particularly appreciate feedback on which launcher/configuration paths already meet these requirements, and what specific reproducer would be needed before proposing an integration change.

No benchmark result or completed implementation is claimed in this draft.

## Review references

- **R1:** [systemd resource-control documentation](https://www.freedesktop.org/software/systemd/man/systemd.resource-control.html); [inspected upstream source snapshot](https://github.com/systemd/systemd/blob/9457f81485bfe8e09d45c0376fe02ebce7c15872/man/systemd.resource-control.xml).
- **R2:** [systemd service source, OOMPolicy section](https://github.com/systemd/systemd/blob/9457f81485bfe8e09d45c0376fe02ebce7c15872/man/systemd.service.xml#L1398-L1437).
- **R3:** [systemd Control Group Interfaces](https://systemd.io/CONTROL_GROUP_INTERFACE/) and [Desktop Environment Integration](https://systemd.io/DESKTOP_ENVIRONMENTS/).
- **R4:** [Desktop Entry Specification: D-Bus activation](https://specifications.freedesktop.org/desktop-entry/latest/dbus.html).
- **R5:** [systemd Resource Pressure Handling](https://systemd.io/PRESSURE/).
- **R6:** [systemd unit configuration](https://github.com/systemd/systemd/blob/main/man/systemd.unit.xml).
- **R7:** [Nix cgroup test](https://github.com/NixOS/nix/blob/master/tests/nixos/cgroups/default.nix).

The primary sources support interface behavior; the recommendations about scope and submission strategy are this review's judgments, not statements of maintainer agreement.
