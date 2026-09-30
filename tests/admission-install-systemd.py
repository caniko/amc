#!/usr/bin/env python3
"""Explicit, isolated install/restart/disable/removal check of the shipped units."""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--amc", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--examples", default=str(Path(__file__).resolve().parents[1] / "examples"))
    args = parser.parse_args()
    amc = str(Path(args.amc).resolve())
    root = Path(args.output).resolve()
    root.mkdir(mode=0o700)
    sources = Path(args.examples)
    prefix = "amcinstall" + uuid.uuid4().hex
    parent = prefix + ".slice"
    pool = prefix + "-background.slice"
    service = prefix + ".service"
    units = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    units.mkdir(parents=True, exist_ok=True)
    endpoint = root / "run/admission.sock"
    state = root / "state"
    policy_path = root / "policy.json"
    policy = json.loads((sources / "admission.json").read_text())
    policy.update(budget_bytes=64 << 20, reserve_bytes=128 << 20)
    policy["contracts"]["background"].update(slice=pool, memory_max=64 << 20, max_running=1)
    policy_path.write_text(json.dumps(policy, indent=2) + "\n")
    manager = shutil.which("systemctl")
    assert manager
    journal = (root / "commands.log").open("w")
    owned = []
    clients = []
    release = root / "release"

    def systemctl(*argv, check=True):
        result = subprocess.run([manager, "--user", *argv], capture_output=True, text=True, timeout=30, check=False)
        journal.write(json.dumps({"argv": argv, "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}) + "\n")
        journal.flush()
        if check:
            assert result.returncode == 0, result.stderr
        return result

    def status():
        result = subprocess.run([amc, "admission", "status", "--socket", str(endpoint), "--json"], capture_output=True, text=True, timeout=10, check=False)
        return json.loads(result.stdout) if result.returncode == 0 else None

    def wait(predicate):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(.1)
        raise AssertionError("installation condition timed out")

    try:
        for name, source in [(parent, "amc.slice"), (pool, "amc-background.slice"), (service, "amc-admission.service")]:
            text = (sources / "systemd" / source).read_text().replace("amc-background.slice", pool)
            if source.endswith(".slice"):
                text = text.replace("MemoryMax=2G", "MemoryMax=128M")
            else:
                command = [amc, "admission", "serve", "--policy", str(policy_path), "--socket", str(endpoint), "--state", str(state), "--systemctl", manager]
                text = text.replace("ExecStart=%h/.cargo/bin/amc admission serve --policy %h/.config/amc/admission.json",
                                    "ExecStart=" + " ".join(json.dumps(arg.replace("%", "%%")) for arg in command))
            path = units / name
            with path.open("x") as stream:
                stream.write(text)
            owned.append(path)
            (root / name).write_text(text)
        systemctl("daemon-reload")
        systemctl("enable", "--now", service)
        wait(lambda: status() is not None)
        systemctl("is-enabled", service)
        native = systemctl("show", pool, "--property=MemoryMax,MemorySwapMax,CPUWeight,IOWeight").stdout
        assert "MemoryMax=134217728" in native and "MemorySwapMax=0" in native, native
        assert "CPUWeight=20" in native and "IOWeight=20" in native, native

        started = root / "started"
        command = [amc, "admission", "exec", "--socket", str(endpoint), "--contract", "background", "--runtime-max-sec", "30", "--"]
        script = "import pathlib,sys,time; pathlib.Path(sys.argv[1]).touch(); p=pathlib.Path(sys.argv[2]);\nwhile not p.exists(): time.sleep(.05)"
        client = subprocess.Popen(command + [shutil.which("python3"), "-c", script, str(started), str(release)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        clients.append(client)
        wait(started.exists)
        before = status()
        assert before["committed_bytes"] == 64 << 20
        client.kill()
        client.wait(timeout=5)
        systemctl("restart", service)
        wait(lambda: (status() or {}).get("committed_bytes") == 64 << 20)
        after = status()
        assert after["entries"][0]["identity"] == before["entries"][0]["identity"]
        (root / "restart-status.json").write_text(json.dumps(after, indent=2) + "\n")
        release.touch()
        wait(lambda: (status() or {}).get("committed_bytes") == 0)
        useful = subprocess.run(command + [shutil.which("python3"), "-c", "import hashlib; print(hashlib.sha256(b'amc-install-work').hexdigest())"], capture_output=True, text=True, timeout=20, check=False)
        assert useful.returncode == 0
        assert useful.stdout.strip() == hashlib.sha256(b"amc-install-work").hexdigest()
        (root / "useful-output.txt").write_text(useful.stdout)
        wait(lambda: (status() or {}).get("entries") == [])
        systemctl("disable", "--now", service)
        refused = subprocess.run(command + ["true"], capture_output=True, timeout=10, check=False)
        assert refused.returncode != 0
        for path in owned:
            path.unlink()
        owned.clear()
        systemctl("stop", pool, parent)
        systemctl("daemon-reload")
        absent = systemctl("show", service, "--property=LoadState").stdout
        assert "LoadState=not-found" in absent, absent
        assert (state / "ledger.json").exists(), "removal erased durable state"
        (root / "result.json").write_text(json.dumps({"passed": True, "service": service, "pool": pool, "ledgerPreserved": True}) + "\n")
        print("PASS: standalone unit installation, effective limits/weights, restart with entered work, useful progress, disable refusal, removal and ledger preservation")
    finally:
        release.touch()
        for client in clients:
            if client.poll() is None:
                client.terminate()
                client.wait(timeout=20)
        systemctl("disable", "--now", service, check=False)
        systemctl("stop", pool, parent, check=False)
        for path in owned:
            path.unlink()
        systemctl("daemon-reload", check=False)
        journal.close()


if __name__ == "__main__":
    main()
