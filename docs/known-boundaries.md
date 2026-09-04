# Known boundaries

## Nix daemon

On multi-user NixOS, `amc run -- nix build ...` constrains the Nix client only.
The client sends work to `nix-daemon`; derivation builders execute in the
system service hierarchy, outside the caller's user cgroup. The VM proof runs a
synthetic derivation, records `/proc/self/cgroup` from its builder, and asserts
that it is outside the AMC user unit.

```text
user.slice/.../app-amc-....service     nix client
canix.slice/.../nix-daemon.service     daemon and brokered builders (Atlas)
```

No successful generic proof changes this boundary. Contract propagation across
a broker requires a separate design.

## Nix build-pool bridge

`programs.amc.nixBuildPool` is present but cannot be enabled in v0.1. An
assertion explains that an aggregate `nix-daemon.service` cap is blocked until
a dedicated VM test proves whole-pool OOM behavior, socket recovery, and a
subsequent successful Nix request. This avoids claiming per-application
attribution and avoids risking the host's active build service.

## Other boundaries

- Only processes that execute in the transient service cgroup are governed.
- Retained empty cgroups can disappear; proof automation samples counters while
  the offender is active.
- Environment values are inherited only for an allowlist or explicit names and
  are never printed by AMC.
- The local proof detects newly failed user units but cannot prove the absence
  of every unrelated kernel event; the bounded VM is the reproducible gate.
