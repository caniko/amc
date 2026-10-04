"""Retain source, inputs and exact native admission qualification results."""

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
}
NATIVE_CASES = {
    "durable root pool tracks every potential execution owner",
    "helper cannot omit host capacity before private entry",
    "simultaneous users, restart persistence, and automatic lending",
    "changed native enforcement inhibits a fitting smaller job",
    "cancelled pending work never executes after native cleanup",
}


def verify_native_report(path):
    cases = ET.parse(path).getroot().findall(".//testcase")
    if not NATIVE_CASES.issubset({case.get("name") for case in cases}):
        raise RuntimeError("Native VM report is missing required execution cases")
    if any(case.find(tag) is not None for case in cases for tag in ("failure", "error", "skipped")):
        raise RuntimeError("Native VM report contains unsuccessful execution cases")


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
        verify_native_report(output / "junit.xml")
        shutil.copyfile(output / "junit.xml", evidence / "junit.xml")
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
