# Native containment and optional admission

The flake exports `lib.executionArgs` and `lib.nixBuilderSettings`
(`lib.executionPolicyVersion = 1`). These pure policy projections don't
start/stop services, submit work, change ledgers or replay jobs on import.

## Workload execution

```nix
amc.lib.executionArgs {
  admission = false;
  contractName = "worker";
  contract = {
    slice = "workers.slice";
    memory_max = 536870912;
    memory_swap_max = 0;
  };
  runtimeMaxSec = 1800;
}
```

Build the command in this order: AMC executable, returned argv, `--`, payload
argv. When generating a shell command in Nix, use `lib.escapeShellArgs` rather
than joining unquoted values.
`admission = false` selects `amc exec` with the supplied hard limits; `true`
selects `amc admission exec --contract worker`, where the coordinator's named
contract owns the memory/swap policy. Both modes have a finite runtime. If the
coordinator is unavailable, admission fails without falling back to native mode.

## Patched Nix package

`amc.lib.mkNixBuilderPackage {inherit pkgs;}` returns `pkgs.nix` with the
builder-cgroup and host-admission patches from `nix/nix-builder/`. Assign it to
`nix.package`. The patches target Nix 2.34 source; other minor series fail
evaluation until the patches are rebased.

## Nix builder settings

`lib.nixBuilderSettings` takes native byte limits (`memoryMaxBytes`,
`memorySwapMaxBytes`, `daemonMemoryMaxBytes`, `daemonReserveBytes`), optional
`memoryHighBytes`/`compilerCores` (default `0`), and an `admission` record:

```nix
admission = {
  enable = false;
  parent = "/sys/fs/cgroup/background.slice";
  availableMinBytes = 25769803776;
  waitSeconds = 300;
  pressureSeconds = 30;
  pressurePercent = 10;
  memoryPsiMax = 5;
  ioPsiMax = 20;
  storeFreeMinBytes = 21474836480;
};
```

Treat these numbers as examples, not universal recommendations. With admission
off, the helper emits an empty `build-cgroup-parent` and omits start-gate
thresholds; turning admission on restores the supplied thresholds. Native
RAM/swap/daemon ceilings stay the same. The settings require the patched Nix
execution owner; the helper neither supplies the patch nor enables the blocked
`programs.amc.nixBuildPool` bridge.

Consumers own package selection, measured budgets, cgroup placement and feature
enablement. AMC owns argument/setting projection. Root shared-host admission,
user admission, supervision and harness-level process governors are independent;
no toggle may silently enable another. Before activating persistent policy
changes, complete normal qualification and drain/reconciliation.

`checks.<system>.execution-policy` evaluates `nix/test-execution-policy.nix`,
which checks both states, unchanged native ceilings and finite runtime bounds.
