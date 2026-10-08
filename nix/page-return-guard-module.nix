{lib, ...}: {
  boot.kernelPatches = [
    {
      name = "amc-page-return-mm-guard-v1";
      patch = ./kernel/amc-page-return-guard.patch;
      extraStructuredConfig.MEMCG = lib.kernel.yes;
    }
    {
      name = "amc-page-return-bounded-faults-v1";
      patch = ./kernel/amc-page-return-bounded-faults.patch;
    }
  ];
}
