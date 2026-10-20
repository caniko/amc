import json
import shlex
import time

# Appended after the shared driver; require its initialized native fixture.
machine = globals()["machine"]
test_section = globals()["test_section"]
wait_committed = globals()["wait_committed"]
launch = globals()["launch"]
wait_entered = globals()["wait_entered"]
status = globals()["status"]
stop_scope = globals()["stop_scope"]
evidence = globals()["evidence"]

ready_token = "0123456789abcdef0123456789abcdef"


def queued_status():
    return json.loads(machine.succeed(
        "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 amc admission status --json"
    ))


def launch_patient(name, timeout=None):
    machine.succeed(
        f"systemd-run --unit={name}-client --uid=1001 "
        f"--property=StandardError=file:/tmp/{name}-stderr "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1001 -- "
        "amc admission exec --contract tool --queue --max-ram-usage 96MiB --runtime-max-sec 2 "
        f"--ready-token {ready_token} "
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


with test_section("native patient queue overflow is typed and preserves every accepted ticket"):
    wait_committed(0)
    assert not queued_status()["entries"]
    receipt = json.loads(machine.succeed("runuser -u bob -- python3 - <<'PY'\n" + """
import json
import socket
from pathlib import Path

def rpc(op, **fields):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(5)
        client.connect('/run/user/1001/amc/admission.sock')
        client.sendall((json.dumps({'version': 1, 'message': {'op': op, **fields}}) + '\\n').encode())
        with client.makefile('rb') as stream:
            return json.loads(stream.readline(262145))

policy = json.loads(Path('/etc/amc-test-user-policy.json').read_text())
request = {'memory_max': 96 * 1048576, 'runtime_max_sec': 2, 'wait_ms': None}
accepted = []
try:
    for _ in range(policy['queue_limit']):
        response = rpc('enqueue_waiting', contract='tool', request=request)
        assert response['error'] is None and response.get('entry_key'), response
        accepted.append(response['entry']['id'])
    for _ in range(3):
        response = rpc('enqueue_waiting', contract='tool', request=request)
        assert response.get('backpressure') == {'retry_after_ms': 250}, response
        assert response.get('entry') is None and response.get('entry_key') is None, response
        entries = rpc('status')['status']['entries']
        assert {entry['id'] for entry in entries} == set(accepted), entries
        assert all(entry['identity'] is None for entry in entries), entries
    invalid = rpc('enqueue_waiting', contract='tool', request={**request, 'memory_max': 128 * 1048576})
    assert invalid['error'] and invalid.get('backpressure') is None and invalid.get('entry') is None, invalid
    print(json.dumps({'accepted': len(accepted), 'overflowAttempts': 3, 'retryAfterMs': 250}))
finally:
    for ticket in accepted:
        response = rpc('cancel', id=ticket)
        assert response['error'] is None, response
""" + "\nPY"))
    assert receipt == {"accepted": 16, "overflowAttempts": 3, "retryAfterMs": 250}, receipt
    assert not queued_status()["entries"]
    wait_committed(0)
    evidence["patientQueueOverflow"] = receipt


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
    # The helper explains its host wait once; no ready frame precedes entry.
    waiting_stderr = machine.succeed("cat /tmp/patient-long-stderr")
    assert "\x1e" not in waiting_stderr, waiting_stderr
    machine.succeed("touch /tmp/patient-blocker-finish")
    wait_entered("patient-long")
    machine.wait_until_succeeds("test -e /tmp/patient-long-completed", timeout=15)
    machine.wait_until_succeeds(
        'test "$(systemctl show patient-long-client --property=ExecMainStatus --value)" = 0 '
        '&& test "$(systemctl show patient-long-client --property=ActiveState --value)" = inactive', timeout=15,
    )
    wait_committed(0)
    stderr = machine.succeed("cat /tmp/patient-long-stderr")
    entered, completed = f"\x1e{ready_token}\x1f", f"\x1e{ready_token}:complete:0\x1f"
    # Exactly one entry frame, then the token-bound completion proof.
    assert stderr.count("\x1e") == 2 and stderr.index(entered) < stderr.index(completed), stderr
    diagnostic = stderr.replace(entered, "", 1).replace(completed, "", 1)
    assert all(line.startswith("waiting for host capacity: ") for line in diagnostic.splitlines()), stderr
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
        assert ready_token not in machine.succeed(f"cat /tmp/{name}-stderr")
        machine.execute(f"systemctl stop {name}-client")
    machine.succeed("systemctl stop patient-cleanup-blocker-client")
    for grant in status()["reservations"]:
        stop_scope(grant)
    wait_committed(0)
    for mode in ["deadline", "cancel", "restart"]:
        machine.fail(f"test -e /tmp/patient-{mode}-entered")
    evidence["patientQueue"]["noPayloadAfter"] = ["deadline", "cancel", "restart"]
