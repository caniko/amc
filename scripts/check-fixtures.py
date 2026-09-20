"""Cheap regressions: no manager, VM, host probe, or workload execution."""
import ast
import importlib.util
import json
import contextlib
import io
import re
import shlex
import signal
import sys
import time
from pathlib import Path
import tempfile

root = Path(__file__).resolve().parent.parent
tree = ast.parse((root / "nix/vm-test.py").read_text())
check = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "emitted_properties")
namespace: dict = {"json": json}
exec(compile(ast.Module(body=[check], type_ignores=[]), "vm-property-regression", "exec"), namespace)
check_properties = namespace["emitted_properties"]
check_properties({"systemdProperties": ["Restart=no"], "migration": "MemoryOOMGroup was removed"})
try:
    check_properties({"systemdProperties": ["Restart=no", "MemoryOOMGroup=yes"]})
except AssertionError:
    pass
else:
    raise AssertionError("emitted unsupported property was accepted")


class Machine:
    def succeed(self, command):
        assert command in {f"cat /tmp/work-{arm}.json" for arm in "abc"}
        return '{"iterationsCompleted": 5, "bytesTouched": 20971520}'


# Execute the actual VM report-reading statements against mixed stdout.
found = 0
for node in ast.walk(tree):
    if isinstance(node, ast.Assign) and "cat /tmp/work-" in ast.unparse(node):
        scope = {"machine": Machine(), "json": json, "output": '{"unrelated": true}\n{"iterationsCompleted": 999}'}
        exec(compile(ast.Module(body=[node], type_ignores=[]), "vm-work-report-regression", "exec"), scope)
        assert scope["report"]["iterationsCompleted"] == 5
        found += 1
assert found == 3

spec = importlib.util.spec_from_file_location("watch", root / "scripts/watch-cgroup.py")
assert spec is not None and spec.loader is not None
watch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(watch)
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory)
    watch.observe(path / "missing-cgroup", path / "report", 1)
    report = json.loads((path / "report/summary.json").read_text())
    assert report["maxObservedSwap"] is None
    assert report["eventDeltas"] is None
    assert report["reason"] == "observer-not-ready"
    assert not (path / "report/ready").exists()
# Rust core (crates/amc-telemetry) is authoritative; the Python parser must
# match it byte-for-byte until the deprecated module is deleted.
for name, bad in [("memory.events", "oom -1"), ("memory.swap.current", "SENSITIVE"), ("memory.pressure", "some avg10=NaN"),
                  ("memory.events", "OOM 1"), ("memory.events", "oom 1\noom 2"),
                  ("memory.events", "oom 18446744073709551616"), ("memory.events", ""),
                  ("memory.oom.group", "2"), ("memory.current", "max"),
                  ("memory.pressure", "some avg10=inf avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0")]:
    try:
        watch.parse(name, bad)
    except ValueError:
        pass
    else:
        raise AssertionError("malformed telemetry accepted")
assert watch.parse("memory.max", "max") == "max"
assert watch.parse("memory.oom.group", "1") == 1
assert watch.parse("memory.current", "0") == 0

# Reuse the actual driver finalizer and outer finally, not a parallel harness.
functions: list[ast.stmt] = [
    node for node in tree.body
    if (isinstance(node, ast.ClassDef) and node.name == "BudgetExhausted")
    or (isinstance(node, ast.FunctionDef) and node.name in ("bounded_guest", "finalize_run", "emit"))
]
outer = next(node for node in tree.body if isinstance(node, ast.Try))


class FailedGuest:
    def __init__(self, failure):
        self.failure = failure
        self.calls = []

    def is_up(self):
        return self.failure != "unavailable"

    def execute(self, command, timeout):
        assert 0 < timeout <= 5
        self.calls.append(command)
        if self.failure == "hang":
            time.sleep(60)
        if self.failure == "transport-once":
            self.failure = "dead"
            raise BrokenPipeError("SENSITIVE_TRANSPORT")
        if self.failure == "dead":
            raise AssertionError("request sent after transport failure")
        if command.startswith("python3"):
            if "/tmp/identity" in command:
                return 0, "app-amc-proof-abcd@2.service\n"
            if "/tmp/missing.json" in command:
                return 1, "SENSITIVE_MISSING_ERROR"
            return 0, '{"iterationsCompleted": 5}'
        if " amc inspect " in command:
            return 0, '{"properties": {"ControlGroup": ""}}'
        if " stop " in command and self.failure == "cleanup-error":
            raise RuntimeError("SENSITIVE_CLEANUP_ERROR")
        if " show " in command:
            return 0, "LoadState=not-found\nActiveState=inactive\nMainPID=0\nControlPID=0\n"
        return 0, ""


for failure in ("none", "cleanup-error", "unavailable"):
    with tempfile.TemporaryDirectory() as directory:
        original = AssertionError("original mechanism failure")
        scope = {"json": json, "re": re, "shlex": shlex, "signal": signal,
                 "time": time, "sys": sys, "ARTIFACTS": ("/tmp/work-a.json", "/tmp/missing.json"),
                 "machine": FailedGuest(failure), "prefix": "fixture-user",
                 "owned": {"app-amc-proof-abcd@1.service"}, "identity_files": {"/tmp/identity"},
                 "observed_cgroups": {"app-amc-proof-abcd@1.service": "/fixture/app-amc-proof-abcd@1.service"},
                 "artifact_dir": Path(directory), "recorded_artifacts": [], "original": original}
        exec(compile(ast.Module(body=functions, type_ignores=[]), "driver-finalizer", "exec"), scope)
        case = ast.Try(body=[ast.Raise(exc=ast.Name(id="original", ctx=ast.Load()))],
                       handlers=[], orelse=[], finalbody=outer.finalbody)
        module = ast.fix_missing_locations(ast.Module(body=[case], type_ignores=[]))
        output = io.StringIO()
        try:
            with contextlib.redirect_stdout(output):
                exec(compile(module, "driver-failure-regression", "exec"), scope)
        except AssertionError as error:
            assert error is original, "secondary failure replaced the mechanism failure"
        else:
            raise AssertionError("original failure was swallowed")
        report = json.loads((Path(directory) / "finalization.json").read_text())
        assert report["primaryFailure"] == "AssertionError"
        assert "SENSITIVE" not in output.getvalue()
        assert report["cleanup"], "cleanup outcomes were never recorded"
        if failure == "none":
            assert (Path(directory) / "work-a.json").exists(), "failure path did not export available evidence"
            assert len(report["cleanup"]) == 2, "failed submission identity was not recovered"
            assert all(item["status"] == "CONFIRMED_EMPTY" for item in report["cleanup"])
            assert any(item["status"] == "UNKNOWN" for item in report["exports"]), "missing artifact fabricated"
        else:
            assert all(item["status"] == "UNKNOWN" for item in report["cleanup"])
        if failure == "unavailable":
            assert not scope["machine"].calls, "unavailable guest was reconnected during cleanup"
        for command in scope["machine"].calls:
            if "systemctl" in command:
                assert "app-amc-proof-abcd@" in command and "*" not in command

started = time.monotonic()
try:
    scope["bounded_guest"](FailedGuest("hang"), "fixture command", started + 0.05)
except TimeoutError:
    pass
else:
    raise AssertionError("host-side control wait was not bounded")
assert time.monotonic() - started < 1

def driver_scope(guest, directory):
    return {
        "json": json, "re": re, "shlex": shlex, "signal": signal, "time": time, "sys": sys,
        "ARTIFACTS": ("/tmp/work-a.json",), "machine": guest, "prefix": "fixture-user",
        "owned": {"app-amc-proof-abcd@1.service"}, "identity_files": {"/tmp/identity"},
        "observed_cgroups": {"app-amc-proof-abcd@1.service": "/fixture/app-amc-proof-abcd@1.service"},
        "artifact_dir": Path(directory), "recorded_artifacts": [],
    }

with tempfile.TemporaryDirectory() as directory:
    guest = FailedGuest("none")
    budget_scope = driver_scope(guest, directory)
    exec(compile(ast.Module(body=functions, type_ignores=[]), "driver-budget", "exec"), budget_scope)
    report = budget_scope["finalize_run"](
        guest, "fixture-user", {"app-amc-proof-abcd@1.service"}, {"/tmp/identity"},
        {"app-amc-proof-abcd@1.service": "/fixture/app-amc-proof-abcd@1.service"},
        Path(directory), None, (), 1)
    assert all(item["status"] == "CONFIRMED_EMPTY" for item in report["cleanup"]), report
    assert any(item["status"] == "UNKNOWN" for item in report["exports"]), report
    assert any(" stop " in command for command in guest.calls), guest.calls

with tempfile.TemporaryDirectory() as directory:
    guest = FailedGuest("none")
    time_scope = driver_scope(guest, directory)
    exec(compile(ast.Module(body=functions, type_ignores=[]), "driver-time-budget", "exec"), time_scope)
    report = time_scope["finalize_run"](
        guest, "fixture-user", {"app-amc-proof-abcd@1.service"}, {"/tmp/identity"},
        {"app-amc-proof-abcd@1.service": "/fixture/app-amc-proof-abcd@1.service"},
        Path(directory), None, (), 16 * 1024 * 1024, 0)
    assert all(item["status"] == "CONFIRMED_EMPTY" for item in report["cleanup"]), report
    assert all(item["status"] == "UNKNOWN" for item in report["exports"]), report
    assert any(" stop " in command for command in guest.calls), guest.calls

with tempfile.TemporaryDirectory() as directory:
    guest = FailedGuest("transport-once")
    pipe_scope = driver_scope(guest, directory)
    exec(compile(ast.Module(body=functions, type_ignores=[]), "driver-transport", "exec"), pipe_scope)
    report = pipe_scope["finalize_run"](
        guest, "fixture-user", {"app-amc-proof-abcd@1.service"}, {"/tmp/identity"},
        {"app-amc-proof-abcd@1.service": "/fixture/app-amc-proof-abcd@1.service"},
        Path(directory), None)
    assert all(item["status"] == "UNKNOWN" for item in report["cleanup"]), report
    assert len(guest.calls) == 1, guest.calls

emit_scope: dict = {"Exception": Exception}
exec(compile(ast.Module(body=[node for node in functions if getattr(node, "name", None) == "emit"], type_ignores=[]), "emit", "exec"), emit_scope)
def boom(*args, **kwargs):
    raise BrokenPipeError("SENSITIVE_PIPE")
emit_scope["print"] = boom
emit_scope["emit"]("should not raise")
print("fixture regressions passed: property scope, explicit reports, unknown telemetry, failure export/cleanup, host deadline, export-budget cleanup, time-budget cleanup, transport fail-closed, and emit swallow")
