#!/usr/bin/env python3
"""Explicit smoke test in a disposable, booted systemd/cgroup v2 guest.

Creates two bounded sleep services and a private runtime slice. The second
slice disables memory accounting to exercise genuinely unavailable readings.
Requires an installed CLI and a working user manager; preserves all captures.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import uuid


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=30).stdout


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--amc", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    amc = str(Path(args.amc).resolve(strict=True))
    root = args.output.resolve()
    root.mkdir(mode=0o700)
    require(Path("/sys/fs/cgroup/cgroup.controllers").is_file(), "cgroup v2 required")
    run("systemctl", "--user", "show", "--property=Version")
    tag = uuid.uuid4().hex
    units = [f"amc-diagnostic-{tag}-{kind}.service" for kind in ("known", "unknown")]
    slice_name = f"amcdiagnostic{tag}.slice"
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "systemd/user"
    runtime.mkdir(parents=True, exist_ok=True)
    slice_file = runtime / slice_name

    def inspect(unit, name):
        text = run(amc, "inspect", unit, "--json")
        (root / f"{name}.json").write_text(text)
        result = json.loads(text)
        require(result["properties"]["ActiveState"] == "active", f"{unit} is not active")
        return result

    def report(name):
        capture = root / name
        text = run(amc, "report", str(capture), "--json")
        (root / f"{name}-report.json").write_text(text)
        markdown = run(amc, "report", str(capture))
        (root / f"{name}-report.md").write_text(markdown)
        return json.loads(text), markdown

    def watch(unit, name, seconds=3):
        return [amc, "watch", unit, "--production", "--seconds", str(seconds),
                "--output", str(root / name)]

    try:
        with slice_file.open("x") as file:
            file.write("[Slice]\nDisableControllers=memory\n")
        run("systemctl", "--user", "daemon-reload")
        for unit, properties in zip(units, (
            ["MemoryAccounting=yes", "MemoryMax=64M", "MemorySwapMax=0"],
            [f"Slice={slice_name}"],
        )):
            run("systemd-run", "--user", "--quiet", f"--unit={unit}", "--service-type=exec",
                "--property=RuntimeMaxSec=120s", "--property=Restart=no",
                *(f"--property={value}" for value in properties), "--", "/usr/bin/sleep", "120")
        before = inspect(units[0], "known-before")
        leaf = before["kernel"][0]["files"]
        require(leaf["memory.max"]["value"] == 64 << 20, "effective memory cap mismatch")
        require(leaf["memory.swap.max"]["value"] == 0, "effective swap cap mismatch")
        run(*watch(units[0], "complete"))
        complete, _ = report("complete")
        require(complete["collection"]["complete"] is True, "normal capture did not complete")
        require(complete["collection"]["eventDeltaCoverage"]["lifetimeComplete"] is False,
                "passive observation claimed complete lifetime")
        coverage = complete["metrics"]["targetCoverage"]["memory.current"]
        require(coverage["knownPoints"] == coverage["totalPoints"] >= 2, "normal readings missing")

        child = subprocess.Popen(watch(units[0], "interrupted", 60),
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 20
            while not (root / "interrupted/ready").is_file():
                require(child.poll() is None, "observer exited before readiness")
                require(time.monotonic() < deadline, "observer readiness timeout")
                time.sleep(0.05)
            child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=15)
            (root / "interrupted-stdout").write_bytes(stdout)
            (root / "interrupted-stderr").write_bytes(stderr)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=5)
        interrupted, _ = report("interrupted")
        require(interrupted["collection"]["reason"] == "cancelled", "cancellation was not recorded")
        require(interrupted["collection"]["complete"] is False, "cancellation claimed completion")
        require(interrupted["integrity"]["countsReconciled"] is True, "cancelled prefix is inconsistent")
        after = inspect(units[0], "known-after")
        for key in ("InvocationID", "ControlGroup", "MemoryMax", "MemorySwapMax", "Slice"):
            require(before["properties"][key] == after["properties"][key], f"target {key} changed")

        unknown = inspect(units[1], "unknown-before")
        cell = unknown["kernel"][0]["files"]["memory.current"]
        require(cell["value"] is None and cell["unknown"], "missing memory controller was not detected")
        run(*watch(units[1], "unavailable"))
        unavailable, markdown = report("unavailable")
        require(unavailable["collection"]["complete"] is False, "missing baseline claimed completion")
        coverage = unavailable["metrics"]["targetCoverage"]["memory.current"]
        require(coverage["knownPoints"] == 0 and coverage["totalPoints"] > 0, "unknown became known")
        require("memory.current: unavailable" in markdown, "Markdown hides unavailable measurement")
        require("memory.current" not in unavailable["metrics"]["targetSampledMax"], "unknown became zero")
        provenance = {
            "osRelease": Path("/etc/os-release").read_text(),
            "kernel": run("uname", "-r").strip(),
            "systemd": run("systemctl", "--version").splitlines()[0],
            "python": run("python3", "--version").strip(),
            "amc": run(amc, "--version").strip(),
            "amcSha256": hashlib.sha256(Path(amc).read_bytes()).hexdigest(),
            "passed": ["inspect", "complete-capture", "cancelled-capture", "unavailable-measurements",
                       "json-report", "markdown-report", "passive-target-identity"],
        }
        (root / "result.json").write_text(json.dumps(provenance, indent=2) + "\n")
        print(json.dumps(provenance, indent=2))
    finally:
        for unit in [*units, slice_name]:
            subprocess.run(["systemctl", "--user", "stop", unit], timeout=15, check=False)
            subprocess.run(["systemctl", "--user", "reset-failed", unit], timeout=15, check=False)
        slice_file.unlink(missing_ok=True)
        run("systemctl", "--user", "daemon-reload")


if __name__ == "__main__":
    main()
