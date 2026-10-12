{lib}: let
  evaluate = version:
    import ./page-return-guard-module.nix {
      inherit lib;
      config.boot.kernelPackages.kernel = {inherit version;};
    };
  old = evaluate "6.18.48";
  current = evaluate "7.2.8";
  unsupported = evaluate "7.3.0";
in
  assert builtins.all (a: a.assertion) old.assertions;
  assert builtins.all (a: a.assertion) current.assertions;
  assert !builtins.all (a: a.assertion) unsupported.assertions;
  assert builtins.length old.boot.kernelPatches == 2;
  assert builtins.length current.boot.kernelPatches == 2;
  assert (builtins.head old.boot.kernelPatches).patch == (builtins.head current.boot.kernelPatches).patch;
  assert (builtins.elemAt old.boot.kernelPatches 1).patch == ./kernel/amc-page-return-bounded-faults.patch;
  assert (builtins.elemAt current.boot.kernelPatches 1).patch == ./kernel/amc-page-return-bounded-faults-7.2.patch;
  assert builtins.all (patch: !(patch ? extraStructuredConfig)) current.boot.kernelPatches;
  assert (builtins.head current.boot.kernelPatches).structuredExtraConfig.MEMCG == lib.kernel.yes; true
