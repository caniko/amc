import json
import shlex

machine = globals()["machine"]
test_section = globals()["subtest"]
globals()["start_all"]()
machine.wait_for_unit("amc-host-admission.service")
for user, uid in [("alice", 1000), ("bob", 1001)]:
    machine.succeed(f"loginctl enable-linger {user}; systemctl start user@{uid}.service")
    machine.wait_until_succeeds(f"test -S /run/user/{uid}/amc/admission.sock")


def status():
    return json.loads(machine.succeed("amc admission host-status"))


def user(command, uid=1000):
    return f"runuser -u {'alice' if uid == 1000 else 'bob'} -- env XDG_RUNTIME_DIR=/run/user/{uid} " + command


def small(command, uid=1000, runtime=5):
    return user(f"amc admission exec --contract tool --protected-pool --max-ram-usage 32MiB --runtime-max-sec {runtime} --timeout 15 -- /bin/sh -c " + shlex.quote(command), uid)


def launch(name, command):
    machine.succeed(f"systemd-run --unit={name} --setenv=PATH=/run/current-system/sw/bin -- /bin/sh -c " + shlex.quote(command))


def wait_bytes(value):
    machine.wait_until_succeeds("amc admission host-status | python3 -c " + shlex.quote(
        f'import json,sys; assert json.load(sys.stdin)["committed_bytes"] == {value}'
    ))


backing = 128 * 1048576
with test_section("idle protected backing survives broker restart"):
    wait_bytes(backing)
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    wait_bytes(backing)

# Fill the entire remaining host budget AND the ordinary host/user queue.
with test_section("full ordinary budget and queues leave both protected users available"):
    launch("bulk", user("amc admission exec --contract tool --timeout 15 -- /bin/sh -c 'touch /tmp/bulk-entered; sleep 120'"))
    machine.wait_until_succeeds("test -e /tmp/bulk-entered", timeout=30)
    wait_bytes(256 * 1048576)
    # Occupied entries count against queue_limit=1. Alice is rejected locally;
    # Bob reaches the already-full host queue. Neither ordinary payload may run.
    for uid in [1000, 1001]:
        denied = machine.fail(user("amc admission exec --contract tool --timeout 15 -- /bin/sh -c 'touch /tmp/ordinary-must-not-run'", uid) + " 2>&1")
        assert "queue is full" in denied, denied
    machine.fail("test -e /tmp/ordinary-must-not-run")
    for number in range(4):
        machine.succeed(small(f"dd if=/dev/zero of=/tmp/quick-cache-{number} bs=1M count=4 status=none; exit 0"))
        wait_bytes(256 * 1048576)
    machine.succeed(small("printf bob", 1001))

# An occupied slot survives both brokers restarting without payload replay.
with test_section("occupied protected restart preserves identity and native deadline without replay"):
    launch("held", small("echo entered >> /tmp/quick-entered; sleep 120", runtime=15))
    machine.wait_until_succeeds("test -e /tmp/quick-entered", timeout=30)
    grant = next(r for r in status()["reservations"] if r["domain"] == "quick-1000")
    group = "/sys/fs/cgroup" + grant["identity"]["cgroup"].rsplit("/", 1)[0]
    machine.succeed(f'test "$(cat {group}/memory.max)" = 67108864; test "$(cat {group}/memory.swap.max)" = 0; test "$(cat {group}/pids.max)" = 32')
    machine.succeed(f'test "$(cat {group}/cpu.max)" = "50000 100000"')
    machine.succeed("systemctl restart amc-host-admission")
    machine.wait_until_succeeds("test -S /run/amc-host/admission.sock")
    machine.succeed(user("systemctl --user restart amc-admission.service"))
    after = json.loads(machine.succeed(user("amc admission status --json")))
    assert any(entry["phase"] == "running" and entry["identity"]["cgroup"] == grant["identity"]["cgroup"] for entry in after["entries"])
    assert any(entry["id"] == grant["id"] and entry["identity"] == grant["identity"] for entry in status()["reservations"])
    wait_bytes(256 * 1048576)
    machine.wait_until_succeeds("! systemctl is-active --quiet held", timeout=30)
    machine.succeed('test "$(wc -l < /tmp/quick-entered)" = 1')
    machine.succeed(small("touch /tmp/after-native-deadline"))

# Missing/widened aggregate enforcement cannot authorize another payload.
# Use a full page: a one-byte ceiling rounds back to zero in the kernel.
with test_section("widened protected aggregate limits deny payload entry"):
    machine.succeed(f'echo 4096 > {group}/memory.swap.max; test "$(cat {group}/memory.swap.max)" = 4096')
    launch("denied", small("touch /tmp/must-not-run"))
    machine.sleep(2)
    machine.fail("test -e /tmp/must-not-run")
    machine.succeed("systemctl stop denied")
    machine.succeed(f"echo 0 > {group}/memory.swap.max")
    machine.succeed(small("exit 0"))

# Cancellation and hard resource failures never replay or grow the request.
with test_section("protected cancellation and memcg OOM retain bounded post-failure progress"):
    launch("cancel", small("touch /tmp/cancel-entered; sleep 120"))
    machine.wait_until_succeeds("test -e /tmp/cancel-entered", timeout=30)
    machine.succeed("systemctl stop cancel")
    machine.succeed(small("exit 0"))
    before_oom = dict(line.split() for line in machine.succeed(f"cat {group}/memory.events").splitlines())
    machine.fail(small("python3 -c 'a=bytearray(128*1024*1024)'"))
    after_oom = dict(line.split() for line in machine.succeed(f"cat {group}/memory.events").splitlines())
    assert int(after_oom["oom_kill"]) > int(before_oom["oom_kill"])
    machine.succeed(small("exit 0"))
    machine.succeed("systemctl stop bulk")
    wait_bytes(backing)
print("protected-pool: full budget/queue, two users, repeated calls, idle/occupied restart, native bounds, cancellation and memory failure")
