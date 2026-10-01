"""Retain source, inputs and exact native admission qualification results."""

import json
import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
evidence = Path(os.environ["SIMIT_NIX_BUILD_RESULTS"])
installable = (evidence / "installable").read_text().strip()
allowed = {
    ".#packages.x86_64-linux.default",
    ".#checks.x86_64-linux.test",
    ".#checks.x86_64-linux.clippy",
    ".#nixosTests.x86_64-linux.shared-admission",
}
if installable not in allowed:
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
    if not (output / "log.html").is_file():
        raise RuntimeError("Passing native VM report is missing")
    shutil.copyfile(output / "log.html", evidence / "vm-log.html")
    for report in output.glob("*.xml"):
        shutil.copyfile(report, evidence / report.name)
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
