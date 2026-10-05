"""VM-only native denial probes; live grants must survive unavailable evidence."""

import json
import os
import pathlib
import socket
import subprocess
import time

health_path = pathlib.Path("/run/amc-supervision/health.json")
original = json.loads(health_path.read_text())
evidence_path = pathlib.Path("/var/lib/amc-fixture")
retained = []


def call(domain=None):
    request = {"op": "status", "version": 1}
    if domain:
        request.update(op="acquire_pool", wait_ms=60000, domain=domain)
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(2)
        connection.connect("/run/amc-host/admission.sock")
        connection.sendall(json.dumps(request).encode() + b"\n")
        with connection.makefile() as stream:
            reply = json.loads(stream.readline())
    assert not reply["error"], reply
    return reply


def restart_broker():
    subprocess.run(["systemctl", "restart", "amc-host-admission"], check=True)
    # Type=simple start completion precedes bind(). Only a successful status
    # exchange establishes readiness; an absent socket is not a denial sample.
    deadline = time.monotonic() + 10
    while True:
        try:
            call()
            return
        except (FileNotFoundError, ConnectionRefusedError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.1)


def write_health(case):
    health = dict(original, observed_boot_ms=int(time.clock_gettime(time.CLOCK_BOOTTIME) * 1000), inhibit=False, degraded=False)
    if case == "expired":
        health["observed_boot_ms"] -= 4000
    elif case == "future":
        health["observed_boot_ms"] += 60000
    elif case == "changed-boot":
        health["boot_id"] = "00000000-0000-0000-0000-000000000000"
    elif case in ("inhibit", "degraded"):
        health[case] = True
    pending = health_path.with_suffix(".probe")
    pending.unlink(missing_ok=True)
    pending.write_text("{" if case == "malformed" else " " * 16385 if case == "oversized" else json.dumps(health))
    pending.chmod(0o666 if case == "writable" else 0o644)
    if case == "foreign-owner":
        os.chown(pending, 65534, 65534)
    pending.replace(health_path)
    if case == "missing":
        health_path.unlink()
    elif case == "symlink":
        health_path.rename(pending)
        health_path.symlink_to(pending)


def probe(case, domain, expected):
    start = time.monotonic()
    deadline = start + (10 if expected else 2)
    samples = 0
    while True:
        write_health(case)
        reply = call(domain)
        ledger = call()
        granted = [entry for entry in ledger["reservations"] if entry["granted"]]
        for entry in retained:
            assert entry in granted, (case, entry, ledger)
        if not expected:
            assert granted == retained, (case, "unexpected asynchronous grant", ledger)
        assert ledger["committed_bytes"] >= sum(entry["memory_bytes"] for entry in retained), ledger
        samples += 1
        if expected and reply["granted"]:
            retained[:] = granted
            break
        assert reply["granted"] is False, (case, reply)
        if time.monotonic() >= deadline:
            assert not expected, (case, reply)
            break
        time.sleep(0.1)
    return {"case": case, "granted": reply["granted"], "samples": samples,
            "elapsedSeconds": time.monotonic() - start, "retained": ledger["reservations"],
            "committedBytes": ledger["committed_bytes"], "waiting": reply["waiting"]}


deadline = time.monotonic() + 10
while call()["reservations"]:
    assert time.monotonic() < deadline, "previous probe execution owner was not cleaned up"
    time.sleep(0.1)
results = [probe("valid", "retained", True)]
for case in ("missing", "malformed", "oversized", "writable", "foreign-owner", "symlink", "expired", "future", "changed-boot", "inhibit", "degraded"):
    results.append(probe(case, "heartbeat", False))
results.append(probe("valid", "heartbeat", True))
(evidence_path / "admission-failures.json").write_text(json.dumps(results))

# Only the broker's private mount namespace receives corrupted proc observations.
# Restarting it must keep both ceiling-backed grants while a new pool waits.
override = pathlib.Path("/run/systemd/system/amc-host-admission.service.d/observations.conf")
override.parent.mkdir(parents=True, exist_ok=True)
fixture = evidence_path / "unknown-memory"
observations = []
try:
    for name, target, contents in (("missing-meminfo", "/proc/meminfo", "SwapFree: 1048576 kB\n"),
                                   ("malformed-meminfo", "/proc/meminfo", "MemAvailable: unknown kB\nSwapFree: 1048576 kB\n"),
                                   ("missing-memory-psi", "/proc/pressure/memory", "some avg10=0.00\n"),
                                   ("malformed-memory-psi", "/proc/pressure/memory", "full avg10=NaN\n")):
        fixture.write_text(contents)
        override.write_text(f"[Service]\nBindReadOnlyPaths={fixture}:{target}\n")
        write_health("valid")
        subprocess.run(["systemctl", "daemon-reload"], check=True)
        restart_broker()
        result = probe("valid", "telemetry", False)
        result["case"] = name
        observations.append(result)
finally:
    override.unlink(missing_ok=True)
    subprocess.run(["systemctl", "daemon-reload"], check=True)
    restart_broker()
observations.append(probe("valid", "telemetry", True))
(evidence_path / "admission-observations.json").write_text(json.dumps(observations))
