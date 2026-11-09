# Settings for the patched Nix execution owner. This does not patch/install Nix
# or choose fleet budgets, cgroup paths, packages or service placement.
{
  memoryMaxBytes,
  memorySwapMaxBytes,
  daemonMemoryMaxBytes,
  daemonReserveBytes,
  memoryHighBytes ? 0,
  compilerCores ? 0,
  admission ? {},
}: let
  enabled = admission.enable or false;
in
  assert !enabled || admission.parent != "";
    {
      experimental-features = ["cgroups"];
      use-cgroups = true;
      build-cgroup-memory-high = memoryHighBytes;
      build-cgroup-memory-max = memoryMaxBytes;
      build-cgroup-swap-max = memorySwapMaxBytes;
      build-cgroup-cpu-cores = compilerCores;
      daemon-cgroup-memory-min = daemonReserveBytes;
      daemon-cgroup-memory-max = daemonMemoryMaxBytes;
      # Empty disables the start gate, never native memory/swap containment.
      build-cgroup-parent =
        if enabled
        then admission.parent
        else "";
    }
    // (
      if enabled
      then {
        build-cgroup-available-min = admission.availableMinBytes;
        build-cgroup-wait-seconds = admission.waitSeconds;
        build-cgroup-pressure-seconds = admission.pressureSeconds;
        build-cgroup-pressure-percent = admission.pressurePercent;
        build-cgroup-memory-psi-max = admission.memoryPsiMax;
        build-cgroup-io-psi-max = admission.ioPsiMax;
        build-cgroup-store-free-min = admission.storeFreeMinBytes;
      }
      else {}
    )
