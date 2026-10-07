import json
import shlex

# Injected by NixOS's native test driver.
machine = globals()["machine"]
test_section = globals()["subtest"]
globals()["start_all"]()
evidence = {}
machine.wait_for_unit("amc-host-admission.service")
for user, uid in [("alice", 1000), ("bob", 1001)]:
    machine.succeed(
        f"loginctl enable-linger {user}; systemctl start user@{uid}.service"
    )
    machine.wait_until_succeeds(f"test -S /run/user/{uid}/amc/admission.sock")


def write_file(path, text):
    machine.succeed("printf '%s' " + shlex.quote(text) + " > " + shlex.quote(path))


def status():
    return json.loads(machine.succeed("amc admission host-status"))


def wait_committed(amount):
    machine.wait_until_succeeds(
        "amc admission host-status | python3 -c "
        + shlex.quote(
            f'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == {amount}'
        ),
        timeout=30,
    )


def stop_scope(grant):
    identity = grant["identity"]
    unit = identity["cgroup"].rsplit("/", 1)[1]
    uid = identity["uid"]
    # The submission client can exit and collect the scope between the broker
    # snapshot and this stop. A missing unit is acceptable only after positive
    # native cleanup; neither a failed stop nor unknown/replaced events suffice.
    machine.execute(f"systemctl --user --machine={uid}@.host stop {unit}")
    group = shlex.quote("/sys/fs/cgroup" + identity["cgroup"])
    machine.wait_until_succeeds(
        f'test ! -e {group} || (test "$(stat -c %i {group})" = {identity["inode"]} '
        f"&& grep -q '^populated 0$' {group}/cgroup.events)",
        timeout=30,
    )


def stop_prepared():
    for grant in status()["reservations"]:
        if grant["domain"].startswith("foreground-"):
            stop_scope(grant)


def launch(name, uid, contract, command):
    machine.succeed(
        f"systemd-run --unit={name}-client --uid={uid} "
        "--setenv=PATH=/run/current-system/sw/bin "
        f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
        f"amc admission exec --contract {contract} --timeout 60 -- /bin/sh -c "
        + shlex.quote(command)
    )


def wait_entered(name, host_socket=None, client_unit=None):
    try:
        machine.wait_until_succeeds(f"test -f /tmp/{name}-entered", timeout=30)
    except Exception as error:
        if host_socket is not None:
            queries = {
                "host": "amc admission host-status --socket "
                + shlex.quote(host_socket),
                "client": f"journalctl -b --no-pager -o cat -u {name}-client -n 12",
                "brokers": "journalctl -b --no-pager -o cat -u normal-host -u normal-private-1000 -u normal-private-1001 -n 12",
            }
            for uid in [1000, 1001]:
                queries[str(uid)] = (
                    f"runuser -u {'alice' if uid == 1000 else 'bob'} -- "
                    f"env XDG_RUNTIME_DIR=/run/user/{uid} amc admission status --json "
                    f"--socket /run/user/{uid}/amc/normal.sock"
                )
            evidence = {
                key: machine.execute(command) for key, command in queries.items()
            }
            # Keep the relevant endpoints and client in the final Nix log tail.
            raise AssertionError(
                json.dumps({"payload": name, "evidence": evidence})
            ) from error
        queries = {
            "host": "amc admission host-status",
            "client": "journalctl -b --no-pager -o cat -u "
            + shlex.quote(client_unit or f"{name}-client")
            + " -n 30",
            "user": "journalctl -b --no-pager -o cat _UID=1000 -n 30",
        }
        observations = {
            key: machine.execute(command) for key, command in queries.items()
        }
        # Keep the actual launch refusal in one final line. Pretty-printed host
        # status otherwise displaces the client journal from Nix's failure tail.
        raise AssertionError(
            json.dumps({"payload": name, "evidence": observations})
        ) from error


with test_section(
    "rapid native and admitted jobs preserve completion and fail closed on startup loss"
):
    user = "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 "
    for mode in ["", " --admission"]:
        receipt = json.loads(
            machine.succeed(
                user + "python3 /etc/amc-native-completion.py amc" + mode, timeout=180
            )
        )
        assert [r["exit"] for r in receipt["rapid_jobs"]] == [0, 42, 0, 42], receipt
        assert receipt["startup_loss"] == {
            "disconnect": "no-exec",
            "invalid-ack": "no-exec",
        }, receipt
    wait_committed(0)

with test_section("durable root pool tracks every potential execution owner"):
    # Owners are outside this empty bounded slice, just like Nix's handlers.
    write_file(
        "/tmp/pool-owner.py",
        """import json, socket, sys, time
while True:
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(2)
        s.connect("/run/amc-host/admission.sock")
        s.sendall(b'{"op":"acquire_pool","version":1,"wait_ms":60000,"domain":"builders","operation":1}\\n')
        r = json.loads(s.makefile().readline())
    assert not r["error"], r
    if r["granted"]:
        open(sys.argv[1], "w").write(r["ticket"])
        break
    time.sleep(.25)
time.sleep(120)
""",
    )
    for name in ["first", "second"]:
        machine.succeed(
            f"systemd-run --unit=pool-{name} -- python3 /tmp/pool-owner.py /tmp/pool-{name}"
        )
        machine.wait_until_succeeds(f"test -s /tmp/pool-{name}")
    wait_committed(96 * 1048576)
    assert len(status()["reservations"]) == 1
    assert len(status()["reservations"][0]["owners"]) == 2
    machine.succeed("systemd-run --unit=pool-child --slice=builders.slice -- sleep 120")
    machine.wait_until_succeeds(
        "grep -q '^populated 1$' /sys/fs/cgroup/builders.slice/cgroup.events"
    )
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
    write_file(
        "/tmp/omit-host.py",
        """import json, socket, subprocess, time
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
""",
    )
    machine.succeed(
        "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 python3 /tmp/omit-host.py"
    )
    machine.fail("test -f /tmp/bypassed")
    wait_committed(0)

with test_section("simultaneous users, restart persistence, and automatic lending"):
    launch(
        "alice",
        1000,
        "tool",
        "touch /tmp/alice-entered; while ! test -e /tmp/alice-finish; do sleep .1; done",
    )
    wait_entered("alice")
    wait_committed(96 * 1048576)
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    wait_committed(96 * 1048576)
    launch(
        "bob",
        1001,
        "tool",
        "touch /tmp/bob-entered; while ! test -e /tmp/bob-finish; do sleep .1; done",
    )
    machine.sleep(1)
    machine.fail("test -f /tmp/bob-entered")
    own = json.loads(machine.succeed("runuser -u bob -- amc admission host-status"))
    assert own["reservations"] and all(
        r["identity"]["uid"] == 1001 for r in own["reservations"]
    )
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

with test_section(
    "short native bursts exceed the normal budget and retain restart accounting"
):
    for name, uid, contract in [
        ("full-a", 1000, "tool"),
        ("full-b", 1001, "small"),
        ("full-c", 1000, "small"),
    ]:
        launch(name, uid, contract, f"touch /tmp/{name}-entered; sleep 120")
        wait_entered(name)
    wait_committed(160 * 1048576)

    def burst(name, uid, command, runtime=5):
        machine.succeed(
            f"systemd-run --unit={name}-client --uid={uid} "
            "--setenv=PATH=/run/current-system/sw/bin "
            f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
            f"amc exec --burst --max-ram-usage 32MiB --runtime-max-sec {runtime} --timeout 30 -- "
            "/bin/sh -c " + shlex.quote(command)
        )

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
    assert any(
        r["id"] == grant["id"] and r["granted"] for r in restored["reservations"]
    )
    burst("burst-b", 1001, "touch /tmp/burst-b-entered; trap '' TERM; sleep 120 & wait")
    wait_entered("burst-b")
    wait_committed(160 * 1048576)
    # RuntimeMaxSec and bounded final SIGKILL clean descendants without releasing
    # the still-running normal jobs. A new call cannot erase the durable cooldown.
    burst("burst-rate", 1001, "touch /tmp/burst-rate-entered", runtime=1)
    machine.wait_until_succeeds(
        "systemctl show burst-rate-client --property=ActiveState --value | grep -qE 'inactive|failed'",
        timeout=15,
    )
    machine.fail("test -f /tmp/burst-rate-entered")
    assert status()["burst_committed_bytes"] == 0
    machine.succeed("systemctl stop full-a-client full-b-client full-c-client")
    wait_committed(0)

with test_section("sized ordinary calls use the normal budget with bursts disabled"):
    # Isolated endpoints use the same native domains, with both policy layers
    # explicitly stripped of burst capacity. No jobs overlap the primary broker.
    machine.succeed(
        "python3 -c "
        + shlex.quote("""
from pathlib import Path
import json
host = json.loads(Path("/etc/amc-test-host-policy.json").read_text())
host.pop("burst")
host.pop("preparations")
host["domains"] = [d for d in host["domains"] if not d.get("burst")]
user = json.loads(Path("/etc/amc-test-user-policy.json").read_text())
user.pop("burst_budget_bytes")
user["contracts"].pop("tool-burst")
Path("/tmp/amc-normal-host.json").write_text(json.dumps(host))
Path("/tmp/amc-normal-user.json").write_text(json.dumps(user))
""")
    )
    machine.succeed(
        "systemd-run --unit=normal-host -- amc admission host-serve "
        "--policy /tmp/amc-normal-host.json --socket /run/amc-normal/admission.sock --state /var/lib/amc-normal"
    )
    machine.wait_until_succeeds("test -S /run/amc-normal/admission.sock")
    for user, uid in [("alice", 1000), ("bob", 1001)]:
        machine.succeed(
            f"systemd-run --unit=normal-private-{uid} --uid={uid} "
            f"--setenv=HOME=/home/{user} --setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
            "amc admission serve --policy /tmp/amc-normal-user.json "
            f"--socket /run/user/{uid}/amc/normal.sock --state /home/{user}/.local/state/amc-normal "
            "--host-socket /run/amc-normal/admission.sock"
        )
        machine.wait_until_succeeds(f"test -S /run/user/{uid}/amc/normal.sock")
    for index, uid in enumerate([1000, 1001, 1000, 1001, 1000, 1001]):
        machine.succeed(
            f"systemd-run --unit=sized-{index}-client --uid={uid} "
            "--setenv=PATH=/run/current-system/sw/bin "
            f"--setenv=XDG_RUNTIME_DIR=/run/user/{uid} -- "
            f"amc admission exec --socket /run/user/{uid}/amc/normal.sock --contract tool "
            "--max-ram-usage 32MiB --runtime-max-sec 120 --timeout 30 -- /bin/sh -c "
            + shlex.quote(f"touch /tmp/sized-{index}-entered; sleep 120")
        )
        if index < 5:
            wait_entered(f"sized-{index}", "/run/amc-normal/admission.sock")
    machine.sleep(1)
    machine.fail("test -e /tmp/sized-5-entered")
    normal = json.loads(
        machine.succeed(
            "amc admission host-status --socket /run/amc-normal/admission.sock"
        )
    )
    assert normal["committed_bytes"] == normal["budget_bytes"] == 160 * 1048576, normal
    assert normal["burst_budget_bytes"] == normal["burst_committed_bytes"] == 0, normal
    assert sum(r["granted"] for r in normal["reservations"]) == 5, normal
    assert all(
        r["memory_bytes"] == 32 * 1048576 and not r.get("burst")
        for r in normal["reservations"]
    ), normal
    for r in normal["reservations"]:
        machine.succeed(
            f"test $(cat /sys/fs/cgroup{r['identity']['cgroup']}/memory.max) = 33554432"
        )
    machine.succeed("systemctl stop sized-0-client")
    wait_entered("sized-5", "/run/amc-normal/admission.sock")
    machine.succeed(
        "systemctl stop sized-1-client sized-2-client sized-3-client sized-4-client sized-5-client"
    )
    machine.wait_until_succeeds(
        "amc admission host-status --socket /run/amc-normal/admission.sock | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'
        ),
        timeout=30,
    )
    machine.succeed(
        "systemctl stop normal-private-1000 normal-private-1001 normal-host"
    )

with test_section(
    "burst client loss and private restart retain grants until cleanup is observable"
):
    # Previous bursts deliberately persisted their cooldowns.
    machine.sleep(10)
    burst(
        "burst-loss",
        1000,
        "touch /tmp/burst-loss-entered; trap '' TERM; sleep 120 & wait",
    )
    wait_entered("burst-loss")
    grant = next(r for r in status()["reservations"] if r.get("burst"))
    group = "/sys/fs/cgroup" + grant["identity"]["cgroup"]
    machine.succeed(f"grep -q '^populated 1$' {group}/cgroup.events")
    machine.succeed("systemctl --user --machine=1000@.host restart amc-admission")
    private = "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000 amc admission status --json"
    restored = json.loads(machine.succeed(private))
    assert restored["committed_bytes"] == 32 * 1048576, restored
    assert any(
        e["phase"] == "running" and e["contract"]["burst"] for e in restored["entries"]
    ), restored
    machine.succeed("touch /tmp/amc-observation-unavailable")
    machine.succeed("systemctl kill --kill-whom=main --signal=KILL burst-loss-client")
    # Real systemd deadlines clean the entire cgroup while the private broker
    # lacks evidence. Neither client death, restart nor expiry may free its grant.
    wait_committed(0)
    machine.wait_until_succeeds(
        private
        + " | python3 -c "
        + shlex.quote(
            'import sys,json; s=json.load(sys.stdin); assert s["committed_bytes"] == 33554432 and s["unreconciled"]'
        ),
        timeout=10,
    )
    machine.succeed(
        f"test ! -e {group} || grep -q '^populated 0$' {group}/cgroup.events"
    )
    machine.succeed("rm /tmp/amc-observation-unavailable")
    machine.wait_until_succeeds(
        private
        + " | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'
        ),
        timeout=10,
    )

with test_section("explicit burst cancellation confirms descendant cleanup"):
    burst(
        "burst-cancel",
        1001,
        "touch /tmp/burst-cancel-entered; trap '' TERM; sleep 120 & wait",
    )
    wait_entered("burst-cancel")
    grant = next(r for r in status()["reservations"] if r.get("burst"))
    group = "/sys/fs/cgroup" + grant["identity"]["cgroup"]
    machine.succeed("systemctl stop burst-cancel-client")
    wait_committed(0)
    machine.succeed(
        f"test ! -e {group} || grep -q '^populated 0$' {group}/cgroup.events"
    )
    machine.wait_until_succeeds(
        "runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1001 amc admission status --json | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin)["committed_bytes"] == 0'
        ),
        timeout=10,
    )

with test_section("an aged ordinary request receives a native burst quiet window"):
    machine.sleep(10)
    machine.succeed(
        "systemd-run --unit=quiet-base-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin "
        "--setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "amc admission exec --contract tool --max-ram-usage 64MiB --timeout 30 -- /bin/sh -c "
        + shlex.quote("touch /tmp/quiet-base-entered; sleep 120")
    )
    wait_entered("quiet-base")
    burst(
        "quiet-first",
        1000,
        "touch /tmp/quiet-first-entered; trap '' TERM; sleep 120 & wait",
    )
    wait_entered("quiet-first")
    launch("quiet-bulk", 1001, "tool", "touch /tmp/quiet-bulk-entered; sleep 120")
    machine.wait_until_succeeds(
        "amc admission host-status | python3 -c "
        + shlex.quote(
            'import sys,json; assert any(not r["granted"] and r["memory_bytes"] == 100663296 for r in json.load(sys.stdin)["reservations"])'
        ),
        timeout=10,
    )
    # 64 MiB ordinary + 96 MiB bulk fits only after the first burst drains.
    # A second 32 MiB burst would fit physically and within its allowance, but
    # must not backfill past the one-second aging threshold.
    machine.sleep(1.25)
    burst("quiet-late", 1001, "touch /tmp/quiet-late-entered; sleep 120")
    machine.wait_until_succeeds(
        "amc admission host-status | python3 -c "
        + shlex.quote(
            'import sys,json; assert any(r.get("burst") and r["identity"]["uid"] == 1001 and not r["granted"] for r in json.load(sys.stdin)["reservations"])'
        ),
        timeout=3,
    )
    machine.fail("test -e /tmp/quiet-late-entered")
    wait_entered("quiet-bulk")
    machine.succeed(
        "systemctl stop quiet-base-client quiet-bulk-client quiet-late-client"
    )
    wait_committed(0)

with test_section(
    "advance game intent drains existing work and gates ordinary and burst entry"
):
    launch(
        "drain-old",
        1000,
        "small",
        "touch /tmp/drain-old-entered; while ! test -e /tmp/drain-old-finish; do sleep .1; done",
    )
    wait_entered("drain-old")
    machine.succeed(
        "systemd-run --unit=prepared-game-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "amc prepare --profile game -- /bin/sh -c "
        + shlex.quote("touch /tmp/prepared-game-entered; sleep 120")
    )
    machine.wait_until_succeeds(
        "amc admission host-status | python3 -c "
        + shlex.quote(
            'import sys,json; s=json.load(sys.stdin); assert s["preparation_barrier"] and s["preparations"][0]["phase"] == "draining"'
        )
    )
    launch("drain-new", 1001, "small", "touch /tmp/drain-new-entered; sleep 120")
    burst("drain-burst", 1001, "touch /tmp/drain-burst-entered; sleep 120")
    machine.sleep(1)
    machine.fail("test -e /tmp/prepared-game-entered")
    machine.fail("test -e /tmp/drain-new-entered")
    machine.fail("test -e /tmp/drain-burst-entered")
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    assert status()["preparation_barrier"]
    machine.succeed("touch /tmp/drain-old-finish")
    wait_entered("prepared-game")
    grant = next(
        r for r in status()["reservations"] if r["domain"] == "foreground-1000"
    )
    assert grant["granted"] and grant["memory_bytes"] == 64 * 1048576
    assert "/app.slice/app-amcforeground.slice/" in grant["identity"]["cgroup"]
    machine.succeed(
        "systemctl stop prepared-game-client drain-new-client drain-burst-client"
    )
    stop_prepared()
    wait_committed(0)

with test_section(
    "a launch from an already-running client owns a separate native game lifetime"
):
    # Keep the parent alive as a persistent Steam client would be. A last-command
    # shell exec instead migrates its sole process and leaves no parent lifetime.
    command = "amc prepare --profile game -- /bin/sh -c 'touch /tmp/warm-game-entered; sleep 120'; sleep 120"
    machine.succeed(
        "systemd-run --unit=warm-steam-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "amc prepare --profile game -- /bin/sh -c " + shlex.quote(command)
    )
    wait_entered("warm-game", client_unit="warm-steam-client")
    state = status()
    games = [r for r in state["reservations"] if r["domain"] == "foreground-1000"]
    assert len(games) == 2 and all(r["granted"] for r in games), state
    assert len({r["identity"]["inode"] for r in games}) == 2, games
    machine.succeed("systemctl stop warm-steam-client")
    stop_prepared()
    wait_committed(0)

with test_section("nested exec handoff reconciles an emptied parent before consume"):
    command = "exec amc prepare --profile game -- /bin/sh -c 'touch /tmp/exec-game-entered; sleep 120'"
    machine.succeed(
        "systemd-run --unit=exec-steam-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "amc prepare --profile game -- /bin/sh -c " + shlex.quote(command)
    )
    wait_entered("exec-game", client_unit="exec-steam-client")
    wait_committed(64 * 1048576)
    games = [r for r in status()["reservations"] if r["domain"] == "foreground-1000"]
    assert len(games) == 1 and games[0]["granted"], games
    machine.succeed("systemctl stop exec-steam-client")
    stop_prepared()
    wait_committed(0)

with test_section(
    "prepared scope preserves a game-only filesystem namespace and surviving descendants"
):
    # A read-only root bind needs an existing mountpoint. Only the marker is
    # namespace-private; a service-mode launch through the manager loses it.
    machine.succeed("mkdir /amc-game-only")
    write_file("/tmp/namespace-marker", "namespace-only\n")
    machine.fail("test -e /amc-game-only/marker")
    command = (
        'test "$(cat /amc-game-only/marker)" = namespace-only '
        "&& { sleep 120 & touch /tmp/namespace-game-entered; }"
    )
    machine.succeed(
        "systemd-run --unit=namespace-game-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "bwrap --ro-bind / / --tmpfs /amc-game-only "
        "--ro-bind /tmp/namespace-marker /amc-game-only/marker "
        "--bind /tmp /tmp -- amc prepare --profile game -- /bin/sh -c "
        + shlex.quote(command)
    )
    wait_entered("namespace-game")
    machine.fail("test -e /amc-game-only/marker")
    machine.wait_until_succeeds(
        "systemctl show namespace-game-client --property=ActiveState --value | grep -qE 'inactive|failed'"
    )
    # The leader and submission client have exited, but the native descendant
    # keeps the scope charged until separately observed cleanup.
    wait_committed(64 * 1048576)
    stop_prepared()
    wait_committed(0)
    machine.succeed("rmdir /amc-game-only")

with test_section(
    "bounded page return makes real swap progress without disabling swap"
):
    machine.succeed("mkswap /dev/vdb; swapon /dev/vdb")
    machine.succeed(
        "systemd-run --unit=page-target --property=MemoryMax=128M --property=MemorySwapMax=128M -- python3 /etc/page-return-target.py"
    )
    machine.wait_until_succeeds("test -e /tmp/page-target-ready")
    group = "/sys/fs/cgroup/system.slice/page-target.service"
    machine.wait_until_succeeds(f"test $(cat {group}/memory.swap.current) -ge 33554432")
    before = int(machine.succeed(f"cat {group}/memory.swap.current"))
    # The recovery helper's 128 MiB reservation cannot fit behind this live
    # 64 MiB operation in the 160 MiB broker budget. A wait is not success.
    machine.succeed(
        "systemd-run --unit=page-budget-client --uid=1000 "
        "--setenv=PATH=/run/current-system/sw/bin --setenv=XDG_RUNTIME_DIR=/run/user/1000 -- "
        "amc admission exec --contract tool --max-ram-usage 64MiB --timeout 30 -- /bin/sh -c "
        + shlex.quote("touch /tmp/page-budget-entered; sleep 120")
    )
    wait_entered("page-budget")
    machine.fail(
        "systemd-run --unit=page-return --property=MemoryMax=128M --property=MemorySwapMax=0 --wait -- amc recover-swap"
    )
    machine.succeed(
        "test $(systemctl show page-return --property=ExecMainStatus --value) = 75; grep -q '^/dev/vdb' /proc/swaps"
    )
    machine.succeed(
        "systemctl stop page-budget-client; systemctl reset-failed page-return"
    )
    wait_committed(0)

    # A cancelled campaign that has acquired, but has not read, a batch retains
    # its claim across broker restart until the helper's native group is empty.
    interrupted = """import json, os, socket, time
target = json.load(open('/tmp/page-target-range.json'))
stat = open('/proc/%s/stat' % target['pid']).read().rsplit(') ', 1)[1].split()
request = {'op':'acquire_page_return','version':1,'pid':target['pid'],'start_ticks':int(stat[19]),'address':target['address'],'bytes':2097152}
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.connect('/run/amc-host/admission.sock')
    connection.sendall((json.dumps(request)+'\\n').encode())
    response = json.loads(connection.makefile().readline())
assert response['granted'] and response['error'] is None, response
open('/tmp/page-return-held','w').close()
time.sleep(120)
"""
    write_file("/tmp/page-return-interrupt.py", interrupted)
    machine.succeed(
        "systemd-run --unit=page-return --property=MemoryMax=128M --property=MemorySwapMax=0 -- python3 /tmp/page-return-interrupt.py"
    )
    machine.wait_until_succeeds("test -e /tmp/page-return-held")
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    assert status()["recovery"]["return_bytes"] == 2097152
    machine.succeed("systemctl stop page-return")
    machine.wait_until_succeeds(
        "amc admission host-status | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin).get("recovery") is None'
        )
    )
    machine.succeed("grep -q '^/dev/vdb' /proc/swaps")

    machine.succeed(
        "systemd-run --unit=page-return --property=MemoryMax=128M --property=MemorySwapMax=0 --wait -- amc recover-swap"
    )
    after = int(machine.succeed(f"cat {group}/memory.swap.current"))
    assert before > after and after == 0, (before, after)
    evidence["pageReturn"] = {
        "beforeSwapBytes": before,
        "afterSwapBytes": after,
        "waitExitCode": 75,
        "interruptedBatchBytes": 2097152,
    }
    machine.succeed(
        "grep -q '^/dev/vdb' /proc/swaps; systemctl is-active page-target.service"
    )
    machine.succeed("touch /tmp/page-target-probe")
    machine.wait_until_succeeds("test -e /tmp/page-target-intact")
    machine.succeed("systemctl stop page-target.service")
    wait_committed(0)

with test_section(
    "explicit whole-device recovery restores swap on success and interrupted cleanup"
):
    device_policy = json.loads(machine.succeed("cat /etc/amc-test-host-policy.json"))
    device_policy["swap_recovery"]["targets"] = [
        {"name": "vm", "path": "/dev/vdb", "priority": 10}
    ]
    write_file("/tmp/page-device-policy.json", json.dumps(device_policy))
    machine.succeed(
        "systemd-run --unit=device-host -- amc admission host-serve --policy /tmp/page-device-policy.json "
        "--socket /run/amc-device/admission.sock --state /var/lib/amc-device"
    )
    machine.wait_until_succeeds("test -S /run/amc-device/admission.sock")
    machine.succeed(
        "rm /tmp/page-target-ready /tmp/page-target-probe /tmp/page-target-intact; "
        "systemd-run --unit=page-target --property=MemoryMax=128M --property=MemorySwapMax=128M -- python3 /etc/page-return-target.py"
    )
    machine.wait_until_succeeds("test -e /tmp/page-target-ready")
    machine.wait_until_succeeds(
        "test $(awk '$1 == \"/dev/vdb\" {print $4}' /proc/swaps) -ge 32768"
    )
    before_device = int(
        machine.succeed("awk '$1 == \"/dev/vdb\" {print $4}' /proc/swaps")
    )
    machine.succeed(
        "systemd-run --unit=page-return --property=MemoryMax=128M --property=MemorySwapMax=0 --wait -- "
        "amc recover-swap --whole-device --socket /run/amc-device/admission.sock"
    )
    after_device = int(
        machine.succeed("awk '$1 == \"/dev/vdb\" {print $4}' /proc/swaps")
    )
    assert 0 <= after_device < before_device, (before_device, after_device)
    machine.succeed("touch /tmp/page-target-probe")
    machine.wait_until_succeeds("test -e /tmp/page-target-intact")
    machine.succeed("systemctl stop page-target")
    machine.wait_until_succeeds(
        "amc admission host-status --socket /run/amc-device/admission.sock | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin).get("recovery") is None'
        )
    )

    interrupted = """import json, socket, subprocess, time
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.connect('/run/amc-device/admission.sock')
    connection.sendall(b'{"op":"acquire_recovery","version":1,"target":"vm"}\\n')
    response = json.loads(connection.makefile().readline())
assert response['granted'] and response['error'] is None, response
subprocess.run(['swapoff','/dev/vdb'],check=True)
open('/tmp/device-return-off','w').close()
time.sleep(120)
"""
    write_file("/tmp/device-return-interrupt.py", interrupted)
    machine.succeed(
        "rm /tmp/page-target-ready /tmp/page-target-probe /tmp/page-target-intact; "
        "systemd-run --unit=page-target --property=MemoryMax=128M --property=MemorySwapMax=128M -- python3 /etc/page-return-target.py"
    )
    machine.wait_until_succeeds("test -e /tmp/page-target-ready")
    machine.wait_until_succeeds(
        "test $(awk '$1 == \"/dev/vdb\" {print $4}' /proc/swaps) -ge 32768"
    )
    machine.succeed(
        "systemd-run --unit=page-return --property=MemoryMax=128M --property=MemorySwapMax=0 "
        "--property='ExecStopPost=amc recover-swap --restore --socket /run/amc-device/admission.sock' -- "
        "python3 /tmp/device-return-interrupt.py"
    )
    machine.wait_until_succeeds("test -e /tmp/device-return-off")
    machine.fail("grep -q '^/dev/vdb' /proc/swaps")
    machine.succeed("systemctl stop page-return")
    machine.succeed(
        "test $(awk '$1 == \"/dev/vdb\" {print $5}' /proc/swaps) = 10; touch /tmp/page-target-probe"
    )
    machine.wait_until_succeeds("test -e /tmp/page-target-intact")
    machine.wait_until_succeeds(
        "amc admission host-status --socket /run/amc-device/admission.sock | python3 -c "
        + shlex.quote(
            'import sys,json; assert json.load(sys.stdin).get("recovery") is None'
        )
    )
    evidence["deviceReturn"] = {
        "beforeUsedKiB": before_device,
        "afterUsedKiB": after_device,
        "restoredPriority": 10,
        "interruptedRestored": True,
    }
    machine.succeed("systemctl stop page-target device-host")
