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
  heartbeatProbe = pkgs.writeText "amc-supervision-heartbeat-test.py" (builtins.readFile ./supervision-heartbeat-test.py);
  admissionFailures = pkgs.writeText "amc-supervision-admission-failures.py" (builtins.readFile ./supervision-admission-failures.py);
  failureWorkload = pkgs.writeText "amc-supervision-failure-workload.py" (builtins.readFile ./supervision-failure-workload.py);
in
  (pkgs.testers.runNixOSTest {
    name = "amc-supervision";
    nodes.machine = {
      imports = [module ./host-admission-module.nix];
      services.amc.hostAdmission = {
        enable = true;
        inherit package;
        healthFile = "/run/amc-supervision/health.json";
        policy = {
          version = 1;
          budget_bytes = 256 * 1024 * 1024;
          reserve_bytes = 64 * 1024 * 1024;
          swap_reserve_bytes = 0;
          max_memory_full_psi = 100.0;
          max_io_full_psi = 100.0;
          resume_ms = 250;
          aging_ms = 1000;
          queue_limit = 16;
          domains = map (name: {
            inherit name;
            uid = 0;
            cgroup = "/${name}.slice";
            ceiling_bytes = 64 * 1024 * 1024;
            swap_bytes = 0;
            fair_share_bytes = 64 * 1024 * 1024;
          }) ["heartbeat" "retained" "telemetry"];
        };
      };
      systemd.slices.heartbeat.sliceConfig = {
        MemoryMax = "64M";
        MemorySwapMax = 0;
      };
      systemd.slices.retained.sliceConfig = {
        MemoryMax = "64M";
        MemorySwapMax = 0;
      };
      systemd.slices.telemetry.sliceConfig = {
        MemoryMax = "64M";
        MemorySwapMax = 0;
      };
      systemd.services.amc-host-admission.requires = ["heartbeat.slice" "retained.slice" "telemetry.slice"];
      systemd.services.amc-host-admission.after = ["heartbeat.slice" "retained.slice" "telemetry.slice"];
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
      systemd.services.amc-cold-backend = {
        script = ''
          if [ ! -e /var/lib/amc-fixture/cold-started ]; then
            touch /var/lib/amc-fixture/cold-started
            exit 42
          fi
          exec ${pkgs.coreutils}/bin/sleep 600
        '';
        serviceConfig = {
          MemoryMax = "64M";
          MemorySwapMax = 0;
          MemoryAccounting = true;
          Restart = "no";
          KillMode = "control-group";
          OOMPolicy = "kill";
          TimeoutStopSec = "2s";
        };
      };
      systemd.services."amc-failure@".serviceConfig = {
        ExecStart = "${pkgs.python3}/bin/python3 ${failureWorkload} %i";
        Slice = "system.slice";
        MemoryMax = "64M";
        MemorySwapMax = 0;
        Restart = "no";
        KillMode = "control-group";
        OOMPolicy = "kill";
        TimeoutStopSec = "2s";
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
          # The cgroup-v2 root has no memory.events. system.slice aggregates
          # every supervised service even after its leaf cgroup is removed.
          events = dict((key, int(value)) for key, value in (line.split() for line in machine.succeed("cat /sys/fs/cgroup/system.slice/memory.events").splitlines()))
          vmstat = dict((key, int(value)) for key, value in (line.split() for line in machine.succeed("cat /proc/vmstat").splitlines()))
          events["host_oom_kill"] = vmstat["oom_kill"]
          return events
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

      with subtest("chronological replay and public fail-closed heartbeat"):
          # The main supervisor is stopped: replay and export now use the same
          # complete, frozen trace, including the in-flight restart evidence.
          machine.succeed(amc + " supervise replay --file /var/lib/amc-supervision/trace.jsonl > /var/lib/amc-fixture/replay.json")
          replay = json.loads(machine.succeed("cat /var/lib/amc-fixture/replay.json"))
          assert replay["backend"]["completed_windows"] > 0, replay
          assert replay["backend"]["strong"] > 0, replay
          binding = {
              "schemaVersion": 1,
              "traceSha256": machine.succeed("sha256sum /var/lib/amc-supervision/trace.jsonl").split()[0],
              "traceBytes": int(machine.succeed("stat -c %s /var/lib/amc-supervision/trace.jsonl").strip()),
              "replaySha256": machine.succeed("sha256sum /var/lib/amc-fixture/replay.json").split()[0],
          }
          machine.succeed("cat > /var/lib/amc-fixture/replay-input.json <<'EOF'\n" + json.dumps(binding) + "\nEOF")
          assert machine.succeed("stat -c %a /run/amc-supervision/health.json").strip() == "644"

      with subtest("host admission denies inhibited and expired heartbeats"):
          machine.wait_for_unit("amc-host-admission.service")
          assert machine.succeed("systemctl show heartbeat.slice -p ControlGroup --value").strip() == "/heartbeat.slice"
          machine.succeed("python3 ${heartbeatProbe}")

      with subtest("invalid heartbeat and missing memory observations retain live grants"):
          machine.succeed("python3 ${admissionFailures}")

      with subtest("cold failed backend recovers once with durable invocation accounting"):
          machine.execute("systemctl start amc-cold-backend")
          wait_for(lambda: machine.succeed("systemctl show amc-cold-backend -p ActiveState --value").strip() == "failed", 10)
          first = machine.succeed("systemctl show amc-cold-backend -p InvocationID --value").strip()
          assert first
          p = status()["policy"]
          p["forecast_recovery"] = False
          p["job_pools"] = []
          p["domains"] = [{"id": "cold-backend", "uid": None, "unit": "amc-cold-backend.service", "lifecycle": "restart",
                           "memory_max": 64 * 1024**2, "memory_swap_max": 0, "priority": 10}]
          p["domain_recovery_limit"] = 1
          p["host_recovery_limit"] = 1
          machine.succeed("mkdir -p /run/amc-cold /var/lib/amc-cold; chmod 700 /var/lib/amc-cold")
          machine.succeed("cat > /var/lib/amc-fixture/cold.json <<'EOF'\n" + json.dumps(p) + "\nEOF")
          machine.succeed("systemd-run --unit=amc-cold " + amc + " supervise serve --policy /var/lib/amc-fixture/cold.json --runtime /run/amc-cold --state /var/lib/amc-cold")
          wait_for(lambda: machine.succeed("systemctl show amc-cold-backend -p ActiveState --value").strip() == "active", 30)
          def cold_status():
              return json.loads(machine.succeed("cat /run/amc-cold/status.json"))
          wait_for(lambda: len(cold_status()["recovery"]["attempts"]) == 1 and cold_status()["recovery"]["active"] is None, 30)
          second = machine.succeed("systemctl show amc-cold-backend -p InvocationID --value").strip()
          assert first != second, (first, second)
          machine.succeed("systemctl restart amc-cold")
          time.sleep(5)
          assert machine.succeed("systemctl show amc-cold-backend -p InvocationID --value").strip() == second
          assert len(cold_status()["recovery"]["attempts"]) == 1
          machine.succeed("cp /run/amc-cold/status.json /var/lib/amc-fixture/cold-status.json")
          machine.succeed("systemctl stop amc-cold amc-cold-backend")

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

      exec(${builtins.toJSON (builtins.readFile ./supervision-recovery-failures.py)})

      with subtest("native recovery completes without OOM kills"):
          final_oom = oom_events()
          assert initial_oom["oom_kill"] == final_oom["oom_kill"] == 0, (initial_oom, final_oom)
          assert initial_oom["oom"] == final_oom["oom"] == 0, (initial_oom, final_oom)
          assert initial_oom["host_oom_kill"] == final_oom["host_oom_kill"] == 0, (initial_oom, final_oom)
          machine.succeed("cat > /var/lib/amc-fixture/oom.json <<'EOF'\n" + json.dumps({"initial": initial_oom, "final": final_oom}) + "\nEOF")
      machine.succeed("mkdir -p /var/lib/amc-fixture/evidence; cp /var/lib/amc-supervision/trace.jsonl /var/lib/amc-supervision/recovery.json /run/amc-supervision/status.json /var/lib/amc-fixture/replay.json /var/lib/amc-fixture/replay-input.json /var/lib/amc-fixture/oom.json /var/lib/amc-fixture/heartbeat.json /var/lib/amc-fixture/cold-status.json /var/lib/amc-fixture/admission-failures.json /var/lib/amc-fixture/admission-observations.json /var/lib/amc-fixture/replacement-status.json /var/lib/amc-fixture/domain-budget-status.json /var/lib/amc-fixture/host-budget-status.json /var/lib/amc-fixture/foreign-status.json /var/lib/amc-fixture/evidence/")
      machine.copy_from_machine("/var/lib/amc-fixture/evidence", "supervision")
    '';
  }).overrideTestDerivation (previous:
    assert pkgs.lib.hasInfix "-o $out" previous.buildCommand; {
      buildCommand = builtins.replaceStrings ["-o $out"] ["-o $out --junit-xml junit.xml"] previous.buildCommand;
    })
