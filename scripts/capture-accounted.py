"""Run passive amc telemetry in its own user unit, export accounting, then stop it.

Python 3.11+, systemd user manager, Linux cgroup v2. No target policy changes.
The outer launcher and the systemd manager are outside the accounting boundary.
On an ambiguous manager/export failure, retain the unit for manual recovery;
never resubmit, and abort if a changed invocation is observed.
"""
import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

METRICS = {
    "CPUUsageNSec": "ns", "MemoryPeak": "bytes", "IOReadBytes": "bytes",
    "IOWriteBytes": "bytes", "IOReadOperations": "operations", "IOWriteOperations": "operations",
}
PROPERTIES = (
    "Id", "ActiveState", "SubState", "InvocationID", "ControlGroup", "Result",
    "ExecMainCode", "ExecMainStatus", "ExecMainStartTimestampMonotonic",
    "ExecMainExitTimestampMonotonic", "MemoryAccounting", "IOAccounting",
) + tuple(METRICS)


def command(argv):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=20, check=True)
    if len(result.stdout) > 64 * 1024:
        raise RuntimeError("manager response exceeded 64 KiB")
    return result.stdout


def write_json(path, value):
    with open(path, "x", encoding="utf-8", opener=lambda name, flags: os.open(name, flags, 0o600)) as handle:
        json.dump(value, handle, indent=2, allow_nan=False)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def number(properties, key):
    text = properties.get(key, "")
    # systemd uses UINT64_MAX or '[not set]' for unavailable accounting.
    if not re.fullmatch(r"[0-9]{1,20}", text):
        return None
    value = int(text)
    return value if value < 2**64 - 1 else None


def accounting(properties):
    start = number(properties, "ExecMainStartTimestampMonotonic")
    end = number(properties, "ExecMainExitTimestampMonotonic")
    metrics = {}
    for key, unit in METRICS.items():
        value = number(properties, key)
        metrics[key] = {"value": value, "unknown": None if value is not None else "unavailable", "unit": unit}
    return {
        "invocationId": properties["InvocationID"] if "InvocationID" in properties else None,
        "startMonotonicUs": start or None, "exitMonotonicUs": end or None,
        "durationUs": end - start if start and end and end >= start else None,
        "metrics": metrics,
        "manager": {key: properties.get(key) for key in PROPERTIES if key not in METRICS},
    }


def capture(args):
    if (not re.fullmatch(r"[A-Za-z0-9_.:@-]+\.(service|scope)", args.unit)
            or args.unit.startswith("-")):
        raise ValueError("target must be a safe .service or .scope unit name")
    if not 1 <= args.seconds <= 86400 or not 1000 <= args.interval_ms <= 60000:
        raise ValueError("seconds must be 1..86400; interval-ms must be 1000..60000")
    source = args.amc.resolve(strict=True)
    if not source.is_file() or not os.access(source, os.X_OK):
        raise ValueError("--amc must identify an executable file")
    output = args.output.absolute()
    if any(parent.is_symlink() for parent in (output, *output.parents)):
        raise ValueError("output path must not contain symlinks")
    output.mkdir(mode=0o700)  # Exclusive: never reuse another session's directory.
    binary = output / "amc"
    shutil.copyfile(source, binary)
    binary.chmod(0o500)
    with binary.open("rb") as handle:
        digest = hashlib.file_digest(handle, "sha256").hexdigest()
        os.fsync(handle.fileno())
    observer = f"amc-observe-{uuid.uuid4().hex}.service"
    metadata = {
        "schemaVersion": 1, "observerUnit": observer, "observerManager": "user",
        "targetUnit": args.unit, "targetManager": "system" if args.system else "user",
        "seconds": args.seconds, "intervalMs": args.interval_ms,
        "binarySha256": digest, "startedUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "scope": "observer cgroup including children; excludes outer launcher and systemd manager",
    }
    write_json(output / "session.json", metadata)
    print(f"Observer: {observer}; session: {output}", flush=True)

    cancelled = []
    handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
    for sig in handlers:
        signal.signal(sig, lambda received, frame: cancelled.append(received))
    try:
        launch = [
            "systemd-run", "--user", "--quiet", f"--unit={observer}",
            "--property=Type=exec", "--property=RemainAfterExit=yes", "--property=Restart=no",
            "--property=CPUAccounting=yes", "--property=MemoryAccounting=yes", "--property=IOAccounting=yes",
            f"--property=RuntimeMaxSec={args.seconds + 60}s", "--property=TimeoutStopSec=10s",
            "--", str(binary), "watch", args.unit, "--production", "--seconds", str(args.seconds),
            "--interval-ms", str(args.interval_ms), "--output", str(output / "capture"),
        ]
        if args.system:
            launch.append("--system")  # Only the read-only target query uses the system manager.
        command(launch)
        deadline = time.monotonic() + args.seconds + 90
        invocation = None
        cancelled_sent = False
        group = None
        while True:
            text = command(["systemctl", "--user", "show", "--no-pager",
                            "--property=" + ",".join(PROPERTIES), "--", observer])
            properties = {}
            for line in text.splitlines():
                key, separator, value = line.partition("=")
                if separator and key in PROPERTIES:
                    if key in properties:
                        raise RuntimeError("duplicate manager property")
                    properties[key] = value
            current = properties.get("InvocationID", "")
            if properties.get("Id") != observer or not re.fullmatch(r"[0-9a-f]{32}", current):
                raise RuntimeError("observer identity unavailable; retaining unit for recovery")
            if invocation is not None and current != invocation:
                raise RuntimeError("observer invocation changed; refusing further control")
            invocation = current
            group = properties.get("ControlGroup") or group
            if properties.get("SubState") in ("exited", "dead", "failed"):
                break
            if cancelled and not cancelled_sent:
                command(["systemctl", "--user", "kill", "--kill-whom=main", "--signal=SIGINT", "--", observer])
                cancelled_sent = True
                deadline = min(deadline, time.monotonic() + 30)
            if time.monotonic() >= deadline:
                raise RuntimeError("observer completion not confirmed; retaining unit for recovery")
            time.sleep(0.5)

        record = accounting(properties)
        record.update(schemaVersion=1, observerUnit=observer, observedControlGroup=group,
                      scope=metadata["scope"], binarySha256=digest,
                      clockDomain="systemd-monotonic-us",
                      cancellationRequested=bool(cancelled),
                      exportedUtc=datetime.datetime.now(datetime.timezone.utc).isoformat())
        # Do not stop/collect the retained unit until this durable export succeeds.
        write_json(output / "accounting.json", record)
        command(["systemctl", "--user", "stop", "--", observer])
        write_json(output / "cleanup.json", {"observerStopped": True, "observerUnit": observer,
                                             "invocationId": invocation})
        if cancelled:
            return 128 + cancelled[0]
        succeeded = (properties.get("Result") == "success" and properties.get("ExecMainCode") == "1"
                     and properties.get("ExecMainStatus") == "0")
        return 0 if succeeded else 2
    finally:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("unit", help="existing target service or scope; never started or changed")
    parser.add_argument("--system", action="store_true", help="observe a system-manager target")
    parser.add_argument("--amc", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/amc")
    parser.add_argument("--output", type=Path, required=True, help="new private session directory on an executable filesystem")
    parser.add_argument("--seconds", type=int, default=1800)
    parser.add_argument("--interval-ms", type=int, default=1000)
    args = parser.parse_args()
    try:
        return capture(args)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"capture failed: {error}. Inspect session.json and the observer unit before recovery; do not resubmit blindly.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
