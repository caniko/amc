# Appended to the shared-admission driver after its foreground/swap scenarios.
with test_section(
    "finite completion children finish a drain while newer root worker serials wait"
):
    completion_policy = json.loads(
        machine.succeed("cat /etc/amc-test-host-policy.json")
    )
    completion_policy.pop("swap_recovery")
    completion_policy["domains"][0]["continuation"] = {
        "parent_max_bytes": 32 * 1048576,
        "memory_bytes": 96 * 1048576,
        "swap_bytes": 0,
        "max_calls": 2,
        "domains": ["builders"],
    }
    write_file("/tmp/completion-policy.json", json.dumps(completion_policy))
    machine.succeed(
        "systemd-run --unit=completion-host -- amc admission host-serve "
        "--policy /tmp/completion-policy.json --socket /run/amc-completion/admission.sock "
        "--state /var/lib/amc-completion"
    )
    machine.wait_until_succeeds("test -S /run/amc-completion/admission.sock")

    def completion_status():
        return json.loads(
            machine.succeed(
                "amc admission host-status --socket /run/amc-completion/admission.sock"
            )
        )

    parent = """import json, os, socket, time
from pathlib import Path
deadline = time.monotonic() + 60
last_wait = None
while time.monotonic() < deadline:
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        connection.connect('/run/amc-completion/admission.sock')
        connection.sendall(b'{"op":"acquire","version":1,"wait_ms":60000}\\n')
        response = json.loads(connection.makefile().readline())
    assert response['error'] is None, response
    observation = {key: response.get(key) for key in
                   ('granted', 'waiting', 'committed_bytes', 'swap_return_bytes')}
    Path('/tmp/completion-parent-status.json').write_text(json.dumps(observation))
    if observation != last_wait:
        print('completion-parent-admission', json.dumps(observation), flush=True)
        last_wait = observation
    if response['granted']:
        break
    time.sleep(.25)
else:
    raise AssertionError(('completion parent admission timed out', last_wait))
assert response['continuation'] is not None, response
stat = Path('/proc/self/stat').read_text().rsplit(') ', 1)[1].split()
Path('/tmp/completion-origin.json').write_text(json.dumps({'pid':os.getpid(),'start_ticks':int(stat[19])}))
while not Path('/tmp/completion-parent-finish').exists():
    time.sleep(.1)
"""
    write_file("/tmp/completion-parent.py", parent)
    machine.succeed(
        "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 systemd-run --user "
        "--unit=app-amc-job-completion-parent --property=Slice=agent-tools.slice "
        "--property=MemoryMax=32M --property=MemorySwapMax=0 --property=Restart=no "
        "--property=KillMode=control-group --property=OOMPolicy=kill -- python3 /tmp/completion-parent.py"
    )
    try:
        machine.wait_until_succeeds("test -s /tmp/completion-origin.json", timeout=75)
    except Exception:
        machine.succeed(
            "journalctl --no-pager _SYSTEMD_UNIT=completion-host.service + "
            "_SYSTEMD_USER_UNIT=app-amc-job-completion-parent.service; "
            "cat /tmp/completion-parent-status.json; "
            "amc admission host-status --socket /run/amc-completion/admission.sock"
        )
        diagnostics = """import json
from pathlib import Path
policy = json.loads(Path('/tmp/completion-policy.json').read_text())
root = Path('/sys/fs/cgroup')
paths = set()
for domain in policy['domains']:
    directory = root / domain['cgroup'].lstrip('/')
    paths.update(path for path in (directory, *directory.parents) if root in path.parents)
for directory in sorted(paths):
    print(directory, flush=True)
    for name in ('memory.max', 'memory.current', 'memory.swap.max', 'memory.swap.current', 'cgroup.events'):
        try:
            print(name, (directory / name).read_text().strip(), flush=True)
        except OSError as error:
            print(name, str(error), flush=True)
for line in Path('/proc/meminfo').read_text().splitlines():
    if line.split(':', 1)[0] in ('MemAvailable', 'SwapTotal', 'SwapFree', 'SwapCached'):
        print(line, flush=True)
"""
        write_file("/tmp/completion-diagnostics.py", diagnostics)
        machine.succeed("python3 /tmp/completion-diagnostics.py")
        raise
    parent_charge = completion_status()["committed_bytes"]
    assert parent_charge == 128 * 1048576, parent_charge

    machine.succeed(
        "systemd-run --unit=completion-game-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "amc prepare --socket /run/amc-completion/admission.sock --profile game -- /bin/sh -c "
        + shlex.quote("touch /tmp/completion-game-entered; sleep 120")
    )
    machine.wait_until_succeeds(
        "amc admission host-status --socket /run/amc-completion/admission.sock | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin)["preparation_barrier"]'
        )
    )

    worker = """import json, socket, time
from pathlib import Path
origin = json.loads(Path('/tmp/completion-origin.json').read_text())
def call(op, serial, with_origin=False):
    request = {'op':op,'version':1,'domain':'builders','operation':serial}
    if op == 'acquire_pool':
        request['wait_ms'] = 60000
    if with_origin:
        request['origin'] = origin
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        connection.connect('/run/amc-completion/admission.sock')
        connection.sendall((json.dumps(request)+'\\n').encode())
        response = json.loads(connection.makefile().readline())
    assert response['error'] is None, response
    return response
while not call('acquire_pool',1,True)['granted']:
    time.sleep(.25)
assert call('acquire_pool',2,True)['granted']
assert call('release_pool',999)['granted']
Path('/tmp/completion-worker-entered').touch()
while not Path('/tmp/completion-inner-finish').exists():
    time.sleep(.1)
assert call('release_pool',2)['granted']
Path('/tmp/completion-inner-released').touch()
while not Path('/tmp/completion-outer-finish').exists():
    time.sleep(.1)
assert call('release_pool',1)['granted']
response = call('acquire_pool',3)
assert not response['granted'] and response['waiting'] == 'preparation', response
Path('/tmp/completion-new-serial.json').write_text(json.dumps({'granted':response['granted'],'waiting':response['waiting']}))
# The idle handler remains alive after the finite operation ends.
time.sleep(120)
"""
    write_file("/tmp/completion-worker.py", worker)
    machine.succeed(
        "systemd-run --unit=completion-worker -- python3 /tmp/completion-worker.py"
    )
    machine.wait_until_succeeds("test -e /tmp/completion-worker-entered")
    transferred_charge = completion_status()["committed_bytes"]
    assert transferred_charge == parent_charge, transferred_charge
    machine.succeed(
        "systemd-run --unit=completion-descendant --slice=builders.slice -- sleep 120"
    )
    machine.wait_until_succeeds(
        "grep -q '^populated 1$' /sys/fs/cgroup/builders.slice/cgroup.events"
    )
    machine.succeed("systemctl restart completion-host")
    machine.wait_until_succeeds("test -S /run/amc-completion/admission.sock")
    machine.succeed("touch /tmp/completion-inner-finish")
    machine.wait_until_succeeds("test -e /tmp/completion-inner-released")
    pool = next(
        r for r in completion_status()["reservations"] if r["domain"] == "builders"
    )
    assert len(pool["owners"]) == 1 and not pool.get("owners_finished", False), pool
    machine.fail("test -e /tmp/completion-game-entered")

    machine.succeed("touch /tmp/completion-parent-finish")
    machine.wait_until_succeeds(
        "amc admission host-status --socket /run/amc-completion/admission.sock | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 100663296'
        )
    )
    post_parent_charge = completion_status()["committed_bytes"]
    machine.succeed("touch /tmp/completion-outer-finish")
    machine.wait_until_succeeds("test -s /tmp/completion-new-serial.json")
    pool = next(
        r for r in completion_status()["reservations"] if r["domain"] == "builders"
    )
    assert pool["owners"] == [] and pool["owners_finished"], pool
    assert completion_status()["committed_bytes"] == post_parent_charge
    machine.succeed("systemctl is-active completion-worker")
    machine.fail("test -e /tmp/completion-game-entered")
    machine.succeed("systemctl stop completion-descendant")
    machine.wait_until_succeeds("test -e /tmp/completion-game-entered")
    state = completion_status()
    assert state["committed_bytes"] == 64 * 1048576, state
    game = next(r for r in state["reservations"] if r["domain"] == "foreground-1000")
    stop_scope(game)
    machine.succeed(
        "systemctl stop completion-worker completion-game-client completion-host"
    )
    evidence["completion"] = {
        "parentAndEscrowBytes": parent_charge,
        "transferredBytes": transferred_charge,
        "postParentBytes": post_parent_charge,
        "outerRetainedAcrossRestart": True,
        "ownersFinishedWithDescendant": True,
        "newSerialGranted": False,
        "gameMemoryBytes": game["memory_bytes"],
    }

machine.succeed("test $(awk '$1 == \"oom_kill\" {print $2}' /proc/vmstat) = 0")
write_file("/tmp/amc-foreground-evidence.json", json.dumps(evidence))
machine.copy_from_machine("/tmp/amc-foreground-evidence.json", "shared-admission")
