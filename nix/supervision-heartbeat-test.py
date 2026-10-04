"""VM-only probe of the production host admission heartbeat integration."""

import json
import pathlib
import socket
import time

path = pathlib.Path("/run/amc-supervision/health.json")
health = json.loads(path.read_text())
results = []
# Root-pool retries retain an existing grant while its execution owner lives.
# Exercise denied admission before granting that identity, and observe multiple
# broker ticks so an initially queued request cannot count as heartbeat proof.
for inhibit, age, expected in [(True, 0, False), (False, 4000, False), (False, 0, True)]:
    deadline = time.monotonic() + (10 if expected else 2)
    while True:
        health.update(
            observed_boot_ms=int(time.clock_gettime(time.CLOCK_BOOTTIME) * 1000) - age,
            inhibit=inhibit,
            degraded=False,
        )
        pending = path.with_suffix(".probe")
        pending.write_text(json.dumps(health))
        pending.chmod(0o644)
        pending.replace(path)
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(2)
            connection.connect("/run/amc-host/admission.sock")
            connection.sendall(
                b'{"op":"acquire_pool","version":1,"wait_ms":60000,"domain":"heartbeat"}\n'
            )
            with connection.makefile() as stream:
                reply = json.loads(stream.readline())
        evidence = {"inhibit": inhibit, "age_ms": age, "expected": expected, "reply": reply}
        assert not reply["error"], evidence
        if expected:
            if reply["granted"]:
                break
            assert time.monotonic() < deadline, evidence
        else:
            assert not reply["granted"], evidence
            if time.monotonic() >= deadline:
                break
        time.sleep(0.1)
    results.append({"inhibit": inhibit, "age_ms": age, "granted": reply["granted"]})
pathlib.Path("/var/lib/amc-fixture/heartbeat.json").write_text(json.dumps(results))
