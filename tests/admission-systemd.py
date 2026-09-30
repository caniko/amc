#!/usr/bin/env python3
"""Explicit native mechanism test: <=64 MiB per job, existing bounded slice."""

import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--amc", required=True)
    parser.add_argument("--slice", required=True)
    args = parser.parse_args()
    amc = str(Path(args.amc).resolve())
    with tempfile.TemporaryDirectory(prefix="amc-admission-native-") as temporary:
        root = Path(temporary)
        endpoint = root / "run/admission.sock"
        state = root / "state"
        policy = root / "policy.json"
        policy.write_text(json.dumps({
            "version": 1, "budget_bytes": 64 << 20, "reserve_bytes": 1 << 30,
            "queue_limit": 8, "contracts": {"test": {
                "slice": args.slice, "memory_max": 64 << 20,
                "memory_swap_max": 0, "max_running": 1, "pause_file": None,
            }},
        }))

        def rpc(op, *, expect_error=False, **fields):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(10)
                client.connect(str(endpoint))
                client.sendall((json.dumps({"version": 1, "message": {"op": op, **fields}}) + "\n").encode())
                with client.makefile("rb") as stream:
                    response = json.loads(stream.readline(262145))
            assert (response["error"] is not None) == expect_error, response
            return response

        def wait(predicate):
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                try:
                    if predicate():
                        return
                except (OSError, ValueError):
                    pass
                time.sleep(0.05)
            raise AssertionError("native admission condition timed out")

        command = [amc, "admission", "exec", "--socket", str(endpoint), "--contract", "test", "--timeout", "10", "--"]
        server_command = [amc, "admission", "serve", "--policy", str(policy), "--socket", str(endpoint), "--state", str(state)]
        server = subprocess.Popen(server_command)
        clients = []
        try:
            wait(lambda: rpc("status"))
            # Stream/cwd/environment/argv preservation, including shell metacharacters.
            env = dict(os.environ, AMC_TEST_VALUE="$literal\nsecond line")
            result = subprocess.run(command + ["python3", "-c", "import os,sys,json; print(json.dumps([os.getcwd(),os.environ['AMC_TEST_VALUE'],sys.argv[1],sys.stdin.read()]))", "$argument"],
                                    cwd=root, env=env, input="input payload", text=True, capture_output=True, timeout=20)
            assert result.returncode == 0, (result.returncode, result.stderr)
            assert json.loads(result.stdout) == [str(root), env["AMC_TEST_VALUE"], "$argument", "input payload"]
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 0)

            release = root / "release"
            started = root / "started"
            script = "import pathlib,time,sys; pathlib.Path(sys.argv[1]).touch(); p=pathlib.Path(sys.argv[2]);\nwhile not p.exists(): time.sleep(.05)"
            first = subprocess.Popen(command + ["python3", "-c", script, str(started), str(release)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            clients.append(first)
            wait(started.exists)
            running = rpc("status")["status"]["entries"][0]
            # Public diagnostic IDs cannot authorize another process to stop
            # the submitting client's workload.
            rpc("cancel", id=running["id"], expect_error=True)
            assert rpc("status")["status"]["committed_bytes"] == 64 << 20
            second = subprocess.Popen(command + ["python3", "-c", "print('second')"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            clients.append(second)
            wait(lambda: len(rpc("status")["status"]["entries"]) == 2)
            assert rpc("status")["status"]["committed_bytes"] == 64 << 20
            server.kill()
            server.wait(timeout=5)
            server = subprocess.Popen(server_command + ["--systemctl", str(root / "unavailable-manager")])
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 64 << 20)
            wait(lambda: bool(rpc("status")["status"]["unreconciled"]))
            assert rpc("status")["status"]["committed_bytes"] == 64 << 20
            server.terminate()
            server.wait(timeout=10)
            server = subprocess.Popen(server_command)
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 64 << 20)
            release.touch()
            assert first.wait(timeout=15) == 0, first.communicate()
            second.wait(timeout=15)  # Unentered work is invalidated by restart.
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 0)

            # A dead submitting client does not own the running reservation.
            release.unlink()
            started.unlink()
            orphan = subprocess.Popen(command + ["python3", "-c", script, str(started), str(release)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            clients.append(orphan)
            wait(started.exists)
            orphan.kill()
            orphan.wait(timeout=5)
            assert rpc("status")["status"]["committed_bytes"] == 64 << 20
            release.touch()
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 0)

            # SIGTERM must stop the whole entered workload before capacity is
            # reusable, and retain the standard shell signal exit status.
            release.unlink()
            started.unlink()
            cancelled = subprocess.Popen(command + ["python3", "-c", script, str(started), str(release)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            clients.append(cancelled)
            wait(started.exists)
            cancelled.terminate()
            assert cancelled.wait(timeout=20) == 143
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 0)

            failed = subprocess.run(command + ["python3", "-c", "raise SystemExit(42)"], capture_output=True, timeout=20)
            assert failed.returncode == 42, (failed.returncode, failed.stderr)
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 0)

            ticket = rpc("enqueue", contract="test", wait_ms=5000)["entry"]["id"]
            wait(lambda: rpc("poll", id=ticket)["entry"]["phase"] == "reserved")
            foreign = subprocess.run([amc, "admission", "enter", "--socket", str(endpoint), "--ticket", ticket, "--", "echo", "MUST-NOT-EXECUTE"], capture_output=True, timeout=10)
            assert foreign.returncode != 0 and b"MUST-NOT-EXECUTE" not in foreign.stdout
            rpc("cancel", id=ticket)

            oom = subprocess.run(command + ["python3", "-c", "x=bytearray(256 << 20)"], capture_output=True, timeout=20)
            assert oom.returncode != 0
            wait(lambda: rpc("status")["status"]["committed_bytes"] == 0)
            after = subprocess.run(command + ["python3", "-c", "print('recovered')"], capture_output=True, timeout=20)
            assert after.returncode == 0 and after.stdout.strip() == b"recovered", after.stderr
            wait(lambda: not rpc("status")["status"]["entries"])
            print("PASS: native limits, streams, exit status, shared admission, SIGKILL recovery, manager unavailability, client death, cancellation ownership, entry ownership, and post-OOM progress")
        finally:
            release_path = root / "release"
            release_path.touch()
            for client in clients:
                if client.poll() is None:
                    client.send_signal(signal.SIGTERM)
                    client.wait(timeout=20)
            if server.poll() is None:
                server.terminate()
                server.wait(timeout=10)


if __name__ == "__main__":
    main()
