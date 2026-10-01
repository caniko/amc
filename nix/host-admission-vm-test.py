import json
import shlex

# Injected by NixOS's native test driver.
machine = globals()["machine"]
test_section = globals()["subtest"]
globals()["start_all"]()
machine.wait_for_unit("amc-host-admission.service")
for user, uid in [("alice", 1000), ("bob", 1001)]:
    machine.succeed(f"loginctl enable-linger {user}; systemctl start user@{uid}.service")
    machine.wait_until_succeeds(f"test -S /run/user/{uid}/amc/admission.sock")


def write_file(path, text):
    machine.succeed("printf '%s' " + shlex.quote(text) + " > " + shlex.quote(path))


def status():
    return json.loads(machine.succeed("amc admission host-status"))


def wait_committed(amount):
    machine.wait_until_succeeds("amc admission host-status | python3 -c " + shlex.quote(
        f'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == {amount}'))


def launch(name, uid, contract, command):
    machine.succeed(f"systemd-run --unit={name}-client --uid={uid} "
                    f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
                    f"amc admission exec --contract {contract} --timeout 60 -- sh -c " + shlex.quote(command))


with test_section("durable root pool tracks every potential execution owner"):
    # Owners are outside this empty bounded slice, just like Nix's handlers.
    write_file("/tmp/pool-owner.py", '''import json, socket, sys, time
while True:
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(2)
        s.connect("/run/amc-host/admission.sock")
        s.sendall(b'{"op":"acquire_pool","version":1,"wait_ms":60000,"domain":"builders"}\\n')
        r = json.loads(s.makefile().readline())
    assert not r["error"], r
    if r["granted"]:
        open(sys.argv[1], "w").write(r["ticket"])
        break
    time.sleep(.25)
time.sleep(120)
''')
    for name in ["first", "second"]:
        machine.succeed(f"systemd-run --unit=pool-{name} -- python3 /tmp/pool-owner.py /tmp/pool-{name}")
        machine.wait_until_succeeds(f"test -s /tmp/pool-{name}")
    wait_committed(96 * 1048576)
    assert len(status()["reservations"]) == 1
    assert len(status()["reservations"][0]["owners"]) == 2
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    wait_committed(96 * 1048576)
    machine.succeed("systemctl stop pool-first")
    machine.sleep(1)
    wait_committed(96 * 1048576)
    machine.succeed("systemctl stop pool-second")
    wait_committed(0)

with test_section("helper cannot omit host capacity before private entry"):
    write_file("/tmp/omit-host.py", '''import json, socket, subprocess, time
path = "/run/user/1001/amc/admission.sock"
def call(op):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(2)
        s.connect(path)
        s.sendall((json.dumps({"version":1,"message":op}) + "\\n").encode())
        r = json.loads(s.makefile().readline())
    assert not r["error"], r
    return r
entry = call({"op":"enqueue","contract":"small","wait_ms":10000})["entry"]
for _ in range(40):
    entry = call({"op":"poll","id":entry["id"]})["entry"]
    if entry["phase"] == "reserved":
        break
    time.sleep(.25)
assert entry["phase"] == "reserved", entry
r = subprocess.run(["systemd-run","--user","--wait","--pipe","--service-type=exec",
    "--unit=app-amc-job-"+entry["id"]+".service","--property=Slice=agent-tools.slice",
    "--property=MemoryMax=33554432","--property=MemorySwapMax=0","--property=Restart=no",
    "--property=KillMode=control-group","--property=OOMPolicy=kill","--","amc","admission","enter","--socket",path,
    "--ticket",entry["id"],"--","touch","/tmp/bypassed"], capture_output=True, text=True)
assert r.returncode != 0, r
assert "native entry has no matching durable host reservation" in r.stdout + r.stderr, r
call({"op":"cancel","id":entry["id"]})
''')
    machine.succeed("runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 python3 /tmp/omit-host.py")
    machine.fail("test -f /tmp/bypassed")
    wait_committed(0)

with test_section("simultaneous users, restart persistence, and automatic lending"):
    launch("alice", 1000, "tool", "touch /tmp/alice-entered; while ! test -e /tmp/alice-finish; do sleep .1; done")
    machine.wait_until_succeeds("test -f /tmp/alice-entered")
    wait_committed(96 * 1048576)
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    wait_committed(96 * 1048576)
    launch("bob", 1001, "tool", "touch /tmp/bob-entered; while ! test -e /tmp/bob-finish; do sleep .1; done")
    machine.sleep(1)
    machine.fail("test -f /tmp/bob-entered")
    own = json.loads(machine.succeed("runuser -u bob -- amc admission host-status"))
    assert own["reservations"] and all(r["identity"]["uid"] == 1001 for r in own["reservations"])
    machine.succeed("touch /tmp/alice-finish")
    machine.wait_until_succeeds("test -f /tmp/bob-entered")
    wait_committed(96 * 1048576)
    machine.succeed("touch /tmp/bob-finish")
    wait_committed(0)

with test_section("changed native enforcement inhibits a fitting smaller job"):
    launch("held", 1000, "tool", "touch /tmp/held-entered; sleep 120")
    machine.wait_until_succeeds("test -f /tmp/held-entered")
    group = "/sys/fs/cgroup" + status()["reservations"][0]["identity"]["cgroup"]
    machine.succeed(f"echo 100663297 > {group}/memory.max")
    launch("small", 1001, "small", "touch /tmp/small-entered; sleep 120")
    machine.sleep(1)
    machine.fail("test -f /tmp/small-entered")
    machine.succeed(f"echo 100663296 > {group}/memory.max")
    machine.wait_until_succeeds("test -f /tmp/small-entered")
    wait_committed(128 * 1048576)
    machine.succeed("systemctl stop held-client small-client")
    wait_committed(0)

with test_section("cancelled pending work never executes after native cleanup"):
    launch("blocking", 1000, "tool", "touch /tmp/blocking-entered; sleep 120")
    machine.wait_until_succeeds("test -f /tmp/blocking-entered")
    launch("cancelled", 1001, "tool", "touch /tmp/cancelled-entered")
    machine.sleep(1)
    machine.fail("test -f /tmp/cancelled-entered")
    machine.succeed("systemctl stop cancelled-client blocking-client")
    wait_committed(0)
    machine.sleep(1)
    machine.fail("test -f /tmp/cancelled-entered")

machine.succeed("test $(awk '$1 == \"oom_kill\" {print $2}' /proc/vmstat) = 0")
