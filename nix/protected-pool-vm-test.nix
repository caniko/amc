{
  pkgs,
  package,
}: let
  mib = 1048576;
  pool = {
    source_contract = "tool";
    memory_bytes = 64 * mib;
    max_job_bytes = 32 * mib;
    max_running = 1;
    runtime_max_sec = 15;
    wait_max_sec = 15;
    queue_limit = 4;
    cpu_quota_usec = 50000;
    tasks_max = 32;
  };
  userPolicy = pkgs.writeText "protected-user-policy.json" (builtins.toJSON {
    version = 1;
    budget_bytes = 192 * mib;
    reserve_bytes = 128 * mib;
    queue_limit = 1;
    contracts = {
      tool = {
        slice = "agent-tools.slice";
        memory_max = 128 * mib;
        memory_swap_max = 0;
        max_running = 1;
        pause_file = null;
      };
      quick = {
        slice = "agent-quick.slice";
        memory_max = pool.max_job_bytes;
        memory_swap_max = 0;
        inherit (pool) max_running;
        pause_file = null;
        inherit (pool) runtime_max_sec;
        protected_pool = pool;
      };
    };
  });
in
  (pkgs.testers.runNixOSTest {
    name = "amc-protected-pool";
    nodes.machine = {config, ...}: {
      imports = [./host-admission-module.nix];
      services.amc.hostAdmission = {
        enable = true;
        inherit package;
        policy = {
          version = 1;
          budget_bytes = 256 * mib;
          reserve_bytes = 128 * mib;
          namespace_runner_bytes = 64 * mib;
          swap_reserve_bytes = 0;
          max_memory_full_psi = 100.0;
          max_io_full_psi = 100.0;
          resume_ms = 250;
          aging_ms = 1000;
          queue_limit = 1;
          domains = pkgs.lib.concatMap (uid: let
            prefix = "/user.slice/user-${toString uid}.slice/user@${toString uid}.service/agent.slice";
          in [
            {
              name = "tools-${toString uid}";
              inherit uid;
              cgroup = "${prefix}/agent-tools.slice";
              ceiling_bytes = 128 * mib;
              swap_bytes = 0;
              fair_share_bytes = 192 * mib;
            }
            {
              name = "quick-${toString uid}";
              inherit uid;
              cgroup = "${prefix}/agent-quick.slice";
              ceiling_bytes = pool.max_job_bytes;
              swap_bytes = 0;
              fair_share_bytes = 192 * mib;
              protected_pool = pool;
            }
          ]) [1000 1001];
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
      systemd.user.slices.agent-quick = {
        wantedBy = ["default.target"];
        sliceConfig = {
          MemoryAccounting = true;
          MemoryMax = pool.memory_bytes;
          MemorySwapMax = 0;
          CPUQuota = "50%";
          CPUQuotaPeriodSec = "100ms";
          TasksMax = pool.tasks_max;
        };
      };
      systemd.user.services.amc-admission = {
        wantedBy = ["default.target"];
        requires = ["agent-tools.slice" "agent-quick.slice"];
        after = ["agent-tools.slice" "agent-quick.slice"];
        path = [pkgs.systemd];
        serviceConfig = {
          ExecStart = "${package}/bin/amc admission serve --policy ${userPolicy} --host-socket /run/amc-host/admission.sock";
          Restart = "on-failure";
        };
      };
      environment.systemPackages = [package pkgs.python3 pkgs.util-linux];
      environment.etc."protected-host-policy.json".text = builtins.toJSON config.services.amc.hostAdmission.policy;
      # Only the disposable guest: intentional memcg OOM must not panic the VM.
      boot.kernel.sysctl."vm.panic_on_oom" = 0;
      virtualisation.memorySize = 1024;
      virtualisation.cores = 2;
    };
    testScript = builtins.readFile ./protected-pool-vm-test.py;
  }).overrideTestDerivation (previous:
    assert pkgs.lib.hasInfix "-o $out" previous.buildCommand; {
      buildCommand = builtins.replaceStrings ["-o $out"] ["-o $out --junit-xml junit.xml"] previous.buildCommand;
    })
