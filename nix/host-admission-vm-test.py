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
        f'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == {amount}'), timeout=30)


def launch(name, uid, contract, command):
    machine.succeed(f"systemd-run --unit={name}-client --uid={uid} "
                    "--setenv=PATH=/run/current-system/sw/bin "
                    f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
                    f"amc admission exec --contract {contract} --timeout 60 -- /bin/sh -c " + shlex.quote(command))


def wait_entered(name, host_socket=None):
    try:
        machine.wait_until_succeeds(f"test -f /tmp/{name}-entered", timeout=30)
    except Exception as error:
        if host_socket is not None:
            queries = {
                "host": "amc admission host-status --socket " + shlex.quote(host_socket),
                "client": f"journalctl -b --no-pager -o cat -u {name}-client -n 12",
                "brokers": "journalctl -b --no-pager -o cat -u normal-host -u normal-private-1000 -u normal-private-1001 -n 12",
            }
            for uid in [1000, 1001]:
                queries[str(uid)] = (f"runuser -u {'alice' if uid == 1000 else 'bob'} -- "
                                    f"env XDG_RUNTIME_DIR=/run/user/{uid} amc admission status --json "
                                    f"--socket /run/user/{uid}/amc/normal.sock")
            evidence = {key: machine.execute(command) for key, command in queries.items()}
            # Keep the relevant endpoints and client in the final Nix log tail.
            raise AssertionError(json.dumps({"payload": name, "evidence": evidence})) from error
        print(machine.succeed("journalctl -b --no-pager -n 100"))
        print(machine.succeed("amc admission host-status"))
        raise


with test_section("rapid native and admitted jobs preserve completion and fail closed on startup loss"):
    user = "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 "
    for mode in ["", " --admission"]:
        receipt = json.loads(machine.succeed(user + "python3 /etc/amc-native-completion.py amc" + mode, timeout=180))
        assert [r["exit"] for r in receipt["rapid_jobs"]] == [0, 42, 0, 42], receipt
        assert receipt["startup_loss"] == {"disconnect": "no-exec", "invalid-ack": "no-exec"}, receipt
    wait_committed(0)

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
    machine.succeed("systemd-run --unit=pool-child --slice=builders.slice -- sleep 120")
    machine.wait_until_succeeds("grep -q '^populated 1$' /sys/fs/cgroup/builders.slice/cgroup.events")
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    wait_committed(96 * 1048576)
    machine.succeed("systemctl stop pool-first")
    machine.sleep(1)
    wait_committed(96 * 1048576)
    machine.succeed("systemctl stop pool-second")
    machine.sleep(1)
    wait_committed(96 * 1048576)
    machine.succeed("systemctl stop pool-child")
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
ticket = call({"op":"enqueue","contract":"small","wait_ms":10000})
entry = ticket["entry"]
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
    "--ticket",entry["id"],"--entry-key",ticket["entry_key"],"--","touch","/tmp/bypassed"], capture_output=True, text=True)
assert r.returncode != 0, r
assert "native entry has no matching durable host reservation" in r.stdout + r.stderr, r
call({"op":"cancel","id":entry["id"]})
''')
    machine.succeed("runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 python3 /tmp/omit-host.py")
    machine.fail("test -f /tmp/bypassed")
    wait_committed(0)

with test_section("simultaneous users, restart persistence, and automatic lending"):
    launch("alice", 1000, "tool", "touch /tmp/alice-entered; while ! test -e /tmp/alice-finish; do sleep .1; done")
    wait_entered("alice")
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
    wait_entered("bob")
    wait_committed(96 * 1048576)
    machine.succeed("touch /tmp/bob-finish")
    wait_committed(0)

with test_section("changed native enforcement inhibits a fitting smaller job"):
    launch("held", 1000, "tool", "touch /tmp/held-entered; sleep 120")
    wait_entered("held")
    group = "/sys/fs/cgroup" + status()["reservations"][0]["identity"]["cgroup"]
    # The kernel converts byte limits to page counts; +1 byte leaves the
    # effective limit unchanged. Prove a distinct page-aligned ceiling.
    machine.succeed(f"echo 101711872 > {group}/memory.max")
    assert machine.succeed(f"cat {group}/memory.max").strip() == "101711872"
    launch("small", 1001, "small", "touch /tmp/small-entered; sleep 120")
    machine.sleep(1)
    machine.fail("test -f /tmp/small-entered")
    machine.succeed(f"echo 100663296 > {group}/memory.max")
    assert machine.succeed(f"cat {group}/memory.max").strip() == "100663296"
    wait_entered("small")
    wait_committed(128 * 1048576)
    machine.succeed("systemctl stop held-client small-client")
    wait_committed(0)

with test_section("cancelled pending work never executes after native cleanup"):
    launch("blocking", 1000, "tool", "touch /tmp/blocking-entered; sleep 120")
    wait_entered("blocking")
    launch("cancelled", 1001, "tool", "touch /tmp/cancelled-entered")
    machine.sleep(1)
    machine.fail("test -f /tmp/cancelled-entered")
    machine.succeed("systemctl stop cancelled-client blocking-client")
    wait_committed(0)
    machine.sleep(1)
    machine.fail("test -f /tmp/cancelled-entered")

with test_section("short native bursts exceed the normal budget and retain restart accounting"):
    for name, uid, contract in [("full-a", 1000, "tool"), ("full-b", 1001, "small"), ("full-c", 1000, "small")]:
        launch(name, uid, contract, f"touch /tmp/{name}-entered; sleep 120")
        wait_entered(name)
    wait_committed(160 * 1048576)

    def burst(name, uid, command, runtime=5):
        machine.succeed(f"systemd-run --unit={name}-client --uid={uid} "
                        "--setenv=PATH=/run/current-system/sw/bin "
                        f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
                        f"amc exec --burst --max-ram-usage 32MiB --runtime-max-sec {runtime} --timeout 30 -- "
                        "/bin/sh -c " + shlex.quote(command))

    burst("burst-a", 1000, "touch /tmp/burst-a-entered; trap '' TERM; sleep 120 & wait")
    wait_entered("burst-a")
    state = status()
    grant = next(r for r in state["reservations"] if r.get("burst"))
    assert state["committed_bytes"] > state["budget_bytes"]
    assert state["burst_committed_bytes"] == 32 * 1048576
    assert grant["runtime_max_ms"] == 5000 and grant["swap_bytes"] == 0
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    restored = status()
    assert any(r["id"] == grant["id"] and r["granted"] for r in restored["reservations"])
    burst("burst-b", 1001, "touch /tmp/burst-b-entered; trap '' TERM; sleep 120 & wait")
    wait_entered("burst-b")
    wait_committed(160 * 1048576)
    # RuntimeMaxSec and bounded final SIGKILL clean descendants without releasing
    # the still-running normal jobs. A new call cannot erase the durable cooldown.
    burst("burst-rate", 1001, "touch /tmp/burst-rate-entered", runtime=1)
    machine.wait_until_succeeds("systemctl show burst-rate-client --property=ActiveState --value | grep -qE 'inactive|failed'", timeout=15)
    machine.fail("test -f /tmp/burst-rate-entered")
    assert status()["burst_committed_bytes"] == 0
    machine.succeed("systemctl stop full-a-client full-b-client full-c-client")
    wait_committed(0)

with test_section("sized ordinary calls use the normal budget with bursts disabled"):
    # Isolated endpoints use the same native domains, with both policy layers
    # explicitly stripped of burst capacity. No jobs overlap the primary broker.
    machine.succeed("python3 -c " + shlex.quote('''
from pathlib import Path
import json
host = json.loads(Path("/etc/amc-test-host-policy.json").read_text())
host.pop("burst")
host["domains"] = [d for d in host["domains"] if not d.get("burst")]
user = json.loads(Path("/etc/amc-test-user-policy.json").read_text())
user.pop("burst_budget_bytes")
user["contracts"].pop("tool-burst")
Path("/tmp/amc-normal-host.json").write_text(json.dumps(host))
Path("/tmp/amc-normal-user.json").write_text(json.dumps(user))
'''))
    machine.succeed("systemd-run --unit=normal-host -- amc admission host-serve "
                    "--policy /tmp/amc-normal-host.json --socket /run/amc-normal/admission.sock --state /var/lib/amc-normal")
    machine.wait_until_succeeds("test -S /run/amc-normal/admission.sock")
    for user, uid in [("alice", 1000), ("bob", 1001)]:
        machine.succeed(f"systemd-run --unit=normal-private-{uid} --uid={uid} "
                        f"--setenv=HOME=/home/{user} --setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
                        "amc admission serve --policy /tmp/amc-normal-user.json "
                        f"--socket /run/user/{uid}/amc/normal.sock --state /home/{user}/.local/state/amc-normal "
                        "--host-socket /run/amc-normal/admission.sock")
        machine.wait_until_succeeds(f"test -S /run/user/{uid}/amc/normal.sock")
    for index, uid in enumerate([1000, 1001, 1000, 1001, 1000, 1001]):
        machine.succeed(f"systemd-run --unit=sized-{index}-client --uid={uid} "
                        f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
                        f"amc admission exec --socket /run/user/{uid}/amc/normal.sock --contract tool "
                        "--max-ram-usage 32MiB --runtime-max-sec 120 --timeout 30 -- /bin/sh -c " + shlex.quote(
                            f"touch /tmp/sized-{index}-entered; sleep 120"))
        if index < 5:
            wait_entered(f"sized-{index}", "/run/amc-normal/admission.sock")
    machine.sleep(1)
    machine.fail("test -e /tmp/sized-5-entered")
    normal = json.loads(machine.succeed("amc admission host-status --socket /run/amc-normal/admission.sock"))
    assert normal["committed_bytes"] == normal["budget_bytes"] == 160 * 1048576, normal
    assert normal["burst_budget_bytes"] == normal["burst_committed_bytes"] == 0, normal
    assert sum(r["granted"] for r in normal["reservations"]) == 5, normal
    assert all(r["memory_bytes"] == 32 * 1048576 and not r.get("burst") for r in normal["reservations"]), normal
    for r in normal["reservations"]:
        machine.succeed(f"test $(cat /sys/fs/cgroup{r['identity']['cgroup']}/memory.max) = 33554432")
    machine.succeed("systemctl stop sized-0-client")
    wait_entered("sized-5", "/run/amc-normal/admission.sock")
    machine.succeed("systemctl stop sized-1-client sized-2-client sized-3-client sized-4-client sized-5-client")
    machine.wait_until_succeeds("amc admission host-status --socket /run/amc-normal/admission.sock | python3 -c " + shlex.quote(
        'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'), timeout=30)
    machine.succeed("systemctl stop normal-private-1000 normal-private-1001 normal-host")

with test_section("burst client loss and private restart retain grants until cleanup is observable"):
    # Previous bursts deliberately persisted their cooldowns.
    machine.sleep(10)
    burst("burst-loss", 1000, "touch /tmp/burst-loss-entered; trap '' TERM; sleep 120 & wait")
    wait_entered("burst-loss")
    grant = next(r for r in status()["reservations"] if r.get("burst"))
    group = "/sys/fs/cgroup" + grant["identity"]["cgroup"]
    machine.succeed(f"grep -q '^populated 1$' {group}/cgroup.events")
    machine.succeed("systemctl --user --machine=1000@.host restart amc-admission")
    private = "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 amc admission status --json"
    restored = json.loads(machine.succeed(private))
    assert restored["committed_bytes"] == 32 * 1048576, restored
    assert any(e["phase"] == "running" and e["contract"]["burst"] for e in restored["entries"]), restored
    machine.succeed("touch /tmp/amc-observation-unavailable")
    machine.succeed("systemctl kill --kill-whom=main --signal=KILL burst-loss-client")
    # Real systemd deadlines clean the entire cgroup while the private broker
    # lacks evidence. Neither client death, restart nor expiry may free its grant.
    wait_committed(0)
    machine.wait_until_succeeds(private + " | python3 -c " + shlex.quote(
        'import sys,json; s=json.load(sys.stdin); assert s["committed_bytes"] == 33554432 and s["unreconciled"]'), timeout=10)
    machine.succeed(f"test ! -e {group} || grep -q '^populated 0$' {group}/cgroup.events")
    machine.succeed("rm /tmp/amc-observation-unavailable")
    machine.wait_until_succeeds(private + " | python3 -c " + shlex.quote(
        'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'), timeout=10)

with test_section("explicit burst cancellation confirms descendant cleanup"):
    burst("burst-cancel", 1001, "touch /tmp/burst-cancel-entered; trap '' TERM; sleep 120 & wait")
    wait_entered("burst-cancel")
    grant = next(r for r in status()["reservations"] if r.get("burst"))
    group = "/sys/fs/cgroup" + grant["identity"]["cgroup"]
    machine.succeed("systemctl stop burst-cancel-client")
    wait_committed(0)
    machine.succeed(f"test ! -e {group} || grep -q '^populated 0$' {group}/cgroup.events")
    machine.wait_until_succeeds("runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 amc admission status --json | python3 -c " + shlex.quote(
        'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'), timeout=10)

with test_section("an aged ordinary request receives a native burst quiet window"):
    machine.sleep(10)
    machine.succeed("systemd-run --unit=quiet-base-client --uid=1000 "
                    "--setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
                    "amc admission exec --contract tool --max-ram-usage 64MiB --timeout 30 -- /bin/sh -c " + shlex.quote(
                        "touch /tmp/quiet-base-entered; sleep 120"))
    wait_entered("quiet-base")
    burst("quiet-first", 1000, "touch /tmp/quiet-first-entered; trap '' TERM; sleep 120 & wait")
    wait_entered("quiet-first")
    launch("quiet-bulk", 1001, "tool", "touch /tmp/quiet-bulk-entered; sleep 120")
    machine.wait_until_succeeds("amc admission host-status | python3 -c " + shlex.quote(
        'import sys,json; assert any(not r["granted"] and r["memory_bytes"] == 100663296 for r in json.load(sys.stdin)["reservations"])'), timeout=10)
    # 64 MiB ordinary + 96 MiB bulk fits only after the first burst drains.
    # A second 32 MiB burst would fit physically and within its allowance, but
    # must not backfill past the one-second aging threshold.
    machine.sleep(1.25)
    burst("quiet-late", 1001, "touch /tmp/quiet-late-entered; sleep 120")
    machine.wait_until_succeeds("amc admission host-status | python3 -c " + shlex.quote(
        'import sys,json; assert any(r.get("burst") and r["identity"]["uid"] == 1001 and not r["granted"] for r in json.load(sys.stdin)["reservations"])'), timeout=3)
    machine.fail("test -e /tmp/quiet-late-entered")
    wait_entered("quiet-bulk")
    machine.succeed("systemctl stop quiet-base-client quiet-bulk-client quiet-late-client")
    wait_committed(0)

machine.succeed("test $(awk '$1 == \"oom_kill\" {print $2}' /proc/vmstat) = 0")
