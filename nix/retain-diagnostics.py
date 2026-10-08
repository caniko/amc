"""Retain complete failed-builder logs without altering the Nix build result."""

import json
import os
import re
import resource
import subprocess
from pathlib import Path


def retain():
    evidence = Path(os.environ["SIMIT_NIX_BUILD_RESULTS"])
    failures = sorted(set(re.findall(
        r"Cannot build '(/nix/store/[a-z0-9]{32}-[^'\n]+\.drv)'",
        (evidence / "build.log").read_text(),
    )))
    logs = evidence / "builder-logs"
    logs.mkdir(exist_ok=True)
    results = []

    def bounded_log():
        limit = 64 * 1024 * 1024
        resource.setrlimit(resource.RLIMIT_FSIZE, (limit, limit))

    for derivation in failures[:16]:
        path = logs / (Path(derivation).name + ".log")
        with path.open("wb") as output:
            try:
                process = subprocess.run(
                    ["nix", "log", "--offline", derivation],
                    stdout=output, stderr=subprocess.PIPE, timeout=30,
                    preexec_fn=bounded_log,
                    check=False,
                )
                result = {"exitCode": process.returncode,
                          "diagnostic": process.stderr.decode(errors="replace")[:4096]}
            except subprocess.TimeoutExpired:
                result = {"timedOut": True}
        results.append({"derivation": derivation, "log": str(path.relative_to(evidence)),
                        "bytes": path.stat().st_size, **result})
    (evidence / "builder-diagnostics.json").write_text(json.dumps({
        "schemaVersion": 1, "revision": (evidence / "revision").read_text().strip(),
        "installable": (evidence / "installable").read_text().strip(),
        "failureCount": len(failures), "logs": results,
    }, indent=2) + "\n")


if __name__ == "__main__":
    retain()
