# Native containment and optional admission

The flake exports `lib.executionArgs` and `lib.nixBuilderSettings`
(`lib.executionPolicyVersion = 1`). They are pure policy projections: importing
them does not start/stop a service, submit work, change a ledger or replay jobs.

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

Append the returned argv to the AMC executable, then `--` and the payload argv.
Shell consumers must use `lib.escapeShellArgs`, not concatenate unquoted values.
`admission = false` selects `amc exec` with the supplied hard limits; `true`
selects `amc admission exec --contract worker`. In admitted mode the coordinator's
named contract owns the memory/swap policy. Both modes retain a finite runtime.
An unavailable coordinator is an error, never an automatic native-mode fallback.

## Patched Nix package

`amc.lib.mkNixBuilderPackage {inherit pkgs;}` returns `pkgs.nix` with the
builder-cgroup and host-admission patches from `nix/nix-builder/`. The consumer
assigns it to `nix.package`. The patches target the Nix 2.34 source; another
minor series fails evaluation until they are rebased.

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

The example numbers are illustrative, not universal recommendations. When off,
the helper emits an empty `build-cgroup-parent` and omits start-gate thresholds.
When on, it restores the supplied thresholds. Native RAM/swap/daemon ceilings
are identical in both modes. These settings require the patched Nix execution
owner; this helper does not provide that patch or enable the blocked
`programs.amc.nixBuildPool` bridge.

Consumers own package selection, measured budgets, cgroup placement and feature
enablement. AMC owns argument/setting projection. Root shared-host admission,
user admission, supervision and harness-level process governors are independent;
one toggle must not silently enable another. Persistent policy changes still
require normal qualification and drain/reconciliation before activation.

`nix/test-execution-policy.nix` verifies both states, unchanged native ceilings
and finite runtime bounds. It is evaluated by `checks.<system>.execution-policy`.
