{ pkgs, amcModule, amcPackage }:

# Mechanisms and small A/B/C smoke tests only. Pressure comparisons are gated
# on an executed mechanism pass, not on this target merely evaluating.
let
  nativeFixture = pkgs.writeTextDir "lib/systemd/user/amc-native.service" ''
    [Unit]
    Description=Disposable native lifecycle fixture
    After=basic.target
    [Service]
    Type=simple
    Slice=app.slice
    ExecStart=${pkgs.coreutils}/bin/sleep 120
    Restart=on-failure
    Delegate=yes
    OOMPolicy=continue
    Environment=AMC_FIXTURE_SENTINEL=preserved
  '';
in
pkgs.testers.runNixOSTest {
  name = "amc-generic";
  globalTimeout = 600;
  nodes.machine = {
    imports = [ amcModule ];
    programs.amc = {
      enable = true;
      package = amcPackage;
      settings = {
        version = 1;
        profiles.proof = {
          slice = "app-amc.slice";
          memory_max = "256MiB";
          memory_swap_max = "0B";
        };
        profiles.proof-high = {
          slice = "app-amc.slice";
          memory_max = "256MiB";
          memory_swap_max = "0B";
          memory_high = "192MiB";
        };
        applications.proof.profile = "proof";
      };
    };
    environment.systemPackages = [ pkgs.busybox pkgs.python3 ];
    environment.etc."amc-native-executable".text = "${pkgs.coreutils}/bin/sleep";
    # The package mechanism only scans etc/systemd/user and lib/systemd/user
    # (nixpkgs nixos/lib/systemd-lib.nix), so the fixture fragment lives at
    # the lib path and is registered via systemd.packages; the memory limits
    # arrive through the declarative asDropin this integration documents.
    systemd.packages = [ nativeFixture ];
    systemd.user.units."amc-native.service" = {
      overrideStrategy = "asDropin";
      text = ''
        [Service]
        MemoryHigh=192M
        MemoryMax=256M
      '';
    };
    users.users.amc-test = { isNormalUser = true; linger = true; uid = 1000; };
    # Only the disposable guest: prevent its test-harness panic_on_oom default
    # from turning intentional memcg OOM into a panic.
    boot.kernel.sysctl."vm.panic_on_oom" = 0;
    virtualisation.memorySize = 1024;
    virtualisation.cores = 2;
    swapDevices = [ ];
    zramSwap.enable = false;
  };
  testScript = builtins.readFile ./vm-test.py;
}
