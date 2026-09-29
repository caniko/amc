"""Native accounting contract checks; never invokes the real user manager."""
import argparse
import importlib.util
import json
import signal
import tempfile
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("capture_accounted", Path(__file__).with_name("capture-accounted.py"))
assert spec is not None and spec.loader is not None
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

properties = {
    "InvocationID": "a" * 32, "ActiveState": "active", "SubState": "exited",
    "ExecMainCode": "1", "ExecMainStatus": "0", "Result": "success",
    "ExecMainStartTimestampMonotonic": "1000000", "ExecMainExitTimestampMonotonic": "4000000",
    "CPUUsageNSec": "12000000", "MemoryPeak": "4194304", "IOReadBytes": "0",
    "IOWriteBytes": "18446744073709551615", "IOReadOperations": "[not set]",
    "MemoryAccounting": "yes", "IOAccounting": "yes",
}
result = module.accounting(properties)
assert result["metrics"]["CPUUsageNSec"] == {"value": 12000000, "unknown": None, "unit": "ns"}
assert result["metrics"]["IOReadBytes"]["value"] == 0
assert result["metrics"]["IOWriteBytes"]["value"] is None
assert result["metrics"]["IOWriteOperations"]["unknown"] == "unavailable"
assert result["durationUs"] == 3000000
assert module.accounting({})["durationUs"] is None
assert module.accounting({**properties, "ExecMainExitTimestampMonotonic": "0"})["durationUs"] is None

with tempfile.TemporaryDirectory() as temporary:
    base = Path(temporary)
    binary = base / "fake-amc"
    binary.write_text("not executed")
    binary.chmod(0o700)
    args = argparse.Namespace(unit="target.scope", system=True, seconds=1, interval_ms=1000,
                              amc=binary, output=base / "session")
    calls = []

    def command(argv):
        calls.append(argv)
        if argv[0] == "systemd-run":
            assert "--user" in argv and "--collect" not in argv
            assert "--property=RemainAfterExit=yes" in argv
            assert "--system" in argv[argv.index("watch"):]
        elif "show" in argv:
            return "\n".join(f"{key}={value}" for key, value in {**properties, "Id": argv[-1]}.items())
        elif "stop" in argv:
            saved = json.loads((args.output / "accounting.json").read_text())
            assert saved["invocationId"] == properties["InvocationID"]
            assert saved["metrics"]["CPUUsageNSec"]["value"] == 12000000
            assert argv[-1].startswith("amc-observe-")
            assert argv[-1] != args.unit
        return ""

    with patch.object(module, "command", side_effect=command):
        assert module.capture(args) == 0
    assert (args.output.stat().st_mode & 0o777) == 0o700
    assert ((args.output / "accounting.json").stat().st_mode & 0o777) == 0o600
    assert json.loads((args.output / "cleanup.json").read_text())["observerStopped"] is True
    assert (args.output / "amc").read_bytes() == binary.read_bytes()

    # Failed export must leave the finished unit available for manual recovery.
    args.output = base / "failed-export"
    original_write = module.write_json

    def fail_export(path, value):
        if path.name == "accounting.json":
            raise OSError("disk full")
        original_write(path, value)

    calls.clear()
    with patch.object(module, "command", side_effect=command), patch.object(module, "write_json", side_effect=fail_export):
        try:
            module.capture(args)
        except OSError:
            pass
        else:
            raise AssertionError("export failure was ignored")
    assert not any("stop" in call for call in calls)

    # Cancellation forwards only to our observer's main process, then exports
    # its retained accounting before stopping the unit. No actual signal is sent.
    args.output = base / "cancelled"
    calls.clear()
    shown = []

    def cancel_command(argv):
        if "show" in argv and not shown:
            shown.append(True)
            handler = signal.getsignal(signal.SIGINT)
            assert callable(handler)
            handler(signal.SIGINT, None)
            calls.append(argv)
            return "\n".join(f"{key}={value}" for key, value in
                             {**properties, "Id": argv[-1], "SubState": "running"}.items())
        if "kill" in argv:
            assert "--kill-whom=main" in argv and "--signal=SIGINT" in argv
            assert argv[-1].startswith("amc-observe-") and argv[-1] != args.unit
        return command(argv)

    with patch.object(module, "command", side_effect=cancel_command), patch.object(module.time, "sleep"):
        assert module.capture(args) == 130
    assert json.loads((args.output / "accounting.json").read_text())["cancellationRequested"] is True
    assert any("kill" in call for call in calls)

    # An observed replacement never receives a kill or stop from this run.
    args.output = base / "replacement"
    calls.clear()
    shown.clear()

    def replaced_command(argv):
        if "show" in argv:
            result = {**properties, "Id": argv[-1], "SubState": "running"}
            if shown:
                result.update(InvocationID="b" * 32, SubState="exited")
            shown.append(True)
            return "\n".join(f"{key}={value}" for key, value in result.items())
        return command(argv)

    with patch.object(module, "command", side_effect=replaced_command), patch.object(module.time, "sleep"):
        try:
            module.capture(args)
        except RuntimeError as error:
            assert "invocation changed" in str(error)
        else:
            raise AssertionError("replacement was controlled")
    assert not any("stop" in call or "kill" in call for call in calls)

    # A successful main exit does not override a failed unit result.
    args.output = base / "failed-unit"
    with patch.dict(properties, Result="oom-kill"), patch.object(module, "command", side_effect=command):
        assert module.capture(args) == 2
    assert json.loads((args.output / "accounting.json").read_text())["manager"]["Result"] == "oom-kill"

print("observer accounting checks passed: units, unknowns, private export, export-before-stop, failed-export retention, cancellation, replacement")
