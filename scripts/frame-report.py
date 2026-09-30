#!/usr/bin/env python3
"""Bounded offline report for MangoHud 0.8.4 raw per-present CSV logs."""
import argparse
import csv
import hashlib
import io
import json
import math
import os
from pathlib import Path
import re
import stat


def report(path, *, start_seconds, end_seconds, stall_ms, log_interval_ms,
           max_bytes=128 * 1024 * 1024, max_frames=1_000_000):
    if log_interval_ms != 0:
        raise ValueError("raw-frame analysis requires a declared log_interval=0")
    if not all(math.isfinite(value) for value in (start_seconds, end_seconds, stall_ms)):
        raise ValueError("nonfinite window or threshold")
    if not (0 <= start_seconds < end_seconds <= 1800 and stall_ms > 0):
        raise ValueError("invalid window or threshold")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("CSV must be a regular file")
        raw = stream.read(max_bytes + 1)
    if len(raw) > max_bytes:
        raise ValueError("CSV exceeds size bound")
    try:
        rows = csv.reader(io.StringIO(raw.decode("utf-8-sig")), strict=True)
        for index, row in enumerate(rows):
            if index >= 64:
                raise ValueError("raw-frame header missing")
            if {"fps", "frametime", "elapsed"}.issubset(row):
                header = row
                break
        else:
            raise ValueError("raw-frame header missing")
        if len(header) != len(set(header)):
            raise ValueError("duplicate columns")
        frame_column = header.index("frametime")
        time_column = header.index("elapsed")
        previous = None
        first = None
        frames = []
        total = 0
        start_ns = round(start_seconds * 1_000_000_000)
        end_ns = round(end_seconds * 1_000_000_000)
        for row in rows:
            total += 1
            if total > max_frames or len(row) != len(header):
                raise ValueError("malformed row or frame-count bound exceeded")
            if re.fullmatch(r"[0-9]{1,20}", row[time_column]) is None:
                raise ValueError("elapsed must be integer nanoseconds")
            elapsed = int(row[time_column])
            frame = float(row[frame_column])
            if not math.isfinite(frame) or frame <= 0:
                raise ValueError("frametime must be positive finite milliseconds")
            if previous is not None and elapsed <= previous:
                raise ValueError("elapsed is not strictly increasing")
            if first is None:
                first = elapsed
            previous = elapsed
            if start_ns <= elapsed <= end_ns:
                frames.append(frame)
        if first is None or first > start_ns or previous < end_ns or not frames:
            raise ValueError("raw log does not cover the requested window")
    except (csv.Error, UnicodeError, OverflowError) as error:
        raise ValueError("malformed raw-frame CSV") from error
    frames.sort()
    percentile = lambda fraction: frames[math.ceil(len(frames) * fraction) - 1]
    stalls = sum(frame > stall_ms for frame in frames)
    return {
        "version": 1, "format": "MangoHud-0.8.4-raw",
        "sha256": hashlib.sha256(raw).hexdigest(), "rawRows": total,
        "declaredLogIntervalMs": log_interval_ms,
        "configurationEvidence": "operator-declared; runtime injection unverified",
        "coverage": "requested logger-relative window; game/lifetime alignment unverified",
        "startSeconds": start_seconds, "endSeconds": end_seconds,
        "windowSeconds": end_seconds - start_seconds, "frameCount": len(frames),
        "percentileMethod": "nearest-rank", "p95Ms": percentile(.95),
        "p99Ms": percentile(.99), "maxMs": frames[-1],
        "stallThresholdMs": stall_ms, "stallRule": "frametime > threshold",
        "stalls": stalls, "stallsPerSecond": stalls / (end_seconds - start_seconds),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", type=Path)
    parser.add_argument("--log-interval-ms", required=True, type=int)
    parser.add_argument("--start-seconds", required=True, type=float)
    parser.add_argument("--end-seconds", required=True, type=float)
    parser.add_argument("--stall-ms", required=True, type=float)
    args = parser.parse_args()
    try:
        result = report(args.csv, start_seconds=args.start_seconds, end_seconds=args.end_seconds,
                        stall_ms=args.stall_ms, log_interval_ms=args.log_interval_ms)
    except (OSError, ValueError):
        parser.exit(1, "Invalid or incomplete per-present log; retain the original CSV.\n")
    print(json.dumps(result, indent=2, allow_nan=False))


if __name__ == "__main__":
    main()
