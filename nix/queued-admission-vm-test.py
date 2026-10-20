import time


def queued_status():
    return json.loads(machine.succeed(
        "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 amc admission status --json"
    ))


def launch_patient(name, timeout=None):
    machine.succeed(
        f"systemd-run --unit={name}-client --uid=1001 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1001 -- "
        "amc admission exec --contract tool --queue --max-ram-usage 96MiB --runtime-max-sec 2 "
        + (f"--timeout {timeout} " if timeout else "")
        + "-- /bin/sh -c "
        + shlex.quote(f"touch /tmp/{name}-entered; sleep .25; touch /tmp/{name}-completed")
    )
    machine.wait_until_succeeds(
        "amc admission host-status | python3 -c " + shlex.quote(
            'import sys,json; s=json.load(sys.stdin); assert any(not r["granted"] and r["identity"]["uid"] == 1001 for r in s["reservations"])'
        ), timeout=30,
    )
    entries = queued_status()["entries"]
    assert len(entries) == 1 and entries[0]["phase"] == "starting", entries
    return entries[0]


with test_section("patient host waiting exceeds 120 seconds without spending native execution time"):
    wait_committed(0)
    launch("patient-blocker", 1000, "tool",
           "touch /tmp/patient-blocker-entered; while ! test -e /tmp/patient-blocker-finish; do sleep .1; done")
    wait_entered("patient-blocker")
    entry = launch_patient("patient-long")
    unit = "app-amc-job-" + entry["id"] + ".service"
    properties = dict(line.split("=", 1) for line in machine.succeed(
        f"systemctl --user --machine=1001@.host show {unit} --property=Type,ActiveState,RuntimeMaxUSec,NotifyAccess"
    ).splitlines())
    assert properties == {"Type": "notify", "ActiveState": "activating", "RuntimeMaxUSec": "2s", "NotifyAccess": "main"}, properties
    latencies = []
    for _ in range(20):
        before = time.monotonic()
        observed = status()
        latencies.append(time.monotonic() - before)
        assert observed["committed_bytes"] == 96 * 1048576, observed
    p95 = sorted(latencies)[18]
    assert p95 <= 1.0, latencies
    machine.sleep(125)
    assert queued_status()["entries"][0]["phase"] == "starting"
    machine.fail("test -e /tmp/patient-long-entered")
    machine.succeed("touch /tmp/patient-blocker-finish")
    wait_entered("patient-long")
    machine.wait_until_succeeds("test -e /tmp/patient-long-completed", timeout=15)
    machine.wait_until_succeeds(
        'test "$(systemctl show patient-long-client --property=ExecMainStatus --value)" = 0 '
        '&& test "$(systemctl show patient-long-client --property=ActiveState --value)" = inactive', timeout=15,
    )
    wait_committed(0)
    evidence["patientQueue"] = {"waitSeconds": 125, "runtimeSeconds": 2,
        "controlP95Seconds": p95, "completed": True}

with test_section("finite host waiting, cancellation and private restart never run a queued payload"):
    launch("patient-cleanup-blocker", 1000, "tool",
           "touch /tmp/patient-cleanup-blocker-entered; sleep 120")
    wait_entered("patient-cleanup-blocker")
    for mode in ["deadline", "cancel", "restart"]:
        name = f"patient-{mode}"
        launch_patient(name, timeout=3 if mode == "deadline" else None)
        if mode == "cancel":
            machine.succeed(f"systemctl kill --signal=TERM --kill-whom=main {name}-client")
        if mode == "restart":
            machine.succeed("systemctl --user --machine=1001@.host restart amc-admission")
        machine.wait_until_succeeds(
            "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 amc admission status --json | python3 -c "
            + shlex.quote('import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'), timeout=30,
        )
        machine.wait_until_succeeds(
            "amc admission host-status | python3 -c " + shlex.quote(
                'import sys,json; assert all(r["identity"]["uid"] != 1001 for r in json.load(sys.stdin)["reservations"])'
            ), timeout=30,
        )
        machine.fail(f"test -e /tmp/{name}-entered")
        machine.execute(f"systemctl stop {name}-client")
    machine.succeed("systemctl stop patient-cleanup-blocker-client")
    for grant in status()["reservations"]:
        stop_scope(grant)
    wait_committed(0)
    for mode in ["deadline", "cancel", "restart"]:
        machine.fail(f"test -e /tmp/patient-{mode}-entered")
    evidence["patientQueue"]["noPayloadAfter"] = ["deadline", "cancel", "restart"]
