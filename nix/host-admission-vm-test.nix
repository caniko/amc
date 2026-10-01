{
  pkgs,
  package,
}: let
  mib = 1048576;
  userPolicy = pkgs.writeText "host-user-policy.json" (builtins.toJSON {
    version = 1;
    budget_bytes = 160 * mib;
    reserve_bytes = 64 * mib;
    queue_limit = 16;
    contracts = {
      tool = {
        slice = "agent-tools.slice";
        memory_max = 96 * mib;
        memory_swap_max = 0;
        max_running = 4;
        pause_file = null;
      };
      small = {
        slice = "agent-tools.slice";
        memory_max = 32 * mib;
        memory_swap_max = 0;
        max_running = 4;
        pause_file = null;
      };
    };
  });
in
  (pkgs.testers.runNixOSTest {
    name = "amc-shared-host-admission";
    nodes.machine = {
      imports = [./host-admission-module.nix];
      services.amc.hostAdmission = {
        enable = true;
        inherit package;
        policy = {
          version = 1;
          budget_bytes = 160 * mib;
          reserve_bytes = 64 * mib;
          swap_reserve_bytes = 0;
          max_memory_full_psi = 100.0;
          max_io_full_psi = 100.0;
          resume_ms = 250;
          aging_ms = 1000;
          queue_limit = 16;
          domains =
            (map (uid: {
              name = "tools-${toString uid}";
              inherit uid;
              cgroup = "/user.slice/user-${toString uid}.slice/user@${toString uid}.service/agent.slice/agent-tools.slice";
              ceiling_bytes = 96 * mib;
              swap_bytes = 0;
              fair_share_bytes = 80 * mib;
            }) [1000 1001])
            ++ [
              {
                name = "builders";
                uid = 0;
                cgroup = "/builders.slice";
                ceiling_bytes = 96 * mib;
                swap_bytes = 0;
                fair_share_bytes = 80 * mib;
              }
            ];
        };
      };
      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };
      users.users.bob = {
        isNormalUser = true;
        uid = 1001;
      };
      systemd.user.slices.agent-tools.sliceConfig = {
        MemoryMax = "256M";
        MemorySwapMax = 0;
      };
      systemd.slices.builders.sliceConfig = {
        MemoryMax = "96M";
        MemorySwapMax = 0;
      };
      systemd.services.amc-host-admission.requires = ["builders.slice"];
      systemd.services.amc-host-admission.after = ["builders.slice"];
      systemd.user.services.amc-admission = {
        wantedBy = ["default.target"];
        requires = ["agent-tools.slice"];
        after = ["agent-tools.slice"];
        serviceConfig = {
          ExecStart = "${package}/bin/amc admission serve --policy ${userPolicy} --host-socket /run/amc-host/admission.sock";
          Restart = "on-failure";
        };
        path = [pkgs.systemd];
      };
      environment.systemPackages = [package pkgs.python3];
      virtualisation.memorySize = 2048;
      virtualisation.cores = 2;
    };
    testScript = builtins.readFile ./host-admission-vm-test.py;
  }).overrideTestDerivation (previous:
    assert pkgs.lib.hasInfix "-o $out" previous.buildCommand; {
      buildCommand = builtins.replaceStrings ["-o $out"] ["-o $out --junit-xml junit.xml"] previous.buildCommand;
    })
