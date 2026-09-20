"""Disposable VM mechanism tests. No OpenCode, host probes or benchmarks."""
import json
from pathlib import Path
import re
import shlex
import signal
import sys
import tempfile
import time

# Supplied by the NixOS test driver prelude (machine, start_all, subtest);
# rebinding them here collides with the harness definitions (ruff F811).


class BudgetExhausted(Exception):
    """Local phase budget expired before a guest request started."""


def emit(message):
    try:
        print(message)
    except Exception:
        pass


def emitted_properties(explanation):
    properties = json.dumps(explanation["systemdProperties"])
    assert "MemoryOOMGroup" not in properties
    assert "KillMode" not in properties
    assert "Restart=no" in properties
    assert "MemoryHigh" not in properties


prefix = (
    "sudo -u amc-test env HOME=/home/amc-test USER=amc-test LOGNAME=amc-test "
    "XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus "
    "PATH=/run/current-system/sw/bin"
)
owned = set()
identity_files = set()
observed_cgroups = {}
recorded_artifacts = []
manager_root = None
artifact_dir = Path(tempfile.mkdtemp(prefix="mechanism-", dir=machine.out_dir))

# Only fixture-owned, allowlisted data. No application journal or raw client logs.
ARTIFACTS = (
    "/tmp/entry.json", "/tmp/high.json", "/tmp/hog-entry.json",
    "/tmp/work-a.json", "/tmp/work-b.json", "/tmp/work-c.json",
    "/tmp/precedence-one.json", "/tmp/precedence-two.json", "/tmp/precedence-winner.json",
    "/tmp/oom-watch/samples.jsonl", "/tmp/oom-watch/summary.json", "/tmp/oom-watch/manifest.json",
)


def record(name, data):
    (artifact_dir / name).write_text(json.dumps(data, indent=2))
    recorded_artifacts.append({"source": "driver", "artifact": name, "status": "EXPORTED"})
    emit("AMC_ARTIFACT " + json.dumps({"run": artifact_dir.name, "path": name, "data": data}))


def own(unit, slice_path="app.slice"):
    owned.add(unit)
    # Explicit fixture topology, not arbitrary inference about application IPC.
    # properties() checks this against the manager while units exist.
    observed_cgroups[unit] = f"{manager_root}/{slice_path}/{unit}"


def bounded_guest(machine, command, deadline):
    """Bound the host wait too: driver execute(timeout=...) only times the guest."""
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise BudgetExhausted("collection budget exhausted")
    if not machine.is_up():
        raise TimeoutError("guest unavailable")
    seconds = min(5.0, remaining)

    def expired(signum, frame):
        raise TimeoutError("guest control deadline")

    previous = signal.getsignal(signal.SIGALRM)
    timer = signal.getitimer(signal.ITIMER_REAL)
    started = time.monotonic()
    # Do not extend a timer installed by a caller.
    seconds = min(seconds, timer[0]) if timer[0] else seconds
    signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        return machine.execute(command, timeout=max(0.1, seconds - 0.1))
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)
        if timer[0]:
            signal.setitimer(signal.ITIMER_REAL, max(0.001, timer[0] - (time.monotonic() - started)), timer[1])


def finalize_run(machine, prefix, owned, identities, cgroups, output, primary_failure, recorded=(), export_budget=16 * 1024 * 1024, export_seconds=20, cleanup_seconds=35):
    # Bare-list annotation: the driver type-checks this script, and an
    # empty inferred element type would reject the export records below.
    exports: list = list(recorded)
    report = {"primaryFailure": primary_failure, "exports": exports, "cleanup": [], "complete": False}
    transport_ok = True
    export_deadline = time.monotonic() + export_seconds
    remaining_bytes = export_budget

    def checkpoint():
        try:
            (output / "finalization.json").write_text(json.dumps(report, indent=2))
        except OSError:
            report["persistence"] = "UNKNOWN: local artifact write failed"

    def query(command, deadline):
        nonlocal transport_ok
        if not transport_ok:
            raise TimeoutError("control channel unavailable")
        try:
            return bounded_guest(machine, command, deadline)
        except BudgetExhausted:
            raise
        except BaseException:
            # A request started; the reply may be lost. Do not reuse the channel.
            transport_ok = False
            raise

    try:
        # Identity recovery precedes other exports, even if submission raised.
        for source in sorted(identities) + list(ARTIFACTS):
            entry = {"source": source, "status": "UNKNOWN", "exitCode": None}
            report["exports"].append(entry)
            try:
                if remaining_bytes <= 0:
                    raise ValueError("artifact byte budget exhausted")
                # Regular files only, at most 4 MiB per file / fixed allowlist.
                script = ("import os,stat,sys; p=sys.argv[1]; "
                          "fd=os.open(p,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK); "
                          "s=os.fstat(fd); "
                          "assert stat.S_ISREG(s.st_mode) and s.st_size<=4194304; "
                          "f=os.fdopen(fd); data=f.read(4194305); "
                          "assert len(data.encode())<=4194304; sys.stdout.write(data)")
                code, text = query(f"python3 -c {shlex.quote(script)} {shlex.quote(source)} 2>/dev/null", export_deadline)
                entry["exitCode"] = code
                if code != 0:
                    entry["reason"] = "missing, unreadable or oversized artifact"
                    continue
                remaining_bytes -= len(text.encode())
                if remaining_bytes < 0:
                    raise ValueError("artifact byte budget exceeded")
                if source in identities:
                    unit = text.strip()
                    if not re.fullmatch(r"app-amc-[a-z0-9-]+@[a-f0-9]+\.service", unit):
                        raise ValueError("invalid identity")
                    owned.add(unit)
                    if unit not in cgroups:
                        # A preceding observed fixture supplies the fixed slice
                        # root, even if this attempt's reply was lost.
                        roots = {path.rsplit("/", 1)[0] for key, path in cgroups.items() if key.startswith("app-amc-")}
                        if len(roots) == 1:
                            cgroups[unit] = f"{next(iter(roots))}/{unit}"
                    data = unit
                elif source.endswith(".jsonl"):
                    data = [json.loads(line) for line in text.splitlines()]
                else:
                    data = json.loads(text)
                relative = source.removeprefix("/tmp/")
                destination = output / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(text)
                entry.update(status="EXPORTED", artifact=relative)
                # Failed Nix outputs may be discarded. Preserve bounded,
                # allowlisted evidence in the driver log as well as $out.
                emit("AMC_ARTIFACT " + json.dumps({"run": output.name, "path": relative, "data": data}))
            except BaseException as error:
                entry["reason"] = type(error).__name__
            finally:
                checkpoint()
        # Capture live manager/kernel state before stopping anything, including
        # units recovered from a failed submission's identity file.
        for unit in sorted(owned):
            entry = {"source": f"inspect:{unit}", "status": "UNKNOWN", "exitCode": None}
            report["exports"].append(entry)
            try:
                code, text = query(f"{prefix} amc inspect {shlex.quote(unit)} --json 2>/dev/null", export_deadline)
                entry["exitCode"] = code
                remaining_bytes -= len(text.encode())
                if code != 0 or remaining_bytes < 0:
                    raise ValueError("snapshot unavailable or artifact budget exceeded")
                data = json.loads(text)
                path = data["properties"].get("ControlGroup", "")
                if re.fullmatch(r"/[a-zA-Z0-9_./@:-]+", path) and ".." not in path.split("/"):
                    cgroups[unit] = path
                name = f"before-cleanup-{unit}.json"
                (output / name).write_text(text)
                entry.update(status="EXPORTED", artifact=name)
                emit("AMC_ARTIFACT " + json.dumps({"run": output.name, "path": name, "data": data}))
            except BaseException as error:
                entry["reason"] = type(error).__name__
            finally:
                checkpoint()
    finally:
        cleanup_deadline = time.monotonic() + cleanup_seconds
        for unit in sorted(owned):
            entry = {"unit": unit, "status": "UNKNOWN", "stopExit": None, "resetExit": None, "stateExit": None}
            report["cleanup"].append(entry)
            try:
                for action, key in [("stop", "stopExit"), ("reset-failed", "resetExit")]:
                    code, _ = query(f"{prefix} systemctl --user {action} -- {shlex.quote(unit)} 2>/dev/null", cleanup_deadline)
                    entry[key] = code
                code, text = query(f"{prefix} systemctl --user show --property=LoadState,ActiveState,MainPID,ControlPID -- {shlex.quote(unit)} 2>/dev/null", cleanup_deadline)
                entry["stateExit"] = code
                values = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
                # Do not retain unexpected diagnostic output.
                entry["loadState"] = values.get("LoadState") if values.get("LoadState") in ("loaded", "not-found") else None
                entry["activeState"] = values.get("ActiveState") if values.get("ActiveState") in ("inactive", "failed", "active", "activating", "deactivating") else None
                path = cgroups.get(unit)
                entry["cgroupPath"] = path
                entry["pathBasis"] = "explicit fixture topology; checked against live manager when sampled"
                if path:
                    # /sys files have st_size=0 even when populated; read data.
                    test = f'test ! -e /sys/fs/cgroup{path}/cgroup.events || grep -qx "populated 0" /sys/fs/cgroup{path}/cgroup.events'
                    empty_code, _ = query(test, cleanup_deadline)
                    entry["cgroupEmptyExit"] = empty_code
                else:
                    empty_code = None
                if (code == 0 and entry["activeState"] in ("inactive", "failed")
                        and (values.get("MainPID"), values.get("ControlPID")) == ("0", "0")
                        and empty_code == 0):
                    entry["status"] = "CONFIRMED_EMPTY"
                # Missing unit metadata alone does not confirm cleanup.
            except BaseException as error:
                entry["reason"] = type(error).__name__
            finally:
                checkpoint()
        report["complete"] = (
            all(item["status"] == "CONFIRMED_EMPTY" for item in report["cleanup"])
            and all(item["status"] == "EXPORTED" for item in report["exports"])
            and "persistence" not in report
        )
        checkpoint()
        emit("AMC_FINALIZATION " + json.dumps(report))
    return report


def user(command):
    return machine.succeed(f"{prefix} sh -c {shlex.quote('cd /tmp && ' + command)}", timeout=45)


def properties(unit):
    # Safe allowlisted diagnostic metadata, never ExecStart/Environment/journal.
    report = json.loads(user(f"amc inspect {shlex.quote(unit)} --json"))
    path = report["properties"].get("ControlGroup", "")
    if path.startswith("/") and re.fullmatch(r"/[a-zA-Z0-9_./@:-]+", path) and ".." not in path.split("/"):
        if unit in observed_cgroups:
            assert path == observed_cgroups[unit], "fixture placement differs from explicit slice topology"
        observed_cgroups[unit] = path
    return report


def state(unit):
    return user(f"systemctl --user show {shlex.quote(unit)} -p ActiveState --value").strip()


def wait_end(unit, seconds=40):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if state(unit) in ("inactive", "failed"):
            return
        time.sleep(0.05)
    raise AssertionError(f"fixture exceeded observation deadline: {unit}")


def managed(command, identity, mode="run", app_id="proof", profile="proof", seconds=10):
    # The file is published before submission, also on an ambiguous response.
    identity_files.add(f"/tmp/{identity}")
    result = user(f"amc {mode} --id {app_id} --profile {profile} --retain-unit --runtime-max-sec {seconds} "
                  f"--unit-file /tmp/{identity} -- {command}")
    code, unit = machine.execute(f"cat /tmp/{identity}", timeout=5)
    if code == 0:
        assert re.fullmatch(r"app-amc-[a-z0-9-]+@[a-f0-9]+\.service", unit.strip())
        own(unit.strip(), "app.slice/app-amc.slice")
    return result


def native_preservation():
    # Compare only this disposable fixture's known argv/environment. Export
    # booleans, not raw ExecStart, /proc cmdline, or environment contents.
    script = (
        "import json,pathlib,shlex,subprocess; "
        "pid=subprocess.check_output(['systemctl','--user','show','amc-native.service','-p','MainPID','--value'],text=True).strip(); "
        "expected=pathlib.Path('/etc/amc-native-executable').read_text().strip(); "
        "proc=pathlib.Path('/proc')/pid; "
        "argv=(proc/'cmdline').read_bytes().split(b'\\0')[:-1]; "
        "configured=shlex.split(subprocess.check_output(['systemctl','--user','show','amc-native.service','-p','Environment','--value'],text=True)); "
        "print(json.dumps({'executableAndArgvPreserved': (proc/'exe').resolve()==pathlib.Path(expected).resolve() and argv==[expected.encode(),b'120'], "
        "'configuredEnvironmentPreserved':configured==['AMC_FIXTURE_SENTINEL=preserved']}))"
    )
    return json.loads(user("python3 -c " + shlex.quote(script)))


try:
    start_all()
    machine.wait_for_unit("multi-user.target", timeout=60)
    machine.wait_for_unit("user@1000.service", timeout=60)
    machine.wait_for_file("/run/user/1000/bus", timeout=60)
    # Bound the control socket too if the guest stops responding mid-test.
    if machine.shell is not None:
        machine.shell.settimeout(45)
    manager_root = machine.succeed("systemctl show user@1000.service -p ControlGroup --value", timeout=5).strip()
    assert re.fullmatch(r"/[a-zA-Z0-9_./@:-]+", manager_root) and ".." not in manager_root.split("/")
    versions = json.loads(machine.succeed("python3 -c " + shlex.quote(
        "import json,platform,subprocess; print(json.dumps({'kernel':platform.release(),"
        "'systemd':subprocess.check_output(['systemctl','--version'],text=True).splitlines()[0],"
        "'nix':subprocess.check_output(['nix','--version'],text=True).strip()}))"), timeout=5))
    record("versions.json", versions)
    with subtest("fixture-properties-and-doctor"):
        explanation = json.loads(user("amc explain --id proof --json"))
        record("explanation.json", explanation)
        emitted_properties(explanation)
        # Doctor is intentionally active here, inside the VM only.
        doctor = json.loads(user("amc doctor --json"))
        record("doctor.json", doctor)
        assert doctor["available"], doctor

    with subtest("helper-entry-settings"):
        managed("amc test self-report "
              "--output /tmp/entry.json --expect-memory-max 256MiB "
              "--expect-memory-swap-max 0B --expect-memory-oom-group 1", "entry-unit")
        report = json.loads(machine.succeed("cat /tmp/entry.json"))
        entry_unit = machine.succeed("cat /tmp/entry-unit").strip()
        assert report["cgroupPath"] == observed_cgroups[entry_unit]
        assert report["memoryMax"] == "268435456"
        assert report["memoryOomGroup"] == "1"

    with subtest("rejected-native-setting-does-not-run-target"):
        own("amc-rejected.service")
        code, _ = machine.execute(f"{prefix} systemd-run --user --unit=amc-rejected.service "
                                  "-p DeliberatelyInvalidAMCProperty=yes -- "
                                  "touch /tmp/rejected-target-ran")
        assert code != 0
        machine.fail("test -e /tmp/rejected-target-ran")

    with subtest("abc-useful-work-smoke-not-a-pressure-comparison"):
        task = "amc test work --size 4MiB --iterations 5 --report"
        user(f"{task} /tmp/work-a.json")
        report = json.loads(machine.succeed("cat /tmp/work-a.json"))
        assert report["iterationsCompleted"] == 5
        own("amc-smoke-b.service", "app.slice/app-amc.slice")
        user("systemd-run --user --service-type=exec --expand-environment=no --wait --pipe "
             "--unit=amc-smoke-b.service -p Slice=app-amc.slice -p MemoryMax=256M "
             "-p MemorySwapMax=0 -p OOMPolicy=kill -p Restart=no -p RuntimeMaxSec=10 "
             f"-- {task} /tmp/work-b.json --expect-memory-max 256MiB --expect-memory-swap-max 0B --expect-memory-oom-group 1")
        report = json.loads(machine.succeed("cat /tmp/work-b.json"))
        assert report["iterationsCompleted"] == 5
        managed(f"{task} /tmp/work-c.json "
                "--expect-memory-max 256MiB --expect-memory-swap-max 0B --expect-memory-oom-group 1", "work-c-unit")
        report = json.loads(machine.succeed("cat /tmp/work-c.json"))
        assert report["iterationsCompleted"] == 5
        assert report["bytesTouched"] == 5 * (4 << 20)

    with subtest("high-max-settings-smoke-not-pressure"):
        managed("amc test self-report --output /tmp/high.json --expect-memory-high 192MiB", "high-unit", profile="proof-high")
        assert json.loads(machine.succeed("cat /tmp/high.json"))["memoryHigh"] == "201326592"

    with subtest("real-manager-cancellation-before-acknowledgment"):
        managed("true", "cancel-seed-unit", app_id="cancel")
        seed = machine.succeed("cat /tmp/cancel-seed-unit").strip()
        template = seed.split("@", 1)[0] + "@.service"
        directory = f"/home/amc-test/.config/systemd/user/{template}.d"
        user(f"mkdir -p {shlex.quote(directory)}")
        user("printf '[Service]\\nExecStartPre=/run/current-system/sw/bin/sleep 20\\n' > "
             + shlex.quote(directory + "/10-cancel-fixture.conf"))
        user("systemctl --user daemon-reload")
        own("amc-cancel-unrelated.service")
        own("amc-cancel-controller.service")
        user("systemd-run --user --unit=amc-cancel-unrelated.service -p Slice=app.slice -p RuntimeMaxSec=35 -- sleep 30")
        identity_files.add("/tmp/cancel-unit")
        user("systemd-run --user --service-type=exec --unit=amc-cancel-controller.service "
             "-p Slice=app.slice -p RuntimeMaxSec=35 -- amc launch --id cancel --profile proof "
             "--unit-file /tmp/cancel-unit --runtime-max-sec 10 -- touch /tmp/cancel-target-ran")
        machine.wait_for_file("/tmp/cancel-unit", timeout=5)
        unit = machine.succeed("cat /tmp/cancel-unit").strip()
        assert re.fullmatch(r"app-amc-[a-z0-9-]+@[a-f0-9]+\.service", unit)
        own(unit, "app.slice/app-amc.slice")
        deadline = time.monotonic() + 5
        while state(unit) != "activating":
            assert time.monotonic() < deadline, "fixture did not enter startup"
            time.sleep(0.02)
        before = properties(unit)
        record("cancellation-before.json", before)
        assert state(unit) == "activating"
        pattern = template.replace("@.service", "@*.service")
        listing = user("systemctl --user list-units --all --plain --no-legend --type=service " + shlex.quote(pattern))
        units = [line.split()[0] for line in listing.splitlines() if line.strip()]
        assert set(units) - {seed} == {unit}, "unexpected duplicate cancellation fixture"
        user("systemctl --user kill --kill-whom=main --signal=TERM amc-cancel-controller.service")
        wait_end("amc-cancel-controller.service", seconds=10)
        controller = properties("amc-cancel-controller.service")
        after = properties(unit)
        record("cancellation.json", {"oneObservedIdentity": unit, "controller": controller, "target": after})
        assert controller["properties"]["ExecMainStatus"] == "143", controller
        assert state(unit) in ("inactive", "failed")
        machine.fail("test -e /tmp/cancel-target-ran")
        user("systemctl --user is-active --quiet amc-cancel-unrelated.service")
        path = observed_cgroups[unit]
        machine.wait_until_succeeds(f'test ! -e /sys/fs/cgroup{path}/cgroup.events || grep -qx "populated 0" /sys/fs/cgroup{path}/cgroup.events', timeout=5)
        user("rm -- " + shlex.quote(directory + "/10-cancel-fixture.conf"))
        user("systemctl --user daemon-reload")

    with subtest("pinned-nixos-dropin-preserves-package-unit"):
        own("amc-native.service")
        user("systemctl --user start amc-native.service")
        report = properties("amc-native.service")
        record("native.json", report)
        props = report["properties"]
        preserved = native_preservation()
        record("native-preservation.json", preserved)
        assert all(preserved.values()), preserved
        assert props["Type"] == "simple" and props["Restart"] == "on-failure"
        assert props["Delegate"] == "yes" and props["OOMPolicy"] == "continue"
        assert props["MemoryMax"] == "268435456" and props["DropInPaths"]
        assert report["kernel"][0]["files"]["memory.max"]["value"] == 268435456
        pid = user("systemctl --user show amc-native.service -p MainPID --value")
        user("systemctl --user start amc-native.service")
        assert user("systemctl --user show amc-native.service -p MainPID --value") == pid
        # Verify ordinary dependencies without logging executable or environment.
        assert "basic.target" in user("systemctl --user show amc-native.service -p After --value")
        user("systemctl --user stop amc-native.service")
        # Home Manager commonly installs a higher-priority per-user symlink.
        # This checks lookup behavior, not evaluation of Home Manager modules
        # (this repository has no pinned Home Manager input).
        fragment = props["FragmentPath"]
        assert fragment.startswith("/")
        user("mkdir -p ~/.config/systemd/user")
        user(f"ln -s {shlex.quote(fragment)} ~/.config/systemd/user/amc-native.service")
        user("systemctl --user daemon-reload; systemctl --user start amc-native.service")
        user_report = properties("amc-native.service")
        record("native-user.json", user_report)
        preserved = native_preservation()
        record("native-user-preservation.json", preserved)
        assert all(preserved.values()), preserved
        assert user_report["properties"]["MemoryMax"] == "268435456", user_report
        assert user_report["properties"]["Restart"] == "on-failure", user_report
        assert user_report["kernel"][0]["files"]["memory.max"]["value"] == 268435456
        user("systemctl --user stop amc-native.service")

    with subtest("transient-versus-targeted-dropin-precedence"):
        winners = []
        for suffix, transient, dropin in [("one", 128, 96), ("two", 96, 128)]:
            unit = f"amc-precedence-{suffix}.service"
            own(unit)
            user(f"mkdir -p ~/.config/systemd/user/{unit}.d")
            user(f"printf '[Service]\\nMemoryMax={dropin}M\\nMemoryHigh=64M\\n' "
                 f"> ~/.config/systemd/user/{unit}.d/90-policy.conf")
            user("systemctl --user daemon-reload")
            user(f"systemd-run --user --unit={unit} --service-type=exec "
                 f"-p MemoryMax={transient}M -p RuntimeMaxSec=15 -- sleep 10")
            report = properties(unit)
            actual = int(report["properties"]["MemoryMax"])
            assert actual in (transient << 20, dropin << 20), report
            assert report["kernel"][0]["files"]["memory.max"]["value"] == actual, report
            assert report["properties"]["MemoryHigh"] == "67108864", report
            assert "90-policy.conf" in report["properties"]["DropInPaths"], report
            winners.append("dropin" if actual == dropin << 20 else "transient")
            # Preserve the actual pinned-version winner, do not invent precedence.
            user(f"amc inspect {unit} --json > /tmp/precedence-{suffix}.json")
            user(f"systemctl --user stop {unit}")
        assert winners[0] == winners[1], winners
        user("printf '%s' " + shlex.quote(json.dumps({"winner": winners[0], "testedMiB": [[128, 96], [96, 128]]})) + " > /tmp/precedence-winner.json")

    with subtest("nix-broker-boundary"):
        busybox = machine.succeed("readlink -f /run/current-system/sw/bin/busybox").strip()
        store = busybox.rsplit("/bin/", 1)[0]
        expression = (f'let busybox = builtins.storePath "{store}"; in '
                      'derivation { name = "amc-boundary"; system = builtins.currentSystem; '
                      'builder = "${busybox}/bin/busybox"; args = [ "sh" "-c" '
                      '"${busybox}/bin/busybox cat /proc/self/cgroup > $out" ]; }')
        inner = "cat /proc/self/cgroup > /tmp/client-cgroup; nix-build --no-substitute --expr " + shlex.quote(expression) + " > /tmp/builder-output"
        managed("sh -c " + shlex.quote(inner), "broker-unit", seconds=30)
        client_cgroup = machine.succeed("cat /tmp/client-cgroup")
        assert "app-amc-" in client_cgroup
        output = machine.succeed("cat /tmp/builder-output").strip()
        builder_cgroup = machine.succeed(f"cat {shlex.quote(output)}")
        record("broker.json", {"clientCgroup": client_cgroup.strip(), "builderCgroup": builder_cgroup.strip()})
        assert "nix-daemon.service" in builder_cgroup

    with subtest("attributed-memcg-oom-and-disappearing-evidence"):
        own("amc-unrelated.service")
        user("systemd-run --user --unit=amc-unrelated.service -p RuntimeMaxSec=60 -- sleep 55")
        # Trusted fixture setup waits for observer readiness. This is not a
        # new production launcher or a claim about arbitrary pre-exec activity.
        managed("sh -c " + shlex.quote(
            "sleep 30 & while ! test -f /tmp/release-oom; do sleep 0.02; done; "
            "exec amc test hog --report /tmp/hog-entry.json --expect-memory-max 256MiB "
            "--expect-memory-swap-max 0B --expect-memory-oom-group 1 "
            "--maximum 512MiB --chunk-size 4MiB --delay-ms 20"), "oom-unit", mode="launch", seconds=35)
        unit = machine.succeed("cat /tmp/oom-unit").strip()
        report = properties(unit)
        record("oom-before.json", report)
        path = report["properties"]["ControlGroup"]
        assert report["kernel"][0]["files"]["memory.oom.group"]["value"] == 1
        pids = machine.succeed(f"cat /sys/fs/cgroup{path}/cgroup.procs").split()
        assert len(pids) >= 2
        own("amc-observer.service")
        # Rust observer runs outside the tested failure domain in its own
        # unit; placement (shared ancestors) is recorded in manifest.json.
        user("systemd-run --user --unit=amc-observer.service -p RuntimeMaxSec=45 "
             f"-- amc watch {shlex.quote(unit)} --seconds 40 --interval-ms 20 --output /tmp/oom-watch")
        machine.wait_for_file("/tmp/oom-watch/ready", timeout=10)
        machine.fail("test -e /tmp/oom-watch/done")
        user("touch /tmp/release-oom")
        wait_end(unit)
        machine.wait_for_file("/tmp/oom-watch/done", timeout=10)
        evidence = json.loads(machine.succeed("cat /tmp/oom-watch/summary.json"))
        record("oom-result.json", properties(unit))
        assert evidence["eventDeltas"] is not None, evidence
        assert evidence["eventDeltas"].get("oom_kill") is not None, evidence
        assert evidence["eventDeltas"]["oom_kill"] > 0, evidence
        assert evidence["maxObservedSwap"] == 0, evidence
        assert user(f"systemctl --user show {unit} -p Result --value").strip() == "oom-kill"
        user("systemctl --user is-active --quiet amc-unrelated.service")
        record("unrelated.json", properties("amc-unrelated.service"))
        machine.wait_until_succeeds(
            f'test ! -e /sys/fs/cgroup{path}/cgroup.procs || test -z "$(cat /sys/fs/cgroup{path}/cgroup.procs)"',
            timeout=10)
        user(f"systemctl --user stop {unit}; systemctl --user reset-failed {unit}")
        missing = properties(unit)
        record("disappeared.json", missing)
        assert not missing["kernel"] or all(
            item["value"] is None for item in missing["kernel"][0]["files"].values())
finally:
    primary = sys.exc_info()[0]
    try:
        finalization = finalize_run(machine, prefix, owned, identity_files, observed_cgroups, artifact_dir,
                                    primary.__name__ if primary else None, recorded_artifacts)
    except Exception as error:
        emit("AMC_FINALIZATION " + json.dumps({"primaryFailure": primary.__name__ if primary else None,
                                               "status": "UNKNOWN", "reason": type(error).__name__}))
        if primary is None:
            raise
    else:
        if primary is None:
            assert finalization["complete"], "artifact export or owned-fixture cleanup incomplete; see finalization.json"
