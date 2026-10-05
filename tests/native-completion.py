"""Rapid native completion and startup-loss regression; no OOM workloads."""
import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path


def check(amc, admission=False):
    if admission:
        prefix = [amc, "admission", "exec", "--contract", "small", "--timeout", "30", "--runtime-max-sec", "5", "--"]
    else:
        prefix = [amc, "exec", "--slice", "agent-tools.slice", "--memory-max", "33554432",
                  "--memory-swap-max", "0", "--runtime-max-sec", "5", "--"]
    results = []
    # Keep these payloads short: adding sleeps would mask the missing-start race.
    for code in [0, 42, 0, 42]:
        run = subprocess.run(prefix + [sys.executable, "-c",
            ("import os,sys; print(sys.argv[1]); print(os.environ['AMC_COMPLETION_VALUE']); "
             "print(sys.stdin.read(), end=''); print('stderr', file=sys.stderr); sys.exit(int(sys.argv[2]))"),
            "$literal; --unit=foreign.service", str(code)],
            input="stdin\n", capture_output=True, text=True, timeout=40, check=False,
            env=dict(os.environ, AMC_COMPLETION_VALUE="environment"))
        # The first host acquisition can wait for the next broker tick. Its
        # existing one-time diagnostic precedes the payload's stderr; retain
        # that evidence separately while checking the payload bytes exactly.
        diagnostic = "waiting for host capacity: None\n"
        host_wait = admission and run.stderr.startswith(diagnostic)
        stderr = run.stderr.removeprefix(diagnostic) if host_wait else run.stderr
        assert (run.returncode, run.stdout, stderr) == (
            code, "$literal; --unit=foreign.service\nenvironment\nstdin\n", "stderr\n"), run
        results.append({"exit": code, "streams": "preserved", "host_wait_diagnostic": host_wait})
    return results


def startup_loss(amc):
    # The waiter must never exec when its runner disconnects or sends junk.
    parent = os.getpid()
    ticks = Path(f"/proc/{parent}/stat").read_text().rsplit(")", 1)[1].split()[19]
    with tempfile.TemporaryDirectory(prefix="amc-startup-loss-") as temporary:
        marker = str(Path(temporary) / "payload-executed")
        for reply in [b"", b"\x02"]:
            name = "amc-start-" + str(uuid.uuid4())
            with socket.socket(socket.AF_UNIX) as server:
                server.bind("\0" + name)
                server.listen(1)
                server.settimeout(5)
                waiter = subprocess.Popen([amc, "native-start", "--socket", name,
                    "--parent", str(parent), "--parent-start", ticks, "--",
                    sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()", marker],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                with server.accept()[0] as connection:
                    if reply:
                        connection.sendall(reply)
                output = waiter.communicate(timeout=5)
                assert waiter.returncode != 0, output
                assert not Path(marker).exists(), "startup failure executed the payload"
    return {"disconnect": "no-exec", "invalid-ack": "no-exec"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("amc")
    parser.add_argument("--admission", action="store_true")
    args = parser.parse_args()
    print(json.dumps({"rapid_jobs": check(args.amc, args.admission), "startup_loss": startup_loss(args.amc)}))
