{
  pkgs,
  package,
  module,
}: let
  policy = {
    version = 1;
    mode = "enforce";
    forecast_recovery = true;
    reserve_bytes = 256 * 1024 * 1024;
    emergency_available_bytes = 128 * 1024 * 1024;
    emergency_full_psi = 0.1;
    term_ms = 5000;
    kill_ms = 5000;
    cooldown_ms = 2000;
    recovery_window_ms = 3600000;
    domain_recovery_limit = 2;
    host_recovery_limit = 2;
    forecast = {
      horizon = 15;
      calibration = 32;
      alpha = 0.1;
      max_gap_ms = 1500;
    };
    domains = [
      {
        id = "backend";
        uid = null;
        unit = "amc-backend.service";
        lifecycle = "restart";
        memory_max = 128 * 1024 * 1024;
        memory_swap_max = 0;
        priority = 10;
      }
    ];
    job_pools = [];
  };
  workload = pkgs.writeText "amc-supervision-growth.py" ''
    import pathlib, signal, subprocess, sys, time
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    counter = pathlib.Path("/var/lib/amc-fixture/invocations")
    count = int(counter.read_text()) if counter.exists() else 0
    counter.write_text(str(count + 1))
    child = subprocess.Popen([sys.executable, "-c", "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(600)"])
    pathlib.Path(f"/var/lib/amc-fixture/child-{count + 1}").write_text(str(child.pid))
    blocks = [bytearray(16 * 1024 * 1024)]
    time.sleep(65)
    while True:
        blocks.append(bytearray(1024 * 1024))
        time.sleep(1)
  '';
in
  (pkgs.testers.runNixOSTest {
    name = "amc-supervision";
    nodes.machine = {
      imports = [module];
      virtualisation.memorySize = 2048;
      boot.kernel.sysctl."vm.panic_on_oom" = 0;
      environment.systemPackages = [package pkgs.python3];
      services.amc.supervision = {
        enable = true;
        inherit package policy;
      };
      systemd.services.amc-backend = {
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          ExecStart = "${pkgs.python3}/bin/python3 ${workload}";
          StateDirectory = "amc-fixture";
          MemoryMax = "128M";
          MemorySwapMax = 0;
          KillMode = "control-group";
          OOMPolicy = "kill";
          Restart = "no";
          TimeoutStopSec = "3s";
        };
      };
    };
    testScript = ''
      import json, time
      start_all()
      machine.wait_for_unit("amc-backend.service")
      machine.wait_for_unit("amc-supervision.service")
      amc = "${package}/bin/amc"
      def status():
          return json.loads(machine.succeed(amc + " supervise status"))
      def wait_for(predicate, seconds):
          deadline = time.monotonic() + seconds
          while time.monotonic() < deadline:
              if predicate():
                  return
              time.sleep(0.2)
          raise AssertionError("supervision condition did not settle within deadline")
      machine.wait_until_succeeds("test -f /run/amc-supervision/health.json")
      def oom_events():
          return dict((key, int(value)) for key, value in (line.split() for line in machine.succeed("cat /sys/fs/cgroup/memory.events").splitlines()))
      initial_oom = oom_events()
      with subtest("identity-bound recovery waits for descendant cleanup and starts once"):
          first = machine.succeed("systemctl show amc-backend -p InvocationID --value").strip()
          wait_for(lambda: machine.succeed("cat /var/lib/amc-fixture/invocations").strip() == "2", 200)
          machine.wait_for_unit("amc-backend.service")
          wait_for(lambda: status()["recovery"]["active"] is None, 15)
          second = machine.succeed("systemctl show amc-backend -p InvocationID --value").strip()
          assert first != second
          child = machine.succeed("cat /var/lib/amc-fixture/child-1").strip()
          machine.succeed("test ! -e /proc/" + child)
          s = status()
          assert len(s["recovery"]["attempts"]) == 1, s

      with subtest("chronological replay and public fail-closed heartbeat"):
          machine.succeed(amc + " supervise replay --file /var/lib/amc-supervision/trace.jsonl > /var/lib/amc-fixture/replay.json")
          replay = json.loads(machine.succeed("cat /var/lib/amc-fixture/replay.json"))
          assert replay["backend"]["completed_windows"] > 0, replay
          assert replay["backend"]["strong"] > 0, replay
          assert machine.succeed("stat -c %a /run/amc-supervision/health.json").strip() == "644"

      with subtest("in-flight supervisor restart trips without replay or forgiven budget"):
          wait_for(lambda: status()["recovery"]["active"] is not None, 200)
          active = status()["recovery"]["active"]
          assert active["phase"]["phase"] == "terminating", active
          machine.succeed("systemctl stop amc-supervision")
          h = json.loads(machine.succeed("cat /run/amc-supervision/health.json"))
          assert h["inhibit"] is True
          machine.succeed("systemctl start amc-supervision")
          wait_for(lambda: status()["recovery"]["active"]["phase"]["phase"] == "tripped", 10)
          time.sleep(8)
          assert machine.succeed("cat /var/lib/amc-fixture/invocations").strip() == "2"
          saved = json.loads(machine.succeed("cat /var/lib/amc-supervision/recovery.json"))
          assert len(saved["attempts"]) == 2, saved
          machine.succeed("systemctl stop amc-supervision amc-backend")

      with subtest("shadow observations have no intervention authority"):
          p = status()["policy"]
          p["mode"] = "shadow"
          # Deliberately unhealthy headroom guarantees an emergency observation.
          p["reserve_bytes"] = 4 * 1024**3
          p["emergency_available_bytes"] = 3 * 1024**3
          machine.succeed("mkdir -p /run/amc-shadow /var/lib/amc-shadow; chmod 700 /var/lib/amc-shadow")
          machine.succeed("cat > /var/lib/amc-fixture/shadow.json <<'EOF'\n" + json.dumps(p) + "\nEOF")
          machine.succeed("systemctl start amc-backend")
          third = machine.succeed("systemctl show amc-backend -p InvocationID --value").strip()
          machine.succeed("systemd-run --unit=amc-shadow " + amc + " supervise serve --policy /var/lib/amc-fixture/shadow.json --runtime /run/amc-shadow --state /var/lib/amc-shadow")
          machine.wait_until_succeeds("test -f /run/amc-shadow/status.json")
          time.sleep(8)
          shadow = json.loads(machine.succeed("cat /run/amc-shadow/status.json"))
          assert shadow["recovery"]["active"] is None
          assert shadow["recovery"]["attempts"] == []
          assert shadow["inhibit"] is False
          assert machine.succeed("systemctl show amc-backend -p InvocationID --value").strip() == third
          machine.succeed("systemctl stop amc-shadow amc-backend")

      with subtest("native recovery completes without OOM kills"):
          final_oom = oom_events()
          assert initial_oom["oom_kill"] == final_oom["oom_kill"] == 0, (initial_oom, final_oom)
          assert initial_oom["oom"] == final_oom["oom"] == 0, (initial_oom, final_oom)
          machine.succeed("cat > /var/lib/amc-fixture/oom.json <<'EOF'\n" + json.dumps({"initial": initial_oom, "final": final_oom}) + "\nEOF")
      machine.succeed("mkdir -p /var/lib/amc-fixture/evidence; cp /var/lib/amc-supervision/trace.jsonl /var/lib/amc-supervision/recovery.json /run/amc-supervision/status.json /var/lib/amc-fixture/replay.json /var/lib/amc-fixture/oom.json /var/lib/amc-fixture/evidence/")
      machine.copy_from_machine("/var/lib/amc-fixture/evidence", "supervision")
    '';
  }).overrideTestDerivation (previous:
    assert pkgs.lib.hasInfix "-o $out" previous.buildCommand; {
      buildCommand = builtins.replaceStrings ["-o $out"] ["-o $out --junit-xml junit.xml"] previous.buildCommand;
    })
