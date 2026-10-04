"""Retain source, inputs and exact native admission qualification results."""

import hashlib
import json
import os
import shutil
import subprocess
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ALLOWED = {
    ".#packages.x86_64-linux.default",
    ".#checks.x86_64-linux.test",
    ".#checks.x86_64-linux.clippy",
    ".#nixosTests.x86_64-linux.shared-admission",
    ".#nixosTests.x86_64-linux.supervision",
}
NATIVE_CASES = {
    "durable root pool tracks every potential execution owner",
    "helper cannot omit host capacity before private entry",
    "simultaneous users, restart persistence, and automatic lending",
    "changed native enforcement inhibits a fitting smaller job",
    "cancelled pending work never executes after native cleanup",
}
SUPERVISION_CASES = {
    "identity-bound recovery waits for descendant cleanup and starts once",
    "chronological replay and public fail-closed heartbeat",
    "in-flight supervisor restart trips without replay or forgiven budget",
    "shadow observations have no intervention authority",
    "native recovery completes without OOM kills",
    "host admission denies inhibited and expired heartbeats",
    "cold failed backend recovers once with durable invocation accounting",
}


def verify_native_report(path, required_cases=NATIVE_CASES):
    cases = ET.parse(path).getroot().findall(".//testcase")
    if not required_cases.issubset({case.get("name") for case in cases}):
        raise RuntimeError("Native VM report is missing required execution cases")
    if any(case.find(tag) is not None for case in cases for tag in ("failure", "error", "skipped")):
        raise RuntimeError("Native VM report contains unsuccessful execution cases")


def verify_supervision_evidence(path):
    for name in ("trace.jsonl", "recovery.json", "status.json", "replay.json", "replay-input.json", "oom.json", "heartbeat.json", "cold-status.json"):
        if not (path / name).is_file():
            raise RuntimeError(f"Native supervision report is missing {name}")
    binding = json.loads((path / "replay-input.json").read_text())
    trace = (path / "trace.jsonl").read_bytes()
    replay = (path / "replay.json").read_bytes()
    if binding != {
        "schemaVersion": 1,
        "traceSha256": hashlib.sha256(trace).hexdigest(),
        "traceBytes": len(trace),
        "replaySha256": hashlib.sha256(replay).hexdigest(),
    }:
        raise RuntimeError("Supervision replay evidence is not bound to the exported trace and summary")
    heartbeat = json.loads((path / "heartbeat.json").read_text())
    if heartbeat != [
        {"inhibit": True, "age_ms": 0, "granted": False},
        {"inhibit": False, "age_ms": 4000, "granted": False},
        {"inhibit": False, "age_ms": 0, "granted": True},
    ]:
        raise RuntimeError("Host admission heartbeat evidence does not prove denial and recovery")
    cold = json.loads((path / "cold-status.json").read_text()).get("recovery", {})
    if cold.get("active", True) is not None or len(cold.get("attempts", [])) != 1 or cold["attempts"][0].get("domain") != "cold-backend":
        raise RuntimeError("Cold failed backend evidence does not prove one accounted recovery")
    oom = json.loads((path / "oom.json").read_text())
    if any(oom[phase][counter] != 0 for phase in ("initial", "final") for counter in ("oom", "oom_kill", "host_oom_kill")):
        raise RuntimeError("Native supervision report contains OOM events")


def retain():
    evidence = Path(os.environ["SIMIT_NIX_BUILD_RESULTS"])
    installable = (evidence / "installable").read_text().strip()
    if installable not in ALLOWED:
        raise RuntimeError("Unexpected qualification installable")
    result = json.loads((evidence / "result.json").read_text())
    if len(result) != 1 or set(result[0]["outputs"]) != {"out"}:
        raise RuntimeError("Expected exactly one selected output")
    output = Path(result[0]["outputs"]["out"])
    if not str(output).startswith("/nix/store/") or not output.exists():
        raise RuntimeError("Selected qualification output is not realized")
    revision = (evidence / "revision").read_text().strip()
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    if revision != actual:
        raise RuntimeError("Checkout changed during qualification")
    subprocess.run(["git", "diff", "--exit-code", "--", "flake.lock"], cwd=ROOT, check=True)
    shutil.copyfile(ROOT / "flake.lock", evidence / "flake.lock")
    if "nixosTests" in installable:
        required = SUPERVISION_CASES if installable.endswith(".supervision") else NATIVE_CASES
        verify_native_report(output / "junit.xml", required)
        shutil.copyfile(output / "junit.xml", evidence / "junit.xml")
        if installable.endswith(".supervision"):
            # The VM explicitly exports these mechanism receipts. Keeping just
            # a driver PASS would lose calibration and bounded recovery evidence.
            verify_supervision_evidence(output / "supervision" / "evidence")
            shutil.copytree(output / "supervision", evidence / "supervision")
    (evidence / "qualification.json").write_text(json.dumps({
        "schemaVersion": 1,
        "passed": True,
        "revision": revision,
        "installable": installable,
        "generatorRevision": "beea3e284a613d46468779bd998e51be2d63566c",
        "runId": os.environ.get("GITHUB_RUN_ID"),
        "runAttempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "eventSha": os.environ.get("GITHUB_SHA"),
        "derivation": result[0]["drvPath"],
        "outputs": result[0]["outputs"],
        "activated": False,
    }, indent=2) + "\n")
    print(f"Retained passing exact-source qualification for {installable}")


if __name__ == "__main__":
    retain()
