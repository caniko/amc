"""Finite VM-only observer; holds files while useful, never assumes retention."""
import argparse
import json
import os
from pathlib import Path
import time

FILES = ("cgroup.events", "memory.events", "memory.current", "memory.peak",
         "memory.swap.current", "memory.pressure")


def parse(name, text):
    if name.endswith("events"):
        values = {}
        for line in text.splitlines():
            key, value = line.split()
            if key in values or not key.replace("_", "").isalpha():
                raise ValueError("malformed")
            values[key] = int(value)
        if not values or any(value < 0 for value in values.values()):
            raise ValueError("malformed")
        return values
    if name == "memory.pressure":
        values = {}
        for line in text.splitlines():
            kind, *fields = line.split()
            if kind not in ("some", "full") or kind in values:
                raise ValueError("malformed")
            row = dict(field.split("=") for field in fields)
            if set(row) != {"avg10", "avg60", "avg300", "total"} or len(fields) != 4:
                raise ValueError("malformed")
            values[kind] = {key: int(value) if key == "total" else float(value)
                            for key, value in row.items()}
            if any(not 0 <= values[kind][key] <= 100 for key in ("avg10", "avg60", "avg300")):
                raise ValueError("malformed")
        if set(values) != {"some", "full"}:
            raise ValueError("malformed")
        return values
    value = int(text)
    if value < 0:
        raise ValueError("malformed")
    return value


def observe(cgroup, output, seconds):
    output.mkdir(mode=0o700)
    handles, last, baseline = {}, {}, None
    maximum_swap = None
    started = time.monotonic()
    reason = "deadline"
    try:
        for name in FILES:
            try:
                handles[name] = (cgroup / name).open()
            except OSError:
                pass
        with (output / "samples.jsonl").open("w") as samples:
            while time.monotonic() - started < seconds:
                snapshot = {"unixMs": time.time_ns() // 1_000_000, "files": {}}
                for name in FILES:
                    try:
                        if name not in handles:
                            raise OSError("unavailable")
                        handles[name].seek(0)
                        text = handles[name].read(4097)
                        if len(text) > 4096:
                            raise ValueError("oversized")
                        value = parse(name, text)
                        snapshot["files"][name] = {"value": value, "unknown": None}
                        last[name] = {"unixMs": snapshot["unixMs"], "value": value}
                    except (OSError, ValueError):
                        snapshot["files"][name] = {"value": None, "unknown": "unavailable, disappeared or malformed"}
                samples.write(json.dumps(snapshot) + "\n")
                samples.flush()
                files = snapshot["files"]
                if baseline is None:
                    if files["memory.events"]["value"] is None or files["cgroup.events"]["value"] is None:
                        reason = "observer-not-ready"
                        break
                    baseline = files["memory.events"]["value"]
                    (output / "ready").touch()
                swap = files["memory.swap.current"]["value"]
                if swap is not None:
                    maximum_swap = swap if maximum_swap is None else max(maximum_swap, swap)
                populated = files["cgroup.events"]["value"]
                if populated is None or populated.get("populated") == 0:
                    reason = "disappeared-or-empty"
                    break
                time.sleep(0.02)
    finally:
        for handle in handles.values():
            handle.close()
        final_events = last.get("memory.events", {}).get("value")
        deltas = None
        if baseline is not None and final_events is not None:
            deltas = {key: final_events[key] - value for key, value in baseline.items()
                      if key in final_events and final_events[key] >= value}
        result = {"reason": reason, "lastReadable": last, "eventDeltas": deltas,
                  "maxObservedSwap": maximum_swap}
        (output / "summary.json").write_text(json.dumps(result))
        (output / "done").touch()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("cgroup", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--seconds", type=int, default=40, choices=range(1, 61))
    args = parser.parse_args()
    os.umask(0o077)
    observe(args.cgroup, args.output, args.seconds)
