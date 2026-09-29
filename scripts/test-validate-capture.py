"""Assert-based regressions for validate-capture.py (no fixtures needed).

Fixtures mirror the real producer envelope: observationId, clockDomain,
target identity with cgroup path, observation path, boot ID, and per-sample
host clocks. Each malformed case differs from a valid capture in exactly
one relevant way.
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("validate_capture", root / "validate-capture.py")
assert spec is not None and spec.loader is not None
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)

TARGET = {"unit": "test.service", "cgroupPath": "/test.service", "invocationId": "inv-1"}


def psi_row(total):
    return {"total": total, "avg10": 1.0, "avg60": 0.5, "avg300": 0.1}


def sample(seq, mono, host_mono=None, invocation="inv-1", unit="test.service",
           boot="boot-1", obs_id="obs-1", clock="monotonic-clock", inode=10,
           mem_available=8_000_000_000, pgmajfault=None, pressure_total=None,
           unknown_mem=False):
    host_files = {"meminfo.MemAvailable": {"value": mem_available, "unknown": None},
                  "unrelated.key": {"value": {"nested": list(range(100))}, "unknown": None}}
    if unknown_mem:
        host_files["meminfo.MemAvailable"] = {"unknown": "missing"}
    if pgmajfault is not None:
        host_files["vmstat.pgmajfault"] = {"value": pgmajfault, "unknown": None}
    if pressure_total is not None:
        host_files["pressure.memory"] = {
            "value": {"some": psi_row(pressure_total), "full": psi_row(pressure_total // 2)},
            "unknown": None}
    return {"observationId": obs_id, "clockDomain": clock,
            "observation": {"schemaVersion": 1, "sequence": seq, "observedMonotonicMs": mono,
                            "observedUnixMs": 1_700_000_000_000 + mono,
                            "invocationId": invocation, "bootId": boot,
                            "path": "/test.service", "inode": inode,
                            "files": {"memory.events": {"value": {"oom": 0}, "unknown": None},
                                      "cgroup.events": {"value": {"populated": 1}, "unknown": None}}},
            "target": {**TARGET, "unit": unit, "invocationId": invocation},
            "host": {"observedMonotonicMs": mono if host_mono is None else host_mono,
                     "observedUnixMs": 1_700_000_000_000 + mono,
                     "files": host_files},
            "scheduleLagMs": 0, "captureDurationUs": 100}


def write_capture(directory, samples, summary_extra=None, manifest_extra=None,
                  ready=True, summary=True, trailing_newline=True, raw_lines=None):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    manifest = {"schemaVersion": 1, "observationId": "obs-1", "bootId": "boot-1",
                "production": True, "seconds": 60, "intervalMs": 1000, "target": dict(TARGET)}
    manifest.update(manifest_extra or {})
    (directory / "manifest.json").write_text(json.dumps(manifest))
    text = raw_lines if raw_lines is not None else "".join(json.dumps(sample) + "\n" for sample in samples)
    (directory / "samples.jsonl").write_bytes(
        text.encode() if trailing_newline else text.rstrip("\n").encode())
    if ready:
        (directory / "ready").write_bytes(b"")
    if summary:
        persisted = len(samples) if raw_lines is None else raw_lines.count("\n")
        extra = dict(summary_extra or {})
        coll = {"attemptedSamples": persisted, "persistedSamples": persisted,
                "storageDurable": True, "bytesWritten": len(text.encode())}
        coll.update(extra.pop("collection", {}))
        final = {"schemaVersion": 1, "observationId": "obs-1", "target": dict(TARGET)}
        final.update(extra)
        final.setdefault("reason", "deadline")
        final.setdefault("complete", True)
        final.setdefault("coverage", {"validBaseline": True, "incompletePersistence": False})
        final["collection"] = coll
        (directory / "summary.json").write_text(json.dumps(final))
        (directory / "done").write_bytes(b"")
    return len(text.encode())


def rejects(directory):
    try:
        validator.validate(directory)
    except validator.InvalidArtifact:
        return
    raise AssertionError(f"corrupt capture {directory} was accepted")


with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    # Happy path: 100 -> 160 faults over 2000ms = 30/s; nearest-rank p99 of
    # lags [0..9] (n=10) is rank ceil(0.99*10)=10 -> value 9. Unrelated host
    # payloads are ignored, never echoed into the report.
    samples = [sample(seq, seq * 1000, pgmajfault=100 + 30 * seq,
                      pressure_total=1_000_000 + 5_000 * seq) for seq in range(3)]
    lagged = [dict(s, scheduleLagMs=i) for i, s in
              enumerate([sample(seq, seq * 100) for seq in range(10)])]
    write_capture(directory / "ok", samples)
    report = validator.validate(directory / "ok")
    faults = report["metrics"]["majorFaults"]
    assert faults["delta"] == 60 and faults["perSecond"] == 30, faults
    assert faults["windowMs"] == 2000 and faults["knownPoints"] == 3
    mem = report["metrics"]["pressureMemory"]
    assert mem["some"]["deltaUs"] == 10_000, mem
    assert abs(mem["some"]["percentOfElapsed"] - 10_000 / 2_000_000 * 100) < 1e-9, mem
    assert report["collection"]["complete"] is True
    assert report["integrity"]["countsReconciled"] is True
    assert report["metrics"]["hostUnknownCells"] == 0
    assert report["identity"]["clockDomain"] == "monotonic-clock"
    assert "nested" not in json.dumps(report)
    hostile = {**report, "directory": "capture\n## Forged <script> ![img](https://example.invalid)",
               "collection": {**report["collection"], "complete": False,
                              "reason": "cancelled\nCollection: complete=True<script>"},
               "identity": {**report["identity"], "clockDomain": None}}
    markdown = validator.as_markdown(hostile)
    assert "\n## Forged" not in markdown
    assert "<script>" not in markdown
    assert "Forged" in markdown and "\\n" in markdown
    assert "&lt;script&gt;" in markdown
    assert "![img](https://example.invalid)" not in markdown
    assert "clock=unavailable" in markdown
    write_capture(directory / "pct", lagged)
    assert validator.validate(directory / "pct")["metrics"]["scheduleLagMs"]["p99"] == 9

    # Host rates use the host clock, not the observation clock: 120 faults
    # over a 4000ms host window with a 2000ms observation window is 30/s.
    clocked = [sample(seq, seq * 1000, host_mono=seq * 2000, pgmajfault=100 + 60 * seq)
               for seq in range(3)]
    write_capture(directory / "hostclock", clocked)
    assert validator.validate(directory / "hostclock")["metrics"]["majorFaults"]["perSecond"] == 30
    write_capture(directory / "hostorder", [clocked[0],
                                            dict(clocked[1], host={"observedMonotonicMs": 0,
                                                                   "files": clocked[1]["host"]["files"]}),
                                            clocked[2]])
    rejects(directory / "hostorder")

    # Truncated final line is invalid, not partial success.
    write_capture(directory / "truncated", samples, trailing_newline=False)
    rejects(directory / "truncated")

    # Count mismatch, non-contiguous sequences, non-increasing timestamps invalid.
    write_capture(directory / "mismatch", samples,
                  summary_extra={"collection": {"attemptedSamples": 3, "persistedSamples": 2,
                                                "storageDurable": True, "bytesWritten": 999999}})
    rejects(directory / "mismatch")
    write_capture(directory / "gap", samples[1:])
    rejects(directory / "gap")
    mutated = [samples[0], dict(samples[1], observation={**samples[1]["observation"],
                                                         "observedMonotonicMs": 0}), samples[2]]
    write_capture(directory / "time", mutated)
    rejects(directory / "time")

    # Strict schema: booleans, NaN, duplicates, bad versions, 1e400 rejected.
    # Each envelope is otherwise valid, so only the targeted defect can fail.
    write_capture(directory / "bool", [dict(samples[0], scheduleLagMs=True), samples[1], samples[2]])
    rejects(directory / "bool")
    write_capture(directory / "version", [dict(samples[0], observation={**samples[0]["observation"],
                                                                        "schemaVersion": True})])
    rejects(directory / "version")
    raw = "".join(json.dumps(sample) + "\n" for sample in samples)
    write_capture(directory / "nan", [], raw_lines=raw.replace("100,", "NaN,", 1))
    rejects(directory / "nan")
    write_capture(directory / "inf", [], raw_lines=raw.replace("100,", "1e400,", 1))
    rejects(directory / "inf")
    dup_key = "x" * 300
    write_capture(directory / "dup", [],
                  raw_lines=raw.replace('"sequence": 0,', f'"sequence": 0, "{dup_key}": 1, "{dup_key}": 2,', 1))
    try:
        validator.validate(directory / "dup")
    except validator.InvalidArtifact as error:
        assert dup_key not in str(error), "duplicate key echoed into error"
    else:
        raise AssertionError("corrupt capture dup was accepted")

    # Malformed known counters are invalid, not suppressed: negative,
    # fractional, and malformed pressure totals.
    write_capture(directory / "neg", [sample(0, 0, pgmajfault=-1), sample(1, 1000, pgmajfault=1),
                                      sample(2, 2000, pgmajfault=2)])
    rejects(directory / "neg")
    write_capture(directory / "frac", [sample(0, 0, pgmajfault=1.5), sample(1, 1000, pgmajfault=2),
                                       sample(2, 2000, pgmajfault=3)])
    rejects(directory / "frac")
    bad_pressure = [dict(s, host={"observedMonotonicMs": s["host"]["observedMonotonicMs"],
                                  "files": {"pressure.memory": {"value": {"some": psi_row("x"),
                                                                         "full": psi_row(0)},
                                                                "unknown": None}}})
                    for s in samples]
    write_capture(directory / "badpressure", bad_pressure)
    rejects(directory / "badpressure")
    # Impossible PSI fractions are suppressed, never published.
    over = [sample(seq, seq * 1000, pressure_total=seq * 3_000_000) for seq in range(3)]
    write_capture(directory / "overshoot", over)
    report = validator.validate(directory / "overshoot")
    assert report["metrics"]["pressureMemory"]["some"]["percentOfElapsed"] is None
    assert report["metrics"]["pressureMemory"]["some"]["overshoot"] is True
    assert any("psi-overshoot" in item for item in report["suppressed"])
    # CPU pressure without a full row is unavailable, not malformed.
    cpu_only = [dict(s, host={"observedMonotonicMs": s["host"]["observedMonotonicMs"],
                              "observedUnixMs": s["host"]["observedUnixMs"],
                              "files": {"pressure.cpu": {"value": {"some": psi_row(1000 * seq)},
                                                         "unknown": None}}})
                for seq, s in enumerate(samples)]
    write_capture(directory / "cpusome", cpu_only)
    report = validator.validate(directory / "cpusome")
    assert report["metrics"]["pressureCpu"]["some"]["deltaUs"] == 2000
    assert report["metrics"]["pressureCpu"]["full"] is None
    # Invalid known PSI shapes are rejected, not reported unavailable: an
    # empty memory object, IO with only `some`, and CPU with only `full`.
    psi_shapes = (("psiempty", "pressure.memory", {}),
                  ("psionoonly", "pressure.io", {"some": psi_row(100)}),
                  ("psifullonly", "pressure.cpu", {"full": psi_row(100)}),
                  ("psiextra", "pressure.memory", {"some": psi_row(0), "full": psi_row(0), "extra": psi_row(0)}),
                  ("psinull", "pressure.cpu", {"some": psi_row(0), "full": None}),
                  ("psihuge", "pressure.cpu", {"some": {**psi_row(0), "avg10": 10**400}}))
    for label, key, value in psi_shapes:
        bad_psi = [dict(s, host={"observedMonotonicMs": s["host"]["observedMonotonicMs"],
                                 "observedUnixMs": s["host"]["observedUnixMs"],
                                 "files": {key: {"value": value, "unknown": None}}})
                   for s in samples]
        write_capture(directory / label, bad_psi)
        rejects(directory / label)

    for boundary in (0, 100):
        value = {"some": {"total": 0, "avg10": boundary, "avg60": boundary, "avg300": boundary}}
        assert validator.read_pressure_rows(value, "boundary", "pressure.cpu") == {"some": 0}

    # Identity: observation ID, unit, cgroup, path, invocation, boot, and
    # clock mismatches are invalid, not averaged. Each case is one defect.
    write_capture(directory / "obsid", [dict(samples[0], observationId="obs-2"),
                                        samples[1], samples[2]])
    rejects(directory / "obsid")
    write_capture(directory / "unit", [sample(0, 0), sample(1, 1000, unit="other.service"),
                                       sample(2, 2000)])
    rejects(directory / "unit")
    write_capture(directory / "cgroup", [dict(samples[0], target={**TARGET, "cgroupPath": "/other.service"}),
                                         samples[1], samples[2]])
    rejects(directory / "cgroup")
    write_capture(directory / "obspath", [dict(samples[0], observation={**samples[0]["observation"],
                                                                        "path": "/other.service"}),
                                          samples[1], samples[2]])
    rejects(directory / "obspath")
    write_capture(directory / "inv", [sample(0, 0), sample(1, 1000, invocation="inv-2"),
                                      sample(2, 2000)])
    rejects(directory / "inv")
    write_capture(directory / "nonstring", [dict(samples[0], observation={**samples[0]["observation"],
                                                                          "invocationId": 123}),
                                            samples[1], samples[2]])
    rejects(directory / "nonstring")
    write_capture(directory / "objunit", [dict(samples[0], target={"unit": {}}),
                                          samples[1], samples[2]])
    rejects(directory / "objunit")
    write_capture(directory / "boot", [sample(0, 0), sample(1, 1000, boot="boot-2"),
                                       sample(2, 2000)])
    rejects(directory / "boot")
    # Same-path replacement after a valid baseline: differing known inodes
    # cannot produce a continuous-identity report.
    write_capture(directory / "inode", [sample(0, 0, inode=10), sample(1, 1000, inode=10),
                                        sample(2, 2000, inode=11)])
    rejects(directory / "inode")
    write_capture(directory / "manifestboot", samples, manifest_extra={"bootId": "boot-9"})
    rejects(directory / "manifestboot")
    write_capture(directory / "manifestnoid", samples, manifest_extra={"observationId": None})
    rejects(directory / "manifestnoid")
    write_capture(directory / "summaryid", samples, summary_extra={"observationId": "obs-9"})
    rejects(directory / "summaryid")
    write_capture(directory / "summarynotarget", samples, summary_extra={"target": None})
    rejects(directory / "summarynotarget")
    write_capture(directory / "clock", [dict(samples[0], clockDomain="other-clock"),
                                        samples[1], samples[2]])
    rejects(directory / "clock")

    # Completion requires baseline measurements in the samples, not just flags.
    nobase = [dict(s, observation={**s["observation"], "files": {}}) for s in samples]
    write_capture(directory / "nobaseline", nobase)
    rejects(directory / "nobaseline")
    # A cell reporting both a value and an unknown reason is malformed.
    contra_cell = [dict(samples[0], observation={**samples[0]["observation"], "files": {
        **samples[0]["observation"]["files"],
        "memory.events": {"value": {"oom": 0}, "unknown": "missing"}}}),
        samples[1], samples[2]]
    write_capture(directory / "contracell", contra_cell)
    rejects(directory / "contracell")
    # Malformed events beside an unknown companion are still invalid: the
    # first sample establishes a valid baseline, so only independent event
    # validation can reject the malformed known map in the second sample.
    bad_middle = dict(samples[1], observation={**samples[1]["observation"], "files": {
        **samples[1]["observation"]["files"],
        "memory.events": {"value": {"oom": "BAD"}, "unknown": None},
        "cgroup.events": {"value": None, "unknown": "missing-or-disappeared"}}})
    write_capture(directory / "badwithunknown", [samples[0], bad_middle, samples[2]])
    rejects(directory / "badwithunknown")
    # An empty known cgroup-event map is malformed, like memory.events.
    empty_cg = [dict(s, observation={**s["observation"], "files": {
        **s["observation"]["files"],
        "cgroup.events": {"value": {}, "unknown": None}}}) for s in samples]
    write_capture(directory / "emptycg", empty_cg)
    rejects(directory / "emptycg")
    # An explicit null target cell is malformed, not an absent field.
    nulled = [samples[0],
              dict(samples[1], observation={**samples[1]["observation"], "files": {
                  **samples[1]["observation"]["files"], "memory.current": None}}),
              samples[2]]
    write_capture(directory / "nullcell", nulled)
    rejects(directory / "nullcell")
    # Known zero is a value: it participates in rates and extrema.
    zero = [sample(0, 0, pgmajfault=0, mem_available=0),
            sample(1, 1000, pgmajfault=0, mem_available=0),
            sample(2, 2000, pgmajfault=5, mem_available=10)]
    write_capture(directory / "zero", zero)
    report = validator.validate(directory / "zero")
    assert report["metrics"]["majorFaults"]["delta"] == 5
    assert report["metrics"]["memAvailableBytes"]["min"] == 0
    # One sample makes True == 1 pass reconciliation, so only the type guard rejects it.
    write_capture(directory / "boolcount1", samples[:1], summary_extra={"collection": {"persistedSamples": True}})
    rejects(directory / "boolcount1")
    # Producer unknown shape {"value": null, "unknown": reason} is valid
    # missing evidence, even in a completed capture; known zero stays known.
    prod_unknown = []
    for seq in range(3):
        s = sample(seq, seq * 1000, pgmajfault=100 + 30 * seq, mem_available=0)
        s["host"]["files"]["meminfo.MemAvailable"] = {"value": None, "unknown": "missing-or-disappeared"}
        prod_unknown.append(s)
    write_capture(directory / "produnknown", prod_unknown)
    report = validator.validate(directory / "produnknown")
    assert report["collection"]["complete"] is True
    assert report["metrics"]["memAvailableBytes"] is None
    assert report["metrics"]["hostUnknownCells"] == 3
    assert report["metrics"]["majorFaults"]["delta"] == 60
    # Neither-set, non-string reasons, and zero-as-unknown are malformed.
    for label, cell in (("bothnull", {"value": None, "unknown": None}),
                        ("badreason", {"value": None, "unknown": False}),
                        ("emptycell", {})):
        bad = [dict(s, host={**s["host"], "files": {"meminfo.MemAvailable": cell,
                                                    "vmstat.pgmajfault": {"value": 1, "unknown": None}}})
               for s in samples]
        write_capture(directory / label, bad)
        rejects(directory / label)
    # Malformed event counters cannot establish a baseline.
    bad_events = [dict(s, observation={**s["observation"], "files": {
        **s["observation"]["files"],
        "memory.events": {"value": {"oom": "BAD"}, "unknown": None}}}) for s in samples]
    write_capture(directory / "badevents", bad_events)
    rejects(directory / "badevents")
    # Summary counts must be integers: floats, booleans, negatives rejected.
    for label, coll in (("floatcount", {"persistedSamples": 3.0}),
                        ("boolcount", {"persistedSamples": True}),
                        ("negbytes", {"bytesWritten": -1})):
        write_capture(directory / label, samples, summary_extra={"collection": coll})
        rejects(directory / label)
    # An existing summary containing null is malformed, not missing.
    write_capture(directory / "summarynull", samples)
    (directory / "summarynull" / "summary.json").write_text("null")
    rejects(directory / "summarynull")
    # Missing inputs are invalid, not crashes.
    try:
        validator.validate(directory / "does-not-exist")
    except validator.InvalidArtifact:
        pass
    else:
        raise AssertionError("missing directory was accepted")
    # Descriptor rejection closes the descriptor and never blocks: the open
    # must be nonblocking and no-follow even before the type check runs.
    opened, closed = [], []
    real_open, real_close, real_fstat = os.open, os.close, os.fstat
    dir_fd = real_open(str(directory), os.O_RDONLY)
    try:
        dir_stat = real_fstat(dir_fd)
    finally:
        real_close(dir_fd)

    def fake_open(path, flags):
        opened.append(flags)
        return 999999

    def fake_close(fd):
        closed.append(fd)

    os.open, os.close = fake_open, fake_close
    os.fstat = lambda fd: dir_stat
    try:
        validator.open_regular(directory / "ok" / "manifest.json", "manifest.json")
    except validator.InvalidArtifact:
        pass
    else:
        raise AssertionError("non-regular descriptor was accepted")
    finally:
        os.open, os.close, os.fstat = real_open, real_close, real_fstat
    assert closed == [999999], closed
    assert opened and all(flags & os.O_NONBLOCK and flags & os.O_NOFOLLOW for flags in opened)

    # Each completion guard fires independently; the fixture is otherwise valid.
    def contra_case(name, **summary_fields):
        write_capture(directory / name, samples[:2], summary_extra={
            "reason": "deadline", "complete": True,
            "coverage": {"validBaseline": True, "incompletePersistence": False},
            "collection": {"attemptedSamples": 2, "storageDurable": True},
            **summary_fields})
        rejects(directory / name)

    contra_case("contra-reason", reason="cancelled")
    contra_case("contra-baseline", coverage={"validBaseline": False, "incompletePersistence": False})
    contra_case("contra-persist", coverage={"validBaseline": True, "incompletePersistence": True})
    contra_case("contra-durable", collection={"attemptedSamples": 2, "storageDurable": False})
    write_capture(directory / "contra-empty", [],
                  summary_extra={"reason": "deadline", "complete": True,
                                 "coverage": {"validBaseline": True, "incompletePersistence": False},
                                 "collection": {"attemptedSamples": 0, "storageDurable": True}})
    rejects(directory / "contra-empty")

    # Intermediate reset suppresses the rate even when endpoints agree.
    reset = [sample(0, 0, pgmajfault=100), sample(1, 1000, pgmajfault=50),
             sample(2, 2000, pgmajfault=100)]
    write_capture(directory / "reset", reset)
    report = validator.validate(directory / "reset")
    assert report["metrics"]["majorFaults"]["delta"] is None
    assert any("counter-decreased" in item for item in report["suppressed"])

    # A missing key suppresses the metric with partial coverage.
    partial = [sample(0, 0, pgmajfault=100), sample(1, 1000, unknown_mem=True),
               sample(2, 2000, pgmajfault=160)]
    write_capture(directory / "partial", partial)
    report = validator.validate(directory / "partial")
    assert report["metrics"]["majorFaults"] is None
    assert any("partial-coverage" in item for item in report["suppressed"])
    # An unknown middle endpoint keeps the level but suppresses the rate.
    mid_unknown = [sample(0, 0, pgmajfault=100),
                   dict(sample(1, 1000, pgmajfault=130),
                        host={"observedMonotonicMs": 1000, "observedUnixMs": 1_700_000_001_000,
                              "files": {"vmstat.pgmajfault": {"unknown": "missing"}}}),
                   sample(2, 2000, pgmajfault=160)]
    write_capture(directory / "midunknown", mid_unknown)
    report = validator.validate(directory / "midunknown")
    assert report["metrics"]["majorFaults"] is None
    assert any("partial-coverage" in item for item in report["suppressed"])

    # Markers: complete:true without a done file is contradictory; a done
    # directory is not a marker. No summary reconciles nothing.
    write_capture(directory / "nodone", samples)
    (directory / "nodone" / "done").unlink()
    rejects(directory / "nodone")
    write_capture(directory / "donedir", samples)
    (directory / "donedir" / "done").unlink()
    (directory / "donedir" / "done").mkdir()
    rejects(directory / "donedir")
    write_capture(directory / "running", samples, summary=False)
    report = validator.validate(directory / "running")
    assert report["collection"]["reason"] == "no-summary"
    assert report["collection"]["complete"] is False
    assert report["integrity"]["countsReconciled"] is False
    write_capture(directory / "runningempty", [], summary=False)
    empty = validator.as_markdown(validator.validate(directory / "runningempty"))
    assert "reason=no-summary complete=False" in empty
    assert "Identity: clock=unavailable" in empty
    assert "Host MemAvailable (sampled): unavailable" in empty

    # Aborted shape stays valid but incomplete.
    write_capture(directory / "aborted", samples[:2],
                  summary_extra={"reason": "cancelled", "complete": False,
                                 "coverage": {"validBaseline": True, "incompletePersistence": False}})
    report = validator.validate(directory / "aborted")
    assert report["collection"]["reason"] == "cancelled"
    assert report["collection"]["complete"] is False

    # Oversize input is rejected while streaming (line count, bytes, meta).
    setattr(validator, "MAX_LINES", 2)
    try:
        write_capture(directory / "big", samples)
        rejects(directory / "big")
    finally:
        setattr(validator, "MAX_LINES", 100_000)
    setattr(validator, "MAX_SAMPLES_BYTES", 10)
    try:
        rejects(directory / "ok")
    finally:
        setattr(validator, "MAX_SAMPLES_BYTES", 320 << 20)
    setattr(validator, "MAX_META_BYTES", 10)
    try:
        rejects(directory / "ok")
    finally:
        setattr(validator, "MAX_META_BYTES", 1 << 20)

    # Missing inputs and symlinked inputs are invalid, not crashes.
    write_capture(directory / "nolink", samples)
    (directory / "nolink" / "samples.jsonl").unlink()
    (directory / "nolink" / "samples.jsonl").symlink_to(directory / "ok" / "samples.jsonl")
    rejects(directory / "nolink")

    # Validator behavior is identical under `python -O` (no assert dependence),
    # including malformed schema and identity cases.
    for name, expected in (("ok", 0), ("truncated", 2), ("bool", 2), ("unit", 2),
                           ("contra-reason", 2), ("neg", 2), ("nobaseline", 2),
                           ("summarynull", 2), ("produnknown", 0), ("badevents", 2),
                           ("zero", 0), ("boolcount1", 2), ("badwithunknown", 2),
                           ("inode", 2)):
        proc = subprocess.run([sys.executable, "-O", str(root / "validate-capture.py"),
                               str(directory / name)], capture_output=True, text=True)
        assert proc.returncode == expected, (name, proc.returncode, proc.stdout)

print("validate-capture regressions passed: math, host clocks, truncation, "
      "counts/sequences/timestamps, strict schema, artifact-wide identity, "
      "baseline evidence and event maps, producer unknown shapes, cell coherence, "
      "summary count types, per-guard completion, resets, "
      "partial/malformed coverage, PSI overshoot, markers, null summary, "
      "missing inputs, descriptor ownership, symlinks, oversize, and python -O parity")
