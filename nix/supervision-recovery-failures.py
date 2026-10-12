"""NixOS driver phases for native replacement and independent recovery budgets."""

import json
import time

# This file is executed in the surrounding NixOS test driver's namespace.
machine = globals()["machine"]
subtest = globals()["subtest"]
status = globals()["status"]
wait_for = globals()["wait_for"]
amc = globals()["amc"]


def write_json(path, value):
    machine.succeed("cat > " + path + " <<'EOF'\n" + json.dumps(value) + "\nEOF")


def unit(name):
    return "amc-failure@" + name + ".service"


def invocation(name):
    return machine.succeed("systemctl show " + unit(name) + " -p InvocationID --value").strip()


def starts(name):
    return int(machine.succeed("cat /var/lib/amc-fixture/" + name + "-starts"))


def fail_initial(name):
    machine.succeed("touch /var/lib/amc-fixture/" + name + "-fail")
    machine.execute("systemctl start " + unit(name))
    wait_for(lambda: machine.succeed("systemctl show " + unit(name) + " -p ActiveState --value").strip() == "failed", 10)
    machine.succeed("rm /var/lib/amc-fixture/" + name + "-fail")


def fail_running(name):
    child = machine.succeed("cat /var/lib/amc-fixture/" + name + "-child").strip()
    machine.succeed("systemctl kill --kill-whom=main --signal=KILL " + unit(name))
    machine.wait_until_succeeds("test ! -e /proc/" + child)
    wait_for(lambda: machine.succeed("systemctl show " + unit(name) + " -p ActiveState --value").strip() == "failed", 10)


def launch(name, domains, domain_limit, host_limit, cooldown=2000):
    p = status()["policy"]
    p.update(forecast_recovery=False, job_pools=[], cooldown_ms=cooldown,
             domain_recovery_limit=domain_limit, host_recovery_limit=host_limit)
    p["domains"] = [{"id": d, "uid": None, "unit": unit(d), "lifecycle": "restart",
                     "memory_max": 64 * 1024**2, "memory_swap_max": 0, "priority": 10} for d in domains]
    machine.succeed("mkdir -p /run/amc-" + name + " /var/lib/amc-" + name + "; chmod 700 /var/lib/amc-" + name)
    policy_file = "/var/lib/amc-fixture/" + name + ".json"
    write_json(policy_file, p)
    machine.succeed("systemd-run --unit=amc-" + name + " " + amc + " supervise serve --policy " + policy_file + " --runtime /run/amc-" + name + " --state /var/lib/amc-" + name)
    machine.wait_until_succeeds("test -f /run/amc-" + name + "/status.json")
    return lambda: json.loads(machine.succeed("cat /run/amc-" + name + "/status.json"))


def settled(read, name, count):
    wait_for(lambda: read()["recovery"]["active"] is None and len(read()["recovery"]["attempts"]) == count, 30)
    machine.wait_for_unit(unit(name))
    return read()


def phase(read):
    active = read()["recovery"]["active"]
    return active["phase"]["phase"] if active else None


machine.succeed("systemctl start " + unit("foreign"))
foreign = invocation("foreign")
machine.wait_until_succeeds("test -f /var/lib/amc-fixture/foreign-child")
foreign_child = machine.succeed("cat /var/lib/amc-fixture/foreign-child").strip()


def unchanged_foreign():
    assert invocation("foreign") == foreign
    assert starts("foreign") == 1
    machine.succeed("test -e /proc/" + foreign_child)


def blocked_receipt(read, supervisor, name):
    wait_for(lambda: phase(read) == "tripped", 15)
    before = read()
    failed = invocation(name)
    count = starts(name)
    time.sleep(8)
    assert invocation(name) == failed and starts(name) == count
    machine.succeed("systemctl restart amc-" + supervisor)
    time.sleep(5)
    after = read()
    assert after["inhibit"] and phase(read) == "tripped"
    assert after["recovery"]["attempts"] == before["recovery"]["attempts"]
    assert after["recovery"]["active"]["identity"] == before["recovery"]["active"]["identity"]
    assert invocation(name) == failed and starts(name) == count
    unchanged_foreign()
    return {"beforeRestart": before, "afterRestart": after, "failedInvocation": failed, "starts": count}


with subtest("replacement during native recovery trips without signalling the replacement"):
    fail_initial("replacement")
    original = invocation("replacement")
    read = launch("replacement", ["replacement"], 2, 4, cooldown=10000)
    wait_for(lambda: phase(read) == "cooling", 10)
    active = read()["recovery"]["active"]
    assert active["identity"]["invocation"] == original
    machine.succeed("systemctl start " + unit("replacement"))
    replacement = invocation("replacement")
    assert replacement != original
    wait_for(lambda: phase(read) == "tripped", 10)
    time.sleep(8)
    receipt = read()
    assert receipt["inhibit"] and len(receipt["recovery"]["attempts"]) == 1
    assert receipt["recovery"]["active"]["identity"] == active["identity"]
    assert invocation("replacement") == replacement and starts("replacement") == 2
    machine.succeed("test -e /proc/$(cat /var/lib/amc-fixture/replacement-child)")
    unchanged_foreign()
    write_json("/var/lib/amc-fixture/replacement-status.json", {"original": active, "replacementInvocation": replacement, "status": receipt})
    machine.succeed("systemctl stop amc-replacement " + unit("replacement"))

with subtest("repeated native recoveries exhaust the domain budget without forgiving attempts"):
    fail_initial("budget-a")
    read = launch("domain-budget", ["budget-a"], 2, 4)
    first = settled(read, "budget-a", 1)
    fail_running("budget-a")
    second = settled(read, "budget-a", 2)
    fail_running("budget-a")
    receipt = blocked_receipt(read, "domain-budget", "budget-a")
    assert len(receipt["afterRestart"]["recovery"]["attempts"]) == 2
    assert starts("budget-a") == 3
    receipt["completedRecoveries"] = [first, second]
    write_json("/var/lib/amc-fixture/domain-budget-status.json", receipt)
    machine.succeed("systemctl stop amc-domain-budget")

with subtest("different native domains exhaust the host budget without forgiving attempts"):
    machine.succeed("systemctl start " + unit("budget-a"))
    fail_initial("budget-b")
    read = launch("host-budget", ["budget-a", "budget-b"], 3, 2)
    first = settled(read, "budget-b", 1)
    fail_running("budget-a")
    second = settled(read, "budget-a", 2)
    fail_running("budget-b")
    receipt = blocked_receipt(read, "host-budget", "budget-b")
    attempts = receipt["afterRestart"]["recovery"]["attempts"]
    assert sorted(a["domain"] for a in attempts) == ["budget-a", "budget-b"]
    assert starts("budget-b") == 2
    receipt["completedRecoveries"] = [first, second]
    write_json("/var/lib/amc-fixture/host-budget-status.json", receipt)
    machine.succeed("systemctl stop amc-host-budget " + unit("budget-a") + " " + unit("budget-b"))

unchanged_foreign()
write_json("/var/lib/amc-fixture/foreign-status.json", {"invocation": foreign, "finalInvocation": invocation("foreign"), "starts": starts("foreign"), "childPid": int(foreign_child), "childAlive": True})
machine.succeed("systemctl stop " + unit("foreign"))
machine.succeed("test ! -e /proc/" + foreign_child)
