"""Memory headroom observation. Observation only, never policy.

Reads host MemAvailable and the named `full` memory-pressure line,
then emits one contract-valid observation event to stdout as JSONL.
`--unit` labels what is observed; nothing is read from or written to
any cgroup. Delivery (e.g. journal) is the caller's pipe. An 8 GiB
default threshold is a comparison reference, never readiness.
Set PROC_MEMINFO / PROC_PRESSURE_MEMORY to override inputs (tests).
"""

import argparse
import json
import os
import socket
import time
import uuid


def read_text(path: str) -> str | None:
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return None


def parse_full_avg300(text: str) -> str:
    """avg300 from the named `full` pressure line.

    Only plain percentages (0-100) are accepted; NaN, infinities,
    negatives, out-of-range and non-decimal readings are unknown
    rather than misleading numbers.
    """
    for line in text.splitlines():
        name, _, rest = line.partition(" ")
        if name != "full":
            continue
        for token in rest.split():
            if not token.startswith("avg300="):
                continue
            try:
                value = float(token.split("=", 1)[1])
            except ValueError:
                return "unknown"
            if not 0 <= value <= 100:
                return "unknown"
            return token.split("=", 1)[1]
    return "unknown"


def parse_available_kb(text: str | None) -> int | None:
    """MemAvailable in bytes; None when missing or impossible.

    The kB unit is verified, not assumed: a missing or unexpected
    unit invalidates the reading instead of scaling it wrongly.
    """
    if text is None:
        return None
    for line in text.splitlines():
        parts = line.split()
        if len(parts) < 3 or parts[0].rstrip(":") != "MemAvailable":
            continue
        if parts[2] != "kB":
            return None
        try:
            kib = int(parts[1])
        except ValueError:
            return None
        return kib * 1024 if kib >= 0 else None
    return None


def assess(meminfo: str | None, pressure: str | None,
           threshold: int) -> dict:
    available = parse_available_kb(meminfo)
    stall = (parse_full_avg300(pressure)
             if pressure is not None else "unknown")
    if available is None:
        finding = "unknown"
    elif available >= threshold:
        finding = "above-threshold"
    else:
        finding = "below-threshold"
    return {"availableBytes": available, "fullAvg300": stall,
            "finding": finding}


def resolve(trigger: str, finding: str) -> tuple:
    """Keep the hook trigger even when readings are unavailable.

    The finding lives in observedBytes vs limitBytes; an unknown
    reading changes only the outcome, never the trigger.
    """
    if finding == "unknown":
        return trigger, "unknown"
    return trigger, "ok"


EVENTS = ("policy.applied", "memory.observed", "quiescence.entered",
          "quiescence.exited")
ENTRY_POINTS = ("gamemode-hook", "operator-cli", "scheduled",
                "worker")
CLASSES = ("service-floor", "games", "interactive-dev",
           "background-workers", "builds-transfers-maintenance")
REASONS = ("gamemode-start", "gamemode-end", "health-check",
           "memory-pressure", "timeout", "unknown")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--policy-rev", default=None)
    ap.add_argument("--unit", default=None)
    ap.add_argument("--threshold-bytes", type=int,
                    default=8 * 1024**3)
    ap.add_argument("--host", default=socket.gethostname() or "localhost")
    ap.add_argument("--event", default="memory.observed",
                    choices=EVENTS)
    ap.add_argument("--entry-point", default="operator-cli",
                    choices=ENTRY_POINTS)
    ap.add_argument("--workload-class", default="games",
                    choices=CLASSES)
    ap.add_argument("--action", default="memory-assessment")
    ap.add_argument("--reason", default="health-check",
                    choices=REASONS)
    ap.add_argument("--self-test", action="store_true",
                    help="run checks, emit nothing")
    args = ap.parse_args()
    if args.self_test:
        return 0 if self_test() else 1
    if not args.policy_rev or not args.unit:
        ap.error("--policy-rev/--unit required (unless --self-test)")
    if args.threshold_bytes <= 0:
        ap.error("--threshold-bytes must be positive")

    meminfo = read_text(os.environ.get("PROC_MEMINFO",
                                       "/proc/meminfo"))
    pressure = read_text(os.environ.get("PROC_PRESSURE_MEMORY",
                                        "/proc/pressure/memory"))
    obs = assess(meminfo, pressure, args.threshold_bytes)
    now_ms = int(time.time() * 1000)
    reason, outcome = resolve(args.reason, obs["finding"])
    event = {
        "event": args.event,
        "policyRev": args.policy_rev,
        "host": args.host,
        "unit": args.unit,
        "invocationId": str(uuid.uuid4()),
        "entryPoint": args.entry_point,
        "workloadClass": args.workload_class,
        "action": args.action,
        "reason": reason,
        "startedMs": now_ms,
        "endedMs": int(time.time() * 1000),
        "outcome": outcome,
        "limitBytes": args.threshold_bytes,
        "details": {
            "threshold": str(args.threshold_bytes),
            "window": "pressure-full-avg300=" + obs["fullAvg300"],
            "policy": "assess-only",
        },
    }
    if obs["availableBytes"] is not None:
        event["observedBytes"] = obs["availableBytes"]
    print(json.dumps(event))
    return 0


def self_test() -> bool:
    fails = []

    def check(name: str, cond: bool) -> None:
        print(("PASS " if cond else "FAIL ") + name)
        if not cond:
            fails.append(name)

    mem = "MemTotal: 1 kB\nMemAvailable: 8000 kB\n"
    both = "some avg10=1.00 avg300=2.00 total=3\nfull avg10=4.00"
    both += " avg300=5.00 total=6"
    check("full-line", parse_full_avg300(both) == "5.00")
    check("some-ignored",
          parse_full_avg300("some avg10=1.00 avg300=2.00\n")
          == "unknown")
    check("malformed",
          parse_full_avg300("garbage\nfull broken\n")
          == "unknown")
    check("nondecimal",
          parse_full_avg300("full avg300=soon total=0\n")
          == "unknown")
    check("negative-stall",
          parse_full_avg300("full avg300=-1.00 total=0\n")
          == "unknown")
    check("above",
          assess(mem, "full avg300=0.00 total=0\n", 1024)
          ["finding"] == "above-threshold")
    check("below",
          assess(mem, "full avg300=9.00 total=0\n", 10**12)
          ["finding"] == "below-threshold")
    check("missing-inputs",
          assess(None, None, 1)["finding"] == "unknown"
          and assess(None, None, 1)["fullAvg300"] == "unknown")
    check("negative-meminfo",
          assess("MemAvailable: -1 kB\n",
                 "full avg300=0.00 total=0\n", 1)
          ["finding"] == "unknown")
    check("missing-unit",
          assess("MemAvailable: 8000\n",
                 "full avg300=0.00 total=0\n", 1)
          ["finding"] == "unknown")
    check("wrong-unit",
          assess("MemAvailable: 8000 MB\n",
                 "full avg300=0.00 total=0\n", 1)
          ["finding"] == "unknown")
    check("nan-stall",
          assess(mem, "full avg300=nan total=0\n", 1)
          ["fullAvg300"] == "unknown")
    check("inf-stall",
          assess(mem, "full avg300=inf total=0\n", 1)
          ["fullAvg300"] == "unknown")
    check("over-range-stall",
          assess(mem, "full avg300=101.00 total=0\n", 1)
          ["fullAvg300"] == "unknown")
    check("trigger-preserved-on-unknown",
          resolve("gamemode-start", "unknown")
          == ("gamemode-start", "unknown"))
    check("trigger-preserved-on-ok",
          resolve("gamemode-end", "above-threshold")
          == ("gamemode-end", "ok"))
    return not fails


if __name__ == "__main__":
    raise SystemExit(main())
