let
  executionArgs = import ./execution-args.nix;
  builderSettings = import ./nix-builder-settings.nix;
  contract = {
    slice = "workloads.slice";
    memory_max = 536870912;
    memory_swap_max = 134217728;
  };
  native = executionArgs {inherit contract;};
  admitted = executionArgs {
    inherit contract;
    admission = true;
    contractName = "worker";
  };
  builder = {
    memoryMaxBytes = 17179869184;
    memorySwapMaxBytes = 2147483648;
    daemonMemoryMaxBytes = 4294967296;
    daemonReserveBytes = 536870912;
    admission = {
      parent = "/sys/fs/cgroup/background.slice";
      availableMinBytes = 25769803776;
      waitSeconds = 300;
      pressureSeconds = 30;
      pressurePercent = 10;
      memoryPsiMax = 5;
      ioPsiMax = 20;
      storeFreeMinBytes = 21474836480;
    };
  };
  disabled = builderSettings builder;
  enabled = builderSettings (builder // {admission = builder.admission // {enable = true;};});
in
  assert native
  == [
    "exec"
    "--slice"
    "workloads.slice"
    "--memory-max"
    "536870912"
    "--memory-swap-max"
    "134217728"
    "--runtime-max-sec"
    "1800"
  ];
  assert admitted == ["admission" "exec" "--contract" "worker" "--runtime-max-sec" "1800"];
  assert builtins.elemAt (executionArgs {
    inherit contract;
    runtimeMaxSec = 86400;
  })
  8
  == "86400";
  assert !(builtins.tryEval (executionArgs {
    inherit contract;
    runtimeMaxSec = 0;
  })).success;
  assert !(builtins.tryEval (executionArgs {
    inherit contract;
    runtimeMaxSec = 86401;
  })).success;
  assert disabled.build-cgroup-parent == "";
  assert !(disabled ? build-cgroup-available-min);
  assert enabled.build-cgroup-parent == builder.admission.parent;
  assert enabled.build-cgroup-available-min == 25769803776;
  assert enabled.build-cgroup-wait-seconds == 300;
  assert enabled.build-cgroup-pressure-seconds == 30;
  assert enabled.build-cgroup-pressure-percent == 10;
  assert enabled.build-cgroup-memory-psi-max == 5;
  assert enabled.build-cgroup-io-psi-max == 20;
  assert enabled.build-cgroup-store-free-min == 21474836480;
  assert builtins.removeAttrs enabled [
    "build-cgroup-parent"
    "build-cgroup-available-min"
    "build-cgroup-wait-seconds"
    "build-cgroup-pressure-seconds"
    "build-cgroup-pressure-percent"
    "build-cgroup-memory-psi-max"
    "build-cgroup-io-psi-max"
    "build-cgroup-store-free-min"
  ]
  == builtins.removeAttrs disabled ["build-cgroup-parent"]; true
