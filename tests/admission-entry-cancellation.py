"""Exercise private Enter with a root-owned fixture broker, never live services."""

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def reply(stream, value):
    stream.sendall((json.dumps(value) + "\n").encode())


def request(listener):
    stream, _ = listener.accept()
    stream.settimeout(5)
    with stream.makefile("rb") as reader:
        value = json.loads(reader.readline(262145))
    return stream, value


def main():
    assert os.geteuid() == 0, "fixture requires a root-owned broker socket"
    amc = str(Path(sys.argv[1]).resolve())
    results = {}
    for cancellation in [None, signal.SIGINT, signal.SIGTERM]:
        with tempfile.TemporaryDirectory(prefix="amc-entry-cancellation-") as temporary:
            root = Path(temporary)
            payload = root / "payload"
            private_path = root / "private.sock"
            host_path = root / "host.sock"
            with socket.socket(socket.AF_UNIX) as private, socket.socket(socket.AF_UNIX) as host:
                for listener, path in [(private, private_path), (host, host_path)]:
                    listener.bind(str(path))
                    listener.listen(1)
                    listener.settimeout(5)
                environment = {key: value for key, value in os.environ.items()
                               if key not in {"AMC_CONTINUATION", "XDG_RUNTIME_DIR"}}
                child = subprocess.Popen([
                    amc, "admission", "enter", "--socket", str(private_path),
                    "--ticket", "fixture-ticket", "--entry-key", "fixture-key",
                    "--host-socket", str(host_path), "--", sys.executable, "-c",
                    "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(payload),
                ], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    stream, value = request(private)
                    with stream:
                        assert value == {"version": 1, "message": {"op": "poll", "id": "fixture-ticket"}}, value
                        reply(stream, {"version": 1, "entry": {
                            "id": "fixture-ticket", "name": "fixture", "phase": "reserved",
                            "deadline_ms": time.time_ns() // 1000000 + 30000,
                            "identity": None, "client": None, "contract": {
                                "slice": "app.slice", "memory_max": 67108864,
                                "memory_swap_max": 0, "max_running": 1, "pause_file": None,
                            },
                        }})
                    stream, value = request(host)
                    with stream:
                        assert value["op"] == "acquire" and value["version"] == 1, value
                        assert 0 < value["wait_ms"] <= 30000, value
                        reply(stream, {"version": 1, "granted": True, "ticket": "host-ticket",
                                       "waiting": None, "committed_bytes": 67108864,
                                       "reservations": None, "error": None})
                    stream, value = request(private)
                    with stream:
                        assert value == {"version": 1, "message": {
                            "op": "enter", "id": "fixture-ticket", "key": "fixture-key",
                        }}, value
                        if cancellation is not None:
                            # Freeze only the fixture child at the private ack,
                            # queue the signal, then make both ready together.
                            # No sleep-based assumption about signal delivery.
                            child.send_signal(signal.SIGSTOP)
                            deadline = time.monotonic() + 5
                            while True:
                                pid, status = os.waitpid(child.pid, os.WNOHANG | os.WUNTRACED)
                                if pid:
                                    assert os.WIFSTOPPED(status), status
                                    break
                                assert time.monotonic() < deadline, "entry did not stop at private ack"
                                time.sleep(0.01)
                            child.send_signal(cancellation)
                        reply(stream, {"version": 1})
                        if cancellation is not None:
                            child.send_signal(signal.SIGCONT)
                    stdout, stderr = child.communicate(timeout=5)
                    expected = 0 if cancellation is None else 128 + cancellation
                    assert child.returncode == expected, (cancellation, child.returncode, stdout, stderr)
                    assert payload.exists() == (cancellation is None), (cancellation, "payload executed")
                    results["success" if cancellation is None else cancellation.name] = child.returncode
                except Exception:
                    if child.poll() is not None:
                        print(child.communicate()[1].decode(errors="replace"), file=sys.stderr)
                    raise
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait(timeout=5)
    print(json.dumps(results))


if __name__ == "__main__":
    main()
