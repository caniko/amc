{
  pkgs,
  amcModule,
  amcPackage,
}:

pkgs.testers.runNixOSTest {
  name = "amc-generic";
  globalTimeout = 300;

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
          memory_swap_max = "32MiB";
        };
        applications.proof.profile = "proof";
      };
    };

    environment.systemPackages = [
      pkgs.busybox
      pkgs.python3
    ];
    environment.etc."amc-watch-cgroup.py" = {
      mode = "0755";
      text = ''
        #!${pkgs.python3}/bin/python3
        import os
        import pathlib
        import sys
        import time

        cgroup = pathlib.Path(sys.argv[1])
        output = pathlib.Path(sys.argv[2])
        output.mkdir()
        names = [
            "cgroup.events", "memory.events", "memory.peak",
            "memory.swap.current", "memory.swap.max",
        ]
        handles = {name: (cgroup / name).open() for name in names}
        maximum_swap = 0
        while True:
            values = {}
            removed = False
            for name, handle in handles.items():
                try:
                    handle.seek(0)
                    values[name] = handle.read()
                    (output / f"{name}.last").write_text(values[name])
                except OSError:
                    removed = True
                    break
            maximum_swap = max(maximum_swap, int(values.get("memory.swap.current", "0").strip()))
            if removed or "populated 0" in values.get("cgroup.events", ""):
                break
            time.sleep(0.005)
        (output / "max-swap").write_text(f"{maximum_swap}\n")
        (output / "done").write_text("done\n")
        fd = os.open(output / "done", os.O_RDONLY)
        os.fsync(fd)
        os.close(fd)
      '';
    };

    users.users.amc-test = {
      isNormalUser = true;
      linger = true;
      uid = 1000;
    };

    # The NixOS test harness defaults this to 2, which turns an intentional
    # memcg OOM into a kernel panic before systemd can enforce OOMPolicy=kill.
    boot.kernel.sysctl."vm.panic_on_oom" = 0;
    virtualisation.memorySize = 768;
    zramSwap = {
      enable = true;
      memoryPercent = 25;
    };
  };

  testScript = ''
    import json
    import re
    import shlex
    import time

    start_all()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@1000.service")
    machine.wait_for_file("/run/user/1000/bus")

    user_prefix = (
        "sudo -u amc-test env HOME=/home/amc-test USER=amc-test "
        "LOGNAME=amc-test XDG_RUNTIME_DIR=/run/user/1000 "
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus "
        "PATH=/run/current-system/sw/bin"
    )

    def user(command):
        return machine.succeed(f"{user_prefix} sh -c {shlex.quote('cd /tmp && ' + command)}")

    def normalized_field(value, wanted):
        if isinstance(value, dict):
            for key, child in value.items():
                if re.sub(r"[^a-z0-9]", "", key.lower()) == wanted:
                    return child
                found = normalized_field(child, wanted)
                if found is not None:
                    return found
        if isinstance(value, list):
            for child in value:
                found = normalized_field(child, wanted)
                if found is not None:
                    return found
        return None

    def first_json_object(output):
        decoder = json.JSONDecoder()
        for start, character in enumerate(output):
            if character == "{":
                try:
                    value, _ = decoder.raw_decode(output[start:])
                    return value
                except json.JSONDecodeError:
                    pass
        raise AssertionError(f"no JSON object in output: {output}")

    with subtest("module installation and user slices"):
        machine.succeed("test -x /run/current-system/sw/bin/amc")
        machine.succeed("test -s /etc/xdg/amc/config.toml")
        user("systemctl --user show app-amc.slice -p MemoryAccounting --value | grep -Fx yes")
        user("systemctl --user show background-amc.slice -p MemoryAccounting --value | grep -Fx yes")

    with subtest("limits exist before exec"):
        output = user(
            "amc run --id proof --profile proof -- "
            "amc test self-report --expect-memory-max 268435456B "
            "--expect-memory-swap-max 33554432B --expect-memory-oom-group 1"
        )
        machine.log(output)
        report = first_json_object(output)
        assert int(normalized_field(report, "memorymax")) == 268435456, report
        assert int(normalized_field(report, "memoryswapmax")) == 33554432, report
        # systemd 261 has no MemoryOOMGroup property; OOMPolicy=kill writes this cgroup file.
        assert int(normalized_field(report, "memoryoomgroup")) == 1, report

    with subtest("Nix builders escape the user application cgroup"):
        expression = (
            'let busybox = builtins.storePath "${pkgs.busybox}"; in '
            'derivation { name = "amc-boundary"; '
            'system = builtins.currentSystem; '
            'builder = "''${busybox}/bin/sh"; '
            'args = [ "-c" "''${busybox}/bin/cat /proc/self/cgroup > $out" ]; }'
        )
        inner = (
            "cat /proc/self/cgroup > /tmp/amc-nix-client-cgroup; "
            "nix-build --no-substitute --expr " + shlex.quote(expression)
            + " > /tmp/amc-boundary-output"
        )
        user(
            "amc run --id proof --profile proof -- sh -c "
            + shlex.quote(inner)
        )
        client_cgroup = machine.succeed("cat /tmp/amc-nix-client-cgroup")
        assert "/user.slice/" in client_cgroup, client_cgroup
        assert "app-amc-" in client_cgroup, client_cgroup
        builder = machine.succeed("cat /tmp/amc-boundary-output").strip()
        builder_cgroup = machine.succeed(f"cat {shlex.quote(builder)}")
        machine.log(builder_cgroup)
        assert "/user.slice/" not in builder_cgroup, builder_cgroup
        assert "nix-daemon.service" in builder_cgroup, builder_cgroup

    with subtest("contained OOM leaves heartbeat responsive"):
        user(
            "systemd-run --user --service-type=exec --collect "
            "--unit=amc-proof-heartbeat.service --property=Slice=app.slice "
            "${amcPackage}/bin/amc test heartbeat "
            "--samples /tmp/amc-heartbeat-samples.json "
            "--summary /tmp/amc-heartbeat.json --interval-ms 50 --duration-ms 10000"
        )
        user(
            "amc launch --id proof --profile proof --retain-unit -- "
            "amc test hog --report /tmp/amc-hog-report.json "
            "--expect-memory-max 268435456B --expect-memory-swap-max 33554432B "
            "--expect-memory-oom-group 1 --chunk-size 4MiB --maximum 512MiB "
            "--progress-interval 64MiB --delay-ms 50"
        )
        machine.wait_until_succeeds(
            f"{user_prefix} systemctl --user list-units --all --type=service "
            "--plain --no-legend 'app-amc-*.service' | grep -q .",
            timeout=20,
        )
        unit = user(
            "systemctl --user list-units --all --type=service --plain "
            "--no-legend 'app-amc-*.service' | sed -n '1s/ .*//p'"
        ).strip()
        assert unit.endswith(".service"), unit
        properties = user(
            f"systemctl --user show {shlex.quote(unit)} "
            "-p ControlGroup -p MemoryMax -p MemorySwapMax -p OOMPolicy -p Slice"
        )
        machine.log(properties)
        assert "MemoryMax=268435456" in properties, properties
        assert "MemorySwapMax=33554432" in properties, properties
        assert "OOMPolicy=kill" in properties, properties
        assert "Slice=app-amc.slice" in properties, properties
        control_group_match = re.search(r"^ControlGroup=(.+)$", properties, re.MULTILINE)
        assert control_group_match is not None, properties
        control_group = control_group_match.group(1)
        user(
            "systemd-run --user --service-type=exec --collect "
            "--unit=amc-cgroup-watch.service "
            f"/etc/amc-watch-cgroup.py /sys/fs/cgroup{control_group} /tmp/amc-watch"
        )
        machine.wait_for_file("/tmp/amc-hog-report.json", timeout=10)
        hog_report = json.loads(machine.succeed("cat /tmp/amc-hog-report.json"))
        assert int(normalized_field(hog_report, "memorymax")) == 268435456, hog_report
        assert int(normalized_field(hog_report, "memoryswapmax")) == 33554432, hog_report
        assert int(normalized_field(hog_report, "memoryoomgroup")) == 1, hog_report

        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            state = user(
                f"systemctl --user show {shlex.quote(unit)} -p ActiveState --value"
            ).strip()
            if state in ("failed", "inactive"):
                break
            time.sleep(0.05)
        else:
            raise AssertionError(f"offender did not terminate: {unit}")

        machine.wait_for_file("/tmp/amc-watch/done", timeout=10)
        last_events = machine.succeed("cat /tmp/amc-watch/memory.events.last")
        maximum_swap = int(machine.succeed("cat /tmp/amc-watch/max-swap"))
        events = dict(
            (key, int(value))
            for key, value in re.findall(r"^([a-z_]+) ([0-9]+)$", last_events, re.MULTILINE)
        )
        assert events.get("oom", 0) > 0, last_events
        assert events.get("oom_kill", 0) > 0, last_events
        assert maximum_swap <= 33554432, maximum_swap
        user("systemctl --user is-active amc-proof-heartbeat.service")

        machine.wait_for_file("/tmp/amc-heartbeat.json", timeout=30)
        summary = json.loads(machine.succeed("cat /tmp/amc-heartbeat.json"))
        machine.log(json.dumps(summary, sort_keys=True))
        assert summary["count"] > 0, summary
        assert summary["maxGapUs"] < 2_000_000, summary
        assert summary["p99DelayUs"] < 500_000, summary

        result = user(
            f"systemctl --user show {shlex.quote(unit)} -p Result --value"
        ).strip()
        assert result in ("oom-kill", "signal"), result
        user(f"systemctl --user stop {shlex.quote(unit)} || true")
        user(f"systemctl --user reset-failed {shlex.quote(unit)} || true")
        machine.wait_until_succeeds(
            f"! {user_prefix} systemctl --user list-units --all --type=service "
            "--plain --no-legend 'app-amc-*.service' | grep -q .",
            timeout=20,
        )
        machine.wait_until_succeeds("! pgrep -u amc-test -x amc", timeout=10)
  '';
}
