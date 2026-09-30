{
  pkgs,
  amcPackage,
}: let
  policy = pkgs.writeText "admission-test.json" (builtins.toJSON {
    version = 1;
    budget_bytes = 64 * 1024 * 1024;
    reserve_bytes = 128 * 1024 * 1024;
    queue_limit = 8;
    contracts.test = {
      slice = "agent-test.slice";
      memory_max = 64 * 1024 * 1024;
      memory_swap_max = 0;
      max_running = 1;
      pause_file = null;
    };
  });
in
  pkgs.testers.runNixOSTest {
    name = "amc-persistent-admission";
    nodes.machine = {
      virtualisation.memorySize = 2048;
      users.users = {
        alice = {
          isNormalUser = true;
          uid = 1000;
          linger = true;
        };
        bob = {
          isNormalUser = true;
          uid = 1001;
          linger = true;
        };
      };
      environment.systemPackages = [amcPackage pkgs.python3];
      systemd.user.slices.agent-test.sliceConfig = {
        MemoryAccounting = true;
        MemoryMax = "128M";
        MemorySwapMax = 0;
      };
      systemd.user.services.amc-admission = {
        requires = ["agent-test.slice"];
        after = ["agent-test.slice"];
        wantedBy = ["default.target"];
        path = [pkgs.systemd];
        serviceConfig = {
          ExecStart = "${amcPackage}/bin/amc admission serve --policy ${policy}";
          UMask = "0077";
        };
      };
    };
    testScript = ''
      import shlex
      start_all()
      def user(name, uid, command):
          env = "XDG_RUNTIME_DIR=/run/user/" + str(uid) + " DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/" + str(uid) + "/bus "
          return "su -s ${pkgs.bash}/bin/bash " + name + " -c " + shlex.quote(env + command)
      for name, uid in [("alice", 1000), ("bob", 1001)]:
          machine.wait_for_unit("user@" + str(uid) + ".service")
          machine.wait_until_succeeds(user(name, uid, "amc admission status --json"))
          machine.succeed(user(name, uid, "python3 ${../tests/admission-systemd.py} --amc ${amcPackage}/bin/amc --slice agent-test.slice"))
      machine.fail(user("bob", 1001, "amc admission status --socket /run/user/1000/amc/admission.sock --json"))
      machine.succeed(user("alice", 1000, "amc admission status --json"))
      machine.succeed(user("bob", 1001, "amc admission status --json"))
    '';
  }
