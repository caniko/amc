{
  config,
  lib,
  ...
}: let
  series = lib.versions.majorMinor config.boot.kernelPackages.kernel.version;
in {
  assertions = [
    {
      assertion = builtins.elem series ["6.18" "7.2"];
      message = "AMC page return requires a supported, natively qualified kernel series (6.18 or 7.2).";
    }
  ];
  boot.kernelPatches = [
    {
      name = "amc-page-return-mm-guard-v1";
      patch = ./kernel/amc-page-return-guard.patch;
      extraStructuredConfig.MEMCG = lib.kernel.yes;
    }
    {
      name = "amc-page-return-bounded-faults-v1";
      patch =
        if series == "7.2"
        then ./kernel/amc-page-return-bounded-faults-7.2.patch
        else ./kernel/amc-page-return-bounded-faults.patch;
    }
  ];
}
