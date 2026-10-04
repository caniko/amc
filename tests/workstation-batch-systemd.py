#!/usr/bin/env python3
"""Small native B/C mechanism smoke; one 64 MiB grant, finite hash jobs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", required=True, type=Path)
    parser.add_argument("--slice", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    driver = str(args.driver.resolve())
    root = args.output.resolve()
    root.mkdir(mode=0o700)
    fifo = root / "invalid-manifest.fifo"
    os.mkfifo(fifo, mode=0o600)
    invalid = subprocess.run([driver, "check", str(fifo)], capture_output=True, timeout=10)
    assert invalid.returncode != 0
    properties = subprocess.check_output([
        "systemctl", "--user", "show", "--property=MemoryMax,MemorySwapMax", "--", args.slice,
    ], text=True)
    limits = dict(line.split("=", 1) for line in properties.splitlines())
    expected = hashlib.sha256(b"$literal\n" * 32768).hexdigest()
    work = "import hashlib,pathlib,sys,time; time.sleep(.5); pathlib.Path(sys.argv[1]).write_text(hashlib.sha256(sys.argv[2].encode()*32768).hexdigest())"
    verify = "import pathlib,sys; assert pathlib.Path(sys.argv[1]).read_text()==sys.argv[2]"
    for arm in ("b", "c", "bad-verifier"):
        jobs = []
        for index in range(3):
            artifact = root / f"{arm}-{index}.sha256"
            jobs.append({
                "id": f"hash-{index}", "weight_bytes": 64 << 20,
                "memory_max": 64 << 20, "memory_swap_max": 0,
                "cwd": str(root), "argv": [sys.executable, "-c", work, str(artifact), "$literal\n"],
                "verify_argv": [sys.executable, "-c", verify, str(artifact), expected if arm != "bad-verifier" else "wrong"],
                "useful_units": 1,
            })
        manifest = {
            "version": 1, "slice": args.slice,
            "aggregate_memory_max": int(limits["MemoryMax"]),
            "aggregate_memory_swap_max": int(limits["MemorySwapMax"]),
            "concurrency": 2, "budget_bytes": 64 << 20, "reserve_bytes": 0,
            "max_ram_fraction": 1.0, "runtime_seconds": 10,
            "admission_seconds": 20, "cpu_percent": 100, "jobs": jobs,
        }
        path = root / f"{arm}.json"
        path.write_text(json.dumps(manifest))
        subprocess.run([driver, "check", str(path)], check=True, timeout=10)
        output = root / arm
        result = subprocess.run([
            driver, "run", "--arm", "b" if arm == "bad-verifier" else arm,
            "--manifest", str(path), "--output", str(output),
        ], capture_output=True, text=True, timeout=90)
        (root / f"{arm}.log").write_text(result.stdout + result.stderr)
        summary = json.loads((output / "summary.json").read_text())
        assert len(summary["jobs"]) == 3
        if arm == "bad-verifier":
            assert result.returncode != 0 and not summary["allJobsSucceeded"]
            assert sum(job["usefulUnits"] for job in summary["jobs"]) == 0
            continue
        assert result.returncode == 0, result.stderr
        assert summary["allJobsSucceeded"]
        assert sum(job["usefulUnits"] for job in summary["jobs"]) == 3
        assert all(job["terminationConfirmed"] is True for job in summary["jobs"]), summary
        if arm == "c":
            assert summary["reservedBytes"] == 0
            receipts = sorted((job["receipt"] for job in summary["jobs"]), key=lambda r: r["startedUnixMs"])
            assert all(a["finishedUnixMs"] <= b["startedUnixMs"] for a, b in zip(receipts, receipts[1:])), receipts
    # Configuration mismatch is rejected before any useful workload starts.
    manifest["aggregate_memory_max"] += 1
    path = root / "mismatch.json"
    path.write_text(json.dumps(manifest))
    output = root / "mismatch"
    failed = subprocess.run([driver, "run", "--arm", "c", "--manifest", str(path), "--output", str(output)], capture_output=True, timeout=10)
    assert failed.returncode != 0 and not list(output.glob("job-*-entry.json"))
    print("PASS: B/C native enforcement, one shared byte budget, literal argv, verified useful output, failure retention, aggregate mismatch and cleanup")


if __name__ == "__main__":
    main()
