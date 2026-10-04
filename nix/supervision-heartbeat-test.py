"""VM-only probe of the production host admission heartbeat integration."""

import json
import pathlib
import socket
import time

path = pathlib.Path("/run/amc-supervision/health.json")
health = json.loads(path.read_text())
results = []
for inhibit, age, expected in [(False, 0, True), (True, 0, False), (False, 4000, False)]:
    health.update(
        observed_boot_ms=int(time.clock_gettime(time.CLOCK_BOOTTIME) * 1000) - age,
        inhibit=inhibit,
        degraded=False,
    )
    path.write_text(json.dumps(health))
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect("/run/amc-host/admission.sock")
        connection.sendall(
            b'{"op":"acquire_pool","version":1,"wait_ms":1000,"domain":"heartbeat"}\n'
        )
        reply = json.loads(connection.makefile().readline())
    assert not reply["error"], reply
    assert reply["granted"] is expected, reply
    results.append({"inhibit": inhibit, "age_ms": age, "granted": reply["granted"]})
pathlib.Path("/var/lib/amc-fixture/heartbeat.json").write_text(json.dumps(results))
