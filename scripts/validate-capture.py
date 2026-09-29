"""Validate one `amc watch` capture directory without modifying it.

Reads manifest.json, samples.jsonl, summary.json and the ready/done markers,
reconciles counts/bytes/identity across the whole artifact set, and derives
report metrics from the samples instead of transcribing them by hand.
Stdlib only.

Validation contract (frozen by review):
- Mandatory envelope identifiers are required, not optional: manifest unit
  and observation ID; per-sample observation ID, clock domain, and unit;
  summary schema version, observation ID, target, reason, completion flag,
  coverage, and collection whenever a summary exists. An existing summary
  containing JSON null is malformed, not missing.
- A producer-recorded unknown is valid missing evidence. A malformed value
  claiming to be known is invalid, as is a cell reporting both at once.
- Completion requires baseline evidence in the samples themselves: readable
  memory.events and cgroup.events on one tick, matching the producer rule.
- Exit 0 means structurally valid (complete or partial); exit 2 means
  contradictory or malformed artifacts.

Bounds are enforced while streaming: lines are assembled in fixed-size
chunks, per-line/total/line-count caps abort the read, opened files are
verified as regular files via their descriptors without following symlinks,
and only consumed metric fields are retained. Errors are sanitized
(truncated, no raw input echoed). Behavior is identical under `python -O`.
"""
import json
import math
import os
import stat
import sys
from pathlib import Path

MAX_META_BYTES = 1 << 20
MAX_SAMPLES_BYTES = 320 << 20
MAX_LINES = 100_000
MAX_LINE_BYTES = 16 << 20
MAX_INT = 2**63
CHUNK_BYTES = 64 << 10
ERROR_SNIPPET = 160
CLOCK_DOMAIN = "monotonic-clock"
SCHEMA_VERSION = 1
COMPLETE_REASONS = ("deadline", "disappeared-or-empty")
CONSUMED_HOST_KEYS = frozenset({
    "meminfo.MemAvailable", "meminfo.SwapFree",
    "vmstat.pswpin", "vmstat.pswpout", "vmstat.pgmajfault",
    "pressure.memory", "pressure.cpu", "pressure.io",
})


class InvalidArtifact(Exception):
    pass


def _short(error):
    text = str(error).replace("\n", " ")
    return text[:ERROR_SNIPPET]


def open_regular(path, what):
    try:
        st = os.lstat(path)
    except OSError as error:
        raise InvalidArtifact(f"unreadable {what}: {_short(error)}") from error
    if not stat.S_ISREG(st.st_mode):
        raise InvalidArtifact(f"{what} is not a regular file")
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as error:
        raise InvalidArtifact(f"unreadable {what}: {_short(error)}") from error
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise InvalidArtifact(f"{what} is not a regular file")
    except OSError as error:
        os.close(fd)
        raise InvalidArtifact(f"unreadable {what}: {_short(error)}") from error
    except InvalidArtifact:
        os.close(fd)
        raise
    try:
        return os.fdopen(fd, "rb")
    except OSError as error:
        os.close(fd)
        raise InvalidArtifact(f"unreadable {what}: {_short(error)}") from error


def read_bounded(path, what):
    with open_regular(path, what) as handle:
        try:
            data = handle.read(MAX_META_BYTES + 1)
        except OSError as error:
            raise InvalidArtifact(f"{what} read failed: {_short(error)}") from error
    if len(data) > MAX_META_BYTES:
        raise InvalidArtifact(f"{what} exceeds {MAX_META_BYTES} bytes")
    return data


def _reject_constant(value):
    raise InvalidArtifact("non-finite number literal")


def _checked_float(text):
    try:
        value = float(text)
    except ValueError as error:
        raise InvalidArtifact("malformed number literal") from error
    if not math.isfinite(value):
        raise InvalidArtifact("non-finite number literal")
    return value


def _no_duplicates(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise InvalidArtifact("duplicate object key")
        obj[key] = value
    return obj


def parse_json(data, what):
    if isinstance(data, bytes):
        try:
            data = data.decode("utf-8")
        except UnicodeDecodeError as error:
            raise InvalidArtifact(f"{what} is not valid UTF-8: {_short(error)}") from error
    try:
        return json.loads(data, object_pairs_hook=_no_duplicates,
                          parse_constant=_reject_constant, parse_float=_checked_float)
    except ValueError as error:
        raise InvalidArtifact(f"{what} is not valid JSON: {_short(error)}") from error


def req_int(value, what, limit=MAX_INT):
    if isinstance(value, bool) or not isinstance(value, int):
        raise InvalidArtifact(f"{what} is not an integer")
    if not 0 <= value < limit:
        raise InvalidArtifact(f"{what} out of range")
    return value


def req_dict(value, what):
    if not isinstance(value, dict):
        raise InvalidArtifact(f"{what} is not an object")
    return value


def opt_str(value, what):
    if value is None:
        return None
    if not isinstance(value, str):
        raise InvalidArtifact(f"{what} is not a string")
    return value


def req_id(value, what):
    if not isinstance(value, str) or not value:
        raise InvalidArtifact(f"{what} missing")
    return value


def req_version(value, what):
    if type(value) is not int or value != SCHEMA_VERSION:
        raise InvalidArtifact(f"{what} unsupported schema version")
    return value


def check_cell(cell, where):
    """Shared coherence check (`Measurement::is_coherent`): returns True for
    known values, False for legitimate unknowns. An explicit null counts as
    unset, so both-set and neither-set cells are malformed."""
    if not isinstance(cell, dict):
        raise InvalidArtifact(f"{where} malformed cell")
    unknown = cell.get("unknown")
    if unknown is not None and not (isinstance(unknown, str) and unknown):
        raise InvalidArtifact(f"{where} malformed unknown reason")
    has_value = "value" in cell and cell["value"] is not None
    if unknown is not None and has_value:
        raise InvalidArtifact(f"{where} contradictory measurement cell")
    if unknown is None and not has_value:
        raise InvalidArtifact(f"{where} empty measurement cell")
    return has_value


def known_strict(files, key, where):
    """Known value under the producer coherence invariant
    (`Measurement::is_coherent`): exactly one of value/unknown is set, where
    an explicit null counts as unset. The Rust producer serializes unknowns
    as `{"value": null, "unknown": "reason"}`. A present non-object cell
    (including an explicit JSON null) is malformed, not an absent field."""
    if key not in files:
        return None
    cell = files[key]
    check_cell(cell, f"{where} {key}")
    return cell.get("value") if cell.get("value") is not None else None


SCALAR_HOST_KEYS = frozenset({
    "meminfo.MemAvailable", "meminfo.SwapFree",
    "vmstat.pswpin", "vmstat.pswpout", "vmstat.pgmajfault",
})
PRESSURE_HOST_KEYS = frozenset({
    "pressure.memory", "pressure.cpu", "pressure.io",
})


def psi_average(value, where):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise InvalidArtifact(f"{where} malformed known counter")
    if not 0 <= value <= 100:
        raise InvalidArtifact(f"{where} malformed known counter")
    return float(value)


def read_pressure_rows(value, where, key):
    """Validate a known pressure value against the producer parser shape
    (`parse_pressure`): `some` is required everywhere, `full` is required
    except for CPU, rows carry exactly total/avg10/avg60/avg300, and an
    absent optional row stays absent (never zero)."""
    if not isinstance(value, dict):
        raise InvalidArtifact(f"{where} host {key} malformed known counter")
    if set(value) - {"some", "full"}:
        raise InvalidArtifact(f"{where} host {key} unexpected pressure row")
    resource = key.split(".")[-1]
    rows = {}
    for row in ("some", "full"):
        if row not in value:
            if row == "some" or resource in ("memory", "io"):
                raise InvalidArtifact(f"{where} host {key} missing required {row} row")
            continue
        cell = value[row]
        if not isinstance(cell, dict) or set(cell) != {"total", "avg10", "avg60", "avg300"}:
            raise InvalidArtifact(f"{where} host {key}.{row} malformed known counter")
        rows[row] = counter_value(cell.get("total"), f"{where} host {key}.{row}")
        for avg in ("avg10", "avg60", "avg300"):
            psi_average(cell.get(avg), f"{where} host {key}.{row}.{avg}")
    return rows


def event_map(value, where, key, non_empty):
    """Validate a known event map against the producer parser shape:
    lowercase/underscore keys with u64 counters (`parse_events`)."""
    if not isinstance(value, dict):
        raise InvalidArtifact(f"{where} {key} malformed event map")
    if non_empty and not value:
        raise InvalidArtifact(f"{where} {key} empty event map")
    for name, entry in value.items():
        if not name or not all(c.isascii() and (c.islower() or c == "_") for c in name):
            raise InvalidArtifact(f"{where} {key} malformed event name")
        counter_value(entry, f"{where} {key}.{name}")
    return value


def stream_sample_lines(path, total_bytes):
    """Yield (line_number, content_bytes) with caps enforced during assembly."""
    with open_regular(path, "samples.jsonl") as handle:
        buf = bytearray()
        n = streamed = 0
        while True:
            try:
                chunk = handle.read(CHUNK_BYTES)
            except OSError as error:
                raise InvalidArtifact(f"samples.jsonl read failed: {_short(error)}") from error
            if not chunk:
                break
            streamed += len(chunk)
            if streamed > total_bytes:
                raise InvalidArtifact("samples.jsonl changed during read")
            buf += chunk
            while True:
                idx = buf.find(b"\n")
                if idx < 0:
                    break
                if idx + 1 > MAX_LINE_BYTES:
                    raise InvalidArtifact(f"samples.jsonl line {n + 1} exceeds limit")
                n += 1
                if n > MAX_LINES:
                    raise InvalidArtifact(f"samples.jsonl exceeds {MAX_LINES} lines")
                yield n, bytes(buf[:idx])
                del buf[:idx + 1]
            if len(buf) > MAX_LINE_BYTES:
                raise InvalidArtifact("samples.jsonl line under assembly exceeds limit")
        if buf:
            raise InvalidArtifact("samples.jsonl final line is truncated")
        if streamed != total_bytes:
            raise InvalidArtifact("samples.jsonl changed during read")


def windowed_rate(points, total_points, label, report):
    """Rate over the known-value window. Any gap, reset, or missing endpoint
    suppresses the calculation instead of bridging it."""
    if len(points) != total_points:
        report["suppressed"].append(f"{label}:partial-coverage")
        return None, None, None
    if len(points) < 2:
        report["suppressed"].append(f"{label}:insufficient-points")
        return None, None, None
    for (prev_mono, prev_value), (mono, value) in zip(points, points[1:]):
        if value < prev_value:
            report["suppressed"].append(f"{label}:counter-decreased")
            return None, None, None
        if mono < prev_mono:  # unreachable; the reader enforces order
            raise InvalidArtifact(f"{label} window out of order")
    window_ms = points[-1][0] - points[0][0]
    if window_ms <= 0:
        report["suppressed"].append(f"{label}:no-elapsed-time")
        return None, None, None
    delta = points[-1][1] - points[0][1]
    return delta, delta / (window_ms / 1000), window_ms


def counter_value(value, what):
    if isinstance(value, bool) or not isinstance(value, int):
        raise InvalidArtifact(f"{what} is not an integer counter")
    if not 0 <= value < 2**64:
        raise InvalidArtifact(f"{what} out of range")
    return value


def opt_unix_ms(value, what):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise InvalidArtifact(f"{what} is not an integer")
    if not 0 <= value < 2**128:
        raise InvalidArtifact(f"{what} out of range")
    return value


def marker_is_file(directory, name):
    try:
        return stat.S_ISREG(os.lstat(directory / name).st_mode)
    except OSError:
        return False


def validate(directory):
    directory = Path(directory)
    report: dict = {"directory": str(directory), "checks": [], "suppressed": []}

    manifest = parse_json(read_bounded(directory / "manifest.json", "manifest.json"), "manifest.json")
    req_dict(manifest, "manifest.json")
    req_version(manifest.get("schemaVersion"), "manifest schemaVersion")
    manifest_target: dict = req_dict(manifest.get("target"), "manifest target")
    manifest_unit = req_id(manifest_target.get("unit"), "manifest target unit")
    manifest_cgroup = opt_str(manifest_target.get("cgroupPath"), "manifest target cgroupPath")
    manifest_invocation = opt_str(manifest_target.get("invocationId"), "manifest target invocationId")
    manifest_obs_id = req_id(manifest.get("observationId"), "manifest observationId")
    manifest_boot = opt_str(manifest.get("bootId"), "manifest bootId")

    try:
        st = os.lstat(directory / "samples.jsonl")
    except OSError as error:
        raise InvalidArtifact(f"unreadable samples.jsonl: {_short(error)}") from error
    if not stat.S_ISREG(st.st_mode):
        raise InvalidArtifact("samples.jsonl is not a regular file")
    total_bytes = st.st_size
    if total_bytes > MAX_SAMPLES_BYTES:
        raise InvalidArtifact(f"samples.jsonl exceeds {MAX_SAMPLES_BYTES} bytes")

    n = 0
    prev_seq = prev_mono = prev_host_mono = None
    obs_ids, clock_domains = set(), set()
    invocations, boot_ids, units, cgroups, inodes = set(), set(), set(), set(), set()
    host_points: dict = {}
    pressure_points: dict = {}
    pressure_present: dict = {}
    target_max: dict = {}
    lags, caps = [], []
    baseline_seen = False
    host_unknown_cells = 0
    for number, raw_line in stream_sample_lines(directory / "samples.jsonl", total_bytes):
        n = number
        where = f"samples.jsonl line {n}"
        sample = parse_json(raw_line, where)
        req_dict(sample, where)
        obs = req_dict(sample.get("observation"), f"{where} observation")
        req_version(obs.get("schemaVersion"), f"{where} schemaVersion")
        seq = req_int(obs.get("sequence"), f"{where} sequence")
        mono = req_int(obs.get("observedMonotonicMs"), f"{where} timestamp")
        opt_unix_ms(obs.get("observedUnixMs"), f"{where} observation wall clock")
        lag = req_int(sample.get("scheduleLagMs"), f"{where} scheduleLagMs")
        cap = req_int(sample.get("captureDurationUs"), f"{where} captureDurationUs")
        files = req_dict(obs.get("files"), f"{where} files")
        target = req_dict(sample.get("target"), f"{where} target")
        clock = req_id(sample.get("clockDomain"), f"{where} clockDomain")
        clock_domains.add(clock)
        obs_id = req_id(sample.get("observationId"), f"{where} observationId")
        obs_ids.add(obs_id)
        unit = req_id(target.get("unit"), f"{where} target unit")
        units.add(unit)
        cgroup = opt_str(target.get("cgroupPath"), f"{where} target cgroupPath")
        if cgroup is not None:
            cgroups.add(cgroup)
        obs_path = opt_str(obs.get("path"), f"{where} observation path")
        if obs_path is not None and cgroup is not None and obs_path != cgroup:
            raise InvalidArtifact(f"{where} observation path disagrees with target cgroup")
        if "inode" in obs:
            # Absent inode is explicit missing evidence; differing known
            # inodes mean same-path replacement, which the live observer
            # ends rather than bridging.
            inodes.add(req_int(obs.get("inode"), f"{where} inode"))
        for label, value in (("observation invocationId", obs.get("invocationId")),
                             ("target invocationId", target.get("invocationId"))):
            if value is None:
                continue
            text = opt_str(value, f"{where} {label}")
            if text:
                invocations.add(text)
        boot = opt_str(obs.get("bootId"), f"{where} bootId")
        if boot:
            boot_ids.add(boot)
        if prev_seq is None and seq != 0:
            raise InvalidArtifact(f"samples.jsonl does not start at sequence 0 (line {n})")
        if prev_seq is not None and seq != prev_seq + 1:
            raise InvalidArtifact(f"sequences not contiguous at line {n}")
        if prev_mono is not None and mono <= prev_mono:
            raise InvalidArtifact(f"timestamps do not strictly increase at line {n}")
        prev_seq, prev_mono = seq, mono
        lags.append(lag)
        caps.append(cap)
        host = sample.get("host")
        if host is not None:
            host = req_dict(host, f"{where} host")
            host_mono = req_int(host.get("observedMonotonicMs"), f"{where} host timestamp")
            opt_unix_ms(host.get("observedUnixMs"), f"{where} host wall clock")
            if prev_host_mono is not None and host_mono <= prev_host_mono:
                raise InvalidArtifact(f"host timestamps do not strictly increase at line {n}")
            prev_host_mono = host_mono
            host_files = req_dict(host.get("files"), f"{where} host files")
            for key, cell in host_files.items():
                if not check_cell(cell, f"{where} host cell"):
                    host_unknown_cells += 1
                elif key in SCALAR_HOST_KEYS:
                    value = counter_value(cell["value"], f"{where} host {key}")
                    host_points.setdefault(key, []).append((host_mono, value))
                elif key in PRESSURE_HOST_KEYS:
                    rows = read_pressure_rows(cell["value"], where, key)
                    pressure_present[key] = pressure_present.get(key, 0) + 1
                    for row, total in rows.items():
                        pressure_points.setdefault((key, row), []).append((host_mono, total))
        for key in ("memory.current", "memory.swap.current"):
            value = known_strict(files, key, where)
            if value is not None:
                value = counter_value(value, f"{where} {key}")
                target_max[key] = max(value, target_max.get(key, 0))
        mem_events = known_strict(files, "memory.events", where)
        cg_events = known_strict(files, "cgroup.events", where)
        if mem_events is not None:
            event_map(mem_events, where, "memory.events", non_empty=True)
        if cg_events is not None:
            event_map(cg_events, where, "cgroup.events", non_empty=True)
        if mem_events is not None and cg_events is not None:
            baseline_seen = True

    if len(clock_domains) > 1 or (clock_domains and CLOCK_DOMAIN not in clock_domains):
        raise InvalidArtifact(f"samples span unsupported clock domains: {sorted(clock_domains)}")
    if len(obs_ids) > 1:
        raise InvalidArtifact("samples span multiple observation IDs")
    if obs_ids and manifest_obs_id not in obs_ids:
        raise InvalidArtifact("samples observationId disagrees with manifest")
    if len(units) > 1:
        raise InvalidArtifact(f"samples span multiple target units: {sorted(units)}")
    if manifest_unit not in units and units:
        raise InvalidArtifact("samples target unit disagrees with manifest")
    if len(cgroups) > 1:
        raise InvalidArtifact(f"samples span multiple target cgroups: {sorted(cgroups)}")
    if manifest_cgroup is not None and cgroups and manifest_cgroup not in cgroups:
        raise InvalidArtifact("samples target cgroup disagrees with manifest")
    if len(invocations) > 1:
        raise InvalidArtifact(f"samples span multiple invocations: {sorted(invocations)}")
    if manifest_invocation is not None and invocations and manifest_invocation not in invocations:
        raise InvalidArtifact("samples invocation disagrees with manifest")
    if len(boot_ids) > 1:
        raise InvalidArtifact("samples span multiple boot IDs")
    if manifest_boot is not None and boot_ids and manifest_boot not in boot_ids:
        raise InvalidArtifact("samples boot ID disagrees with manifest")
    if len(inodes) > 1:
        raise InvalidArtifact("samples span multiple cgroup inodes")

    summary_path = directory / "summary.json"
    if summary_path.exists():
        summary_raw = parse_json(read_bounded(summary_path, "summary.json"), "summary.json")
        if summary_raw is None:
            raise InvalidArtifact("summary.json is null, not a missing summary")
        req_dict(summary_raw, "summary.json")
        req_version(summary_raw.get("schemaVersion"), "summary schemaVersion")
        summary = summary_raw
    else:
        summary = {}
    if summary:
        req_id(summary.get("observationId"), "summary observationId")
        summary_target: dict = req_dict(summary.get("target"), "summary target")
        req_id(summary_target.get("unit"), "summary target unit")
        if not isinstance(summary.get("reason"), str):
            raise InvalidArtifact("summary reason is not a string")
        if not isinstance(summary.get("complete"), bool):
            raise InvalidArtifact("summary complete is not a boolean")
        req_dict(summary.get("coverage"), "summary coverage")
        req_dict(summary.get("collection"), "summary collection")
        for key in ("unit", "cgroupPath", "invocationId"):
            mine, theirs = manifest_target.get(key), summary_target.get(key)
            if mine is not None and theirs is not None and mine != theirs:
                raise InvalidArtifact(f"summary target {key} disagrees with manifest")
        if manifest_obs_id != summary.get("observationId"):
            raise InvalidArtifact("summary observationId disagrees with manifest")
    collection = summary.get("collection", {}) if summary else {}
    reconciled = False
    if summary:
        persisted = collection.get("persistedSamples")
        written = collection.get("bytesWritten")
        for label, count in (("persistedSamples", persisted), ("bytesWritten", written)):
            if isinstance(count, bool) or not isinstance(count, int) or count < 0:
                raise InvalidArtifact(f"summary {label} is not a non-negative integer")
        if persisted != n:
            raise InvalidArtifact("summary persistedSamples disagrees with samples.jsonl lines")
        if written != total_bytes:
            raise InvalidArtifact("summary bytesWritten disagrees with samples.jsonl size")
        attempted = collection.get("attemptedSamples")
        if not isinstance(attempted, int) or isinstance(attempted, bool) or attempted < n:
            raise InvalidArtifact("summary attemptedSamples contradicts persisted lines")
        reconciled = True

    metrics: dict = {"samples": n}
    levels: dict = {}
    for key, label in (("meminfo.MemAvailable", "memAvailableBytes"), ("meminfo.SwapFree", "swapFreeBytes")):
        raw = host_points.get(key, [])
        try:
            points = [(mono, counter_value(value, label)) for mono, value in raw]
        except InvalidArtifact as error:
            raise InvalidArtifact(f"{label} malformed known counter: {_short(error)}") from error
        if len(points) != n or not points:
            levels[label] = None
            report["suppressed"].append(f"{label}:partial-coverage" if n else f"{label}:no-samples")
            continue
        values = [value for _, value in points]
        levels[label] = {"first": values[0], "last": values[-1], "min": min(values),
                         "knownPoints": len(points), "totalPoints": n}
    metrics.update(levels)
    for key, label, unit in (("vmstat.pswpin", "swapPagesIn", "pages"),
                             ("vmstat.pswpout", "swapPagesOut", "pages"),
                             ("vmstat.pgmajfault", "majorFaults", "events")):
        points = host_points.get(key, [])
        if len(points) != n:
            metrics[label] = None
            report["suppressed"].append(f"{label}:partial-coverage" if n else f"{label}:no-samples")
            continue
        delta, per_second, window_ms = windowed_rate(points, n, label, report)
        metrics[label] = {"unit": unit, "delta": delta, "perSecond": per_second,
                          "windowMs": window_ms, "knownPoints": len(points), "totalPoints": n}
    for resource in ("memory", "cpu", "io"):
        totals: dict = {}
        key = f"pressure.{resource}"
        if pressure_present.get(key, 0) != n:
            for row in ("some", "full"):
                totals[row] = None
                report["suppressed"].append(f"pressure.{resource}.{row}:partial-coverage" if n
                                           else f"pressure.{resource}.{row}:no-samples")
            metrics[f"pressure{resource.capitalize()}"] = totals
            continue
        for row in ("some", "full"):
            cells = pressure_points.get((key, row), [])
            if len(cells) != n:
                totals[row] = None
                report["suppressed"].append(f"pressure.{resource}.{row}:unavailable")
                continue
            delta, _, window_ms = windowed_rate(cells, n, f"pressure.{resource}.{row}", report)
            if delta is None:
                totals[row] = None
                continue
            percent = delta / (window_ms * 1000) * 100 if window_ms else None
            if percent is not None and percent > 100:
                totals[row] = {"deltaUs": delta, "percentOfElapsed": None,
                               "windowMs": window_ms, "overshoot": True}
                report["suppressed"].append(f"pressure.{resource}.{row}:psi-overshoot")
            else:
                totals[row] = {"deltaUs": delta, "percentOfElapsed": percent, "windowMs": window_ms}
        metrics[f"pressure{resource.capitalize()}"] = totals
    metrics["scheduleLagMs"] = {"max": max(lags) if lags else None,
                                "p99": percentile_nearest_rank(lags, 99)}
    metrics["captureDurationUs"] = {"max": max(caps) if caps else None,
                                    "p99": percentile_nearest_rank(caps, 99),
                                    "note": "read-path timing, not whole-observer CPU"}
    metrics["targetSampledMax"] = target_max
    metrics["hostUnknownCells"] = host_unknown_cells
    metrics["invocations"] = sorted(invocations)

    done_exists = marker_is_file(directory, "done")
    ready_exists = marker_is_file(directory, "ready")
    report["manifest"] = {"production": manifest.get("production"), "seconds": manifest.get("seconds"),
                          "intervalMs": manifest.get("intervalMs"), "target": manifest.get("target")}
    report["markers"] = {"ready": ready_exists, "done": done_exists}
    report["integrity"] = {"lines": n,
                           "sequencesContiguous": True,
                           "timestampsStrictlyIncreasing": True,
                           "countsReconciled": reconciled}
    reason = summary.get("reason", "no-summary") if summary else "no-summary"
    coverage = summary.get("coverage") or {}
    if not isinstance(coverage, dict):
        raise InvalidArtifact("summary coverage is not an object")
    contradictions = []
    if summary.get("complete") is True:
        if n == 0:
            contradictions.append("complete without samples")
        if not baseline_seen:
            contradictions.append("complete without baseline measurements in samples")
        if not done_exists:
            contradictions.append("complete without done marker file")
        if not ready_exists:
            contradictions.append("complete without ready marker file")
        if reason not in COMPLETE_REASONS:
            contradictions.append(f"complete with reason {reason!r}")
        if coverage.get("validBaseline") is not True:
            contradictions.append("complete without valid baseline")
        if coverage.get("incompletePersistence") is not False:
            contradictions.append("complete with incomplete persistence")
        if collection.get("storageDurable") is not True:
            contradictions.append("complete without durable storage")
        if contradictions:
            raise InvalidArtifact("contradictory completion evidence: " + "; ".join(contradictions))
    complete = summary.get("complete") is True and done_exists and not contradictions
    report["collection"] = {"reason": reason,
                            "complete": complete,
                            "coverage": summary.get("coverage") if summary else None,
                            "attemptedSamples": collection.get("attemptedSamples"),
                            "persistedSamples": collection.get("persistedSamples"),
                            "missedIntervals": collection.get("missedIntervals"),
                            "eventDeltas": summary.get("eventDeltas") if summary else None}
    report["identity"] = {"clockDomain": sorted(clock_domains)[0] if clock_domains else None,
                          "observationIds": sorted(obs_ids),
                          "invocations": sorted(invocations),
                          "bootIds": sorted(boot_ids),
                          "units": sorted(units),
                          "inodes": sorted(inodes)}
    report["metrics"] = metrics
    report["checks"].append("ok")
    return report


def percentile_nearest_rank(values, pct):
    """Nearest-rank percentile: rank=ceil(p/100*n), 1-indexed into sorted data."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(pct / 100 * len(ordered)))
    return ordered[min(rank, len(ordered)) - 1]


def format_percent(totals):
    if not totals or not totals.get("windowMs"):
        return "unavailable"
    percent = totals.get("percentOfElapsed")
    rendered = f"{percent:.3f}" if percent is not None else "OVERSHOOT"
    return (f"deltaUs={totals['deltaUs']} percentOfElapsed={rendered} "
            f"windowMs={totals['windowMs']}")


def as_markdown(report):
    lines = [f"# Capture validation: {report['directory']}", ""]
    collection = report["collection"]
    lines.append(f"Collection: reason={collection['reason']} complete={collection['complete']}")
    identity = report["identity"]
    lines.append(f"Identity: clock={identity['clockDomain']} observations={len(identity['observationIds'])} "
                 f"units={len(identity['units'])} invocations={len(identity['invocations'])} "
                 f"boots={len(identity['bootIds'])} inodes={len(identity['inodes'])}")
    metrics = report["metrics"]
    lines.append(f"Samples: {metrics['samples']} reconciled={report['integrity']['countsReconciled']}")
    for label in ("memAvailableBytes", "swapFreeBytes"):
        values = metrics[label]
        lines.append(f"{label}: {values}" if values else f"{label}: unavailable")
    for label in ("swapPagesIn", "swapPagesOut", "majorFaults"):
        values = metrics[label]
        if values and values.get("windowMs"):
            lines.append(f"{label}: delta={values['delta']} perSecond={values['perSecond']:.3f} "
                         f"windowMs={values['windowMs']} points={values['knownPoints']}/{values['totalPoints']}")
        else:
            lines.append(f"{label}: {values}")
    for resource in ("Memory", "Cpu", "Io"):
        for row in ("some", "full"):
            lines.append(f"pressure.{resource.lower()}.{row}: "
                         f"{format_percent(report['metrics'][f'pressure{resource}'][row])}")
    lines.append(f"timing: {metrics['scheduleLagMs']} {metrics['captureDurationUs']}")
    lines.append(f"unknown host cells: {metrics['hostUnknownCells']}")
    if report["suppressed"]:
        lines.append(f"suppressed: {', '.join(report['suppressed'])}")
    return "\n".join(lines) + "\n"


def main(argv):
    if len(argv) not in (2, 3) or (len(argv) == 3 and argv[2] != "--markdown"):
        print("usage: validate-capture.py <capture-dir> [--markdown]", file=sys.stderr)
        return 3
    try:
        report = validate(argv[1])
    except InvalidArtifact as error:
        print(json.dumps({"error": str(error), "directory": argv[1]}, indent=1))
        return 2
    if len(argv) == 3:
        print(as_markdown(report), end="")
    else:
        print(json.dumps(report, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
