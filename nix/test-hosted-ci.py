import ast
import contextlib
import errno
import hashlib
import importlib.util
import io
import json
import runpy
import tempfile
import types
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET
from contextlib import nullcontext
from pathlib import Path
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "hosted_ci", Path(__file__).with_name("hosted-ci.py")
)
hosted = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hosted)

guard_spec = importlib.util.spec_from_file_location(
    "guard_proof", Path(__file__).with_name("page-return-guard-proof.py")
)
guard = importlib.util.module_from_spec(guard_spec)
guard_spec.loader.exec_module(guard)


class QuietWindowTests(unittest.TestCase):
    def test_actual_guest_barrier_uses_request_age_and_requires_the_first_charge(self):
        tree = ast.parse((hosted.ROOT / "nix/host-admission-vm-test.py").read_text())
        call = next(node for node in ast.walk(tree)
                    if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                    and node.func.attr == "succeed" and node.args
                    and "agedOrdinary" in ast.unparse(node.args[0]))
        guest = next(node.value for node in ast.walk(call.args[0])
                     if isinstance(node, ast.Constant) and isinstance(node.value, str)
                     and "agedOrdinary" in node.value)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "quiet-window-guest.py"
            path.write_text(guest)
            for charged, wall_ns in [(True, 1_792_102_000_000_000_000), (True, 0), (False, 0)]:
                with self.subTest(charged=charged, wall_ns=wall_ns):
                    elapsed = [0.0]
                    state = {"burst_committed_bytes": 33554432 if charged else 0,
                             "reservations": [{"granted": False, "memory_bytes": 100663296,
                                               "requested_ms": 1000}]}

                    def advance(seconds, elapsed=elapsed):
                        elapsed[0] += seconds

                    def boot_ns(clock, elapsed=elapsed):
                        import time
                        self.assertEqual(clock, time.CLOCK_BOOTTIME)
                        return int((1.5 + elapsed[0]) * 1_000_000_000)

                    with (
                        mock.patch("pathlib.Path.read_text", return_value='{"aging_ms":1000}'),
                        mock.patch("subprocess.check_output", return_value=json.dumps(state)) as probe,
                        mock.patch("time.monotonic", side_effect=lambda elapsed=elapsed: elapsed[0]),
                        mock.patch("time.time_ns", return_value=wall_ns),
                        mock.patch("time.clock_gettime_ns", side_effect=boot_ns),
                        mock.patch("time.sleep", side_effect=advance),
                        mock.patch("builtins.print") as report,
                    ):
                        if charged:
                            runpy.run_path(str(path))
                            self.assertGreaterEqual(elapsed[0], .5)
                            self.assertLess(elapsed[0], .55)
                            self.assertGreater(probe.call_count, 1)
                            receipt = json.loads(report.call_args.args[0])
                            self.assertEqual(receipt["agedOrdinary"], state["reservations"][0])
                            self.assertEqual(receipt["burstCommittedBytes"], 33554432)
                        else:
                            with self.assertRaises(AssertionError):
                                runpy.run_path(str(path))
                            self.assertEqual(probe.call_count, 1)
                            report.assert_not_called()


class GuardNonresidentSetupTests(unittest.TestCase):
    def test_target_stays_frozen_through_reclaim_and_destination_migration(self):
        writes = {}
        original = guard.ROOT / "original"
        backed = guard.ROOT / "backed"

        def write(path, value):
            if path.name == "memory.reclaim":
                self.assertEqual(writes.get(original / "cgroup.freeze"), "1",
                                 "the executing target can refault reclaimed pages")
            if path == backed / "cgroup.procs":
                self.assertEqual(writes.get(backed / "cgroup.freeze"), "1",
                                 "migration must not thaw the target")
            writes[path] = value

        def read(path):
            if path.name == "cgroup.events":
                self.assertEqual(writes.get(path.parent / "cgroup.freeze"), "1")
                return "populated 1\nfrozen 1\n"
            if path.name == "memory.stat":
                return "swapcached 0\n"
            if path.name == "memory.swap.current":
                return str(4 * guard.MIB)
            self.assertEqual(str(path), "/etc/amc-test-host-policy.json")
            return '{"swap_recovery":{"page_cgroups":[]}}'

        with (
            mock.patch.object(guard, "cleanup_scope", return_value=nullcontext()),
            mock.patch.object(guard.Path, "mkdir"),
            mock.patch.object(guard.Path, "write_text", autospec=True, side_effect=write),
            mock.patch.object(guard.Path, "read_text", autospec=True, side_effect=read),
            mock.patch.object(guard.os, "pipe", return_value=(100, 101)),
            mock.patch.object(guard.os, "fork", return_value=123),
            mock.patch.object(guard.os, "close"),
            mock.patch.object(guard.os, "read", return_value=b"ready"),
            mock.patch.object(guard.subprocess, "run", side_effect=RuntimeError("setup complete")),
            self.assertRaisesRegex(RuntimeError, "setup complete"),
        ):
            guard.prove_path("cache")
        self.assertIn(original / "memory.reclaim", writes)
        self.assertEqual(writes[backed / "cgroup.procs"], "123")


class ChargeOwnerSetupTests(unittest.TestCase):
    def test_target_is_frozen_before_reclaim_and_during_migration(self):
        tree = ast.parse((hosted.ROOT / "nix/charge-owner-proof.py").read_text())
        proof = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "proof")
        setup = next(node for node in proof.body if isinstance(node, ast.Try)).body
        end = next(index for index, node in enumerate(setup)
                   if isinstance(node, ast.Assign) and ast.unparse(node.targets[0]) == "policy")
        root = Path("/sys/fs/cgroup/amc-charge-proof")
        writes = {}
        frozen = set()

        def write(path, value):
            if path.name == "memory.reclaim":
                self.assertIn(root / "destination", frozen,
                              "the executing target can refault original-owner pages")
            if path == root / "moved" / "cgroup.procs":
                self.assertIn(root / "destination", frozen)
                self.assertEqual(writes.get(root / "moved" / "cgroup.freeze"), "1")
            writes[path] = value

        def read(path):
            if path.name == "cgroup.events":
                self.assertEqual(writes.get(path.parent / "cgroup.freeze"), "1")
                frozen.add(path.parent)
                return "populated 1\nfrozen 1\n"
            self.assertEqual(str(path), "/proc/123/cgroup")
            return "0::/amc-charge-proof/moved\n"

        namespace = {
            "ROOT": root, "MIB": 1048576, "Path": Path, "errno": errno,
            "pid": 123, "read": 100,
            "os": types.SimpleNamespace(read=lambda *_: b"ready"),
            "wait": lambda predicate: self.assertTrue(predicate()),
            "observe": lambda group: {"swap": 32 * 1048576 if group.name in ("a", "b") else 0,
                                      "cached": 0},
        }
        with (
            mock.patch.object(Path, "write_text", autospec=True, side_effect=write),
            mock.patch.object(Path, "read_text", autospec=True, side_effect=read),
        ):
            exec(compile(ast.Module(body=setup[:end], type_ignores=[]), "charge-owner-setup", "exec"), namespace)
        for name in ("a", "b"):
            self.assertIn(root / name / "memory.reclaim", writes)
        self.assertEqual(writes[root / "moved" / "cgroup.procs"], "123")
        self.assertIn(root / "moved", frozen)


class GuardCleanupTests(unittest.TestCase):
    def setUp(self):
        self.now = 0
        self.real_diagnostics = guard.cleanup_diagnostics
        self.reader = mock.patch.object(
            guard.Path, "read_text", autospec=True,
            side_effect=lambda path: "Filename Type Size Used Priority\n" if str(path) == "/proc/swaps" else "0\n",
        )
        self.reader.start()
        self.addCleanup(self.reader.stop)
        clock = mock.patch.object(guard.time, "monotonic", side_effect=lambda: self.now)
        clock.start()
        self.addCleanup(clock.stop)
        sleeper = mock.patch.object(guard.time, "sleep", side_effect=self.advance)
        self.sleep = sleeper.start()
        self.addCleanup(sleeper.stop)
        diagnostics = mock.patch.object(guard, "cleanup_diagnostics", return_value='{"phase":"fixture-reset","swaps":"inactive"}')
        diagnostics.start()
        self.addCleanup(diagnostics.stop)

    def advance(self, seconds):
        self.now += seconds

    def test_reset_retries_only_transient_busy_and_closes_each_attempt(self):
        with mock.patch.object(guard.Path, "open") as opened:
            handle = opened.return_value.__enter__.return_value
            handle.write.side_effect = [OSError(errno.EBUSY, "busy"), 1]
            guard.reset_zram(timeout=.1)
            self.assertEqual(handle.write.call_count, 2)
            self.assertEqual(opened.return_value.__exit__.call_count, 2)
            self.assertEqual(self.now, .05)

    def test_persistent_busy_exhausts_the_bound_with_diagnostics(self):
        busy = OSError(errno.EBUSY, "held open")
        with mock.patch.object(guard.Path, "open") as opened:
            handle = opened.return_value.__enter__.return_value
            handle.write.side_effect = busy
            with self.assertRaises(OSError) as caught:
                guard.reset_zram(timeout=.1)
            self.assertIs(caught.exception, busy)
            self.assertEqual(handle.write.call_count, 3)
            self.assertEqual(self.now, .1)
            self.assertIn("exhausted", " ".join(busy.__notes__))
            self.assertIn("inactive", " ".join(busy.__notes__))

    def test_unrelated_reset_error_propagates_without_retry(self):
        error = OSError(errno.EIO, "broken reset")
        with mock.patch.object(guard.Path, "open") as opened:
            handle = opened.return_value.__enter__.return_value
            handle.write.side_effect = error
            with self.assertRaises(OSError) as caught:
                guard.reset_zram()
            self.assertIs(caught.exception, error)
            self.assertEqual(handle.write.call_count, 1)
            self.sleep.assert_not_called()

    def test_reset_refuses_active_swap_before_opening_reset(self):
        self.reader.stop()
        with (
            mock.patch.object(guard.Path, "read_text", return_value="Filename Type Size Used Priority\n/dev/zram0 partition 100 1 100\n"),
            mock.patch.object(guard.Path, "open") as opened,
        ):
            with self.assertRaisesRegex(AssertionError, "still active swap"):
                guard.reset_zram()
            opened.assert_not_called()

    def test_cleanup_preserves_primary_and_attempts_independent_steps(self):
        completed = []
        primary = AssertionError("migration proof failed")
        cleanup_error = OSError(errno.EBUSY, "reset failed")

        def fail():
            raise cleanup_error

        steps = {"remaining": lambda: completed.append("remaining"), "reset": fail}
        with (
            self.assertLogs(level="ERROR"),
            self.assertRaises(guard.builtins.BaseExceptionGroup) as caught,
            guard.cleanup_scope("direct", steps),
        ):
            raise primary
        self.assertEqual(list(caught.exception.exceptions), [primary, cleanup_error])
        self.assertEqual(completed, ["remaining"])
        self.assertIn("primary failure", " ".join(primary.__notes__))
        self.assertIn("cleanup step=reset", " ".join(cleanup_error.__notes__))

    def test_successful_cleanup_does_not_replace_primary_failure(self):
        primary = AssertionError("original guard failure")
        with self.assertRaises(AssertionError) as caught, guard.cleanup_scope("cache", {}):
            raise primary
        self.assertIs(caught.exception, primary)

    def test_diagnostic_failure_is_reported_without_replacing_cleanup_error(self):
        # Exercise the real diagnostic collector with a failing manager query.
        error = OSError(errno.EIO, "reset failed")
        with (
            self.assertLogs(level="ERROR"),
            mock.patch.object(guard, "cleanup_diagnostics", self.real_diagnostics),
            mock.patch.object(guard.subprocess, "check_output", side_effect=RuntimeError("manager unavailable")),
            mock.patch.object(guard.Path, "open") as opened,
        ):
            opened.return_value.__enter__.return_value.write.side_effect = error
            with self.assertRaises(OSError) as caught:
                guard.reset_zram()
        self.assertIs(caught.exception, error)
        self.assertIn("zram-reset-error", " ".join(error.__notes__))
        self.assertIn("manager unavailable", " ".join(error.__notes__))


class NativeEvidenceTests(unittest.TestCase):
    def test_burst_responsiveness_uses_host_status_response_schema(self):
        source = ast.parse((hosted.ROOT / "nix/host-admission-vm-test.py").read_text())
        probes = [node.value for node in ast.walk(source)
                  if isinstance(node, ast.Constant) and isinstance(node.value, str)
                  and "elapsedSeconds" in node.value and "subprocess.run" in node.value]
        self.assertEqual(len(probes), 1)
        probe = probes[0].removeprefix("python3 - <<'PY'\n").removesuffix("\nPY")
        response = {"version": 1, "error": None, "budget_bytes": 160 * 1048576, "reservations": []}
        with patch("subprocess.run", return_value=types.SimpleNamespace(stdout=json.dumps(response))) as run:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                exec(probe, {})
            self.assertEqual(json.loads(output.getvalue())["responses"], 3)
            self.assertEqual(run.call_count, 3)
            self.assertTrue(all(call.kwargs["timeout"] == 1 for call in run.call_args_list))
        for invalid in [{**response, "version": 2}, {**response, "error": "denied"},
                        {**response, "budget_bytes": 0},
                        {**response, "reservations": None}, {}]:
            with self.subTest(response=invalid), patch("subprocess.run", return_value=types.SimpleNamespace(stdout=json.dumps(invalid))):
                with self.assertRaises((AssertionError, KeyError)):
                    exec(probe, {})

    def test_protected_pool_matrix_requires_every_native_case_and_rejects_failures(self):
        installable = ".#nixosTests.x86_64-linux.protected-pool"
        self.assertIn(installable, hosted.ALLOWED)
        policy = hosted.tomllib.loads((hosted.ROOT / "simit.toml").read_text())
        self.assertIn(installable, policy["ci"]["nix_builds"])
        tree = ast.parse((hosted.ROOT / "nix/protected-pool-vm-test.py").read_text())
        names = {
            node.args[0].value for node in ast.walk(tree)
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
            and node.func.id == "test_section"
        }
        self.assertEqual(len(names), 5)
        self.assertEqual(names, hosted.PROTECTED_POOL_CASES)
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "junit.xml"
            report = ET.Element("testsuite")
            for name in sorted(names):
                ET.SubElement(report, "testcase", name=name)
            ET.ElementTree(report).write(path)
            hosted.verify_native_report(path, hosted.PROTECTED_POOL_CASES)
            for tag in ("failure", "error", "skipped"):
                with self.subTest(tag=tag):
                    failure = ET.SubElement(report[0], tag)
                    ET.ElementTree(report).write(path)
                    with self.assertRaises(RuntimeError):
                        hosted.verify_native_report(path, hosted.PROTECTED_POOL_CASES)
                    report[0].remove(failure)
            report.remove(report[0])
            ET.ElementTree(report).write(path)
            with self.assertRaises(RuntimeError):
                hosted.verify_native_report(path, hosted.PROTECTED_POOL_CASES)

    def test_supervision_settlement_uses_one_coherent_status_snapshot(self):
        tree = ast.parse((hosted.ROOT / "nix/supervision-recovery-failures.py").read_text())
        settled = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "settled")
        source = ast.unparse(ast.Module(body=[settled], type_ignores=[]))
        snapshots = iter([
            {"recovery": {"active": None, "attempts": []}},
            {"recovery": {"active": {"phase": "cooling"}, "attempts": [1]}},
            {"recovery": {"active": None, "attempts": [1]}},
        ])
        current = None
        observations = []

        def read():
            nonlocal current
            current = next(snapshots, current)
            return current

        def wait_for(predicate, seconds):
            self.assertEqual(seconds, 30)
            for _ in range(3):
                result = predicate()
                observations.append(result)
                if result:
                    return
            raise AssertionError("did not settle")

        machine = mock.Mock()

        def wait_for_unit(unit):
            self.assertEqual(unit, "amc-failure@budget-a.service")
            self.assertIsNone(current["recovery"]["active"], "fixture accepted a mixed startup/cooling snapshot")
            self.assertEqual(len(current["recovery"]["attempts"]), 1)

        machine.wait_for_unit.side_effect = wait_for_unit
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "settlement.py"
            path.write_text(source)
            namespace = runpy.run_path(str(path), init_globals={
                "wait_for": wait_for, "machine": machine,
                "unit": lambda name: f"amc-failure@{name}.service",
            })
            receipt = namespace["settled"](read, "budget-a", 1)
        self.assertEqual(observations, [False, False, True])
        self.assertEqual(receipt, current)
        machine.wait_for_unit.assert_called_once()

    def test_completion_teardown_requires_an_exited_client_and_strict_stops(self):
        tree = ast.parse((hosted.ROOT / "nix/completion-vm-test.py").read_text())
        section = tree.body[0]
        stop = next(
            index for index, node in enumerate(section.body)
            if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "stop_scope"
        )
        # Execute the real teardown statements, ending before the receipt.
        cleanup = ast.unparse(
            ast.Module(body=section.body[stop + 1:-1], type_ignores=[])
        )

        def run_cleanup(machine):
            with tempfile.TemporaryDirectory() as temp:
                path = Path(temp) / "completion-teardown.py"
                path.write_text(cleanup)
                runpy.run_path(str(path), init_globals={"machine": machine})

        class Machine:
            def __init__(self, state, stop_error=False):
                self.state = state
                self.stop_error = stop_error
                self.stopped = False

            def succeed(self, command):
                if command == "systemctl show completion-game-client.service --property=ActiveState,MainPID,ControlPID":
                    return self.state
                if command != "systemctl stop completion-worker completion-host":
                    raise AssertionError(command)
                if self.stop_error:
                    raise RuntimeError("strict stop failed")
                self.stopped = True
                return ""

        # systemctl show reports these properties even for an unloaded unit.
        inactive = "ActiveState=inactive\nMainPID=0\nControlPID=0\n"
        machine = Machine(inactive)
        run_cleanup(machine)
        self.assertTrue(machine.stopped)
        for state in [
            "ActiveState=active\nMainPID=123\nControlPID=0\n",
            "ActiveState=deactivating\nMainPID=123\nControlPID=0\n",
            "ActiveState=failed\nMainPID=0\nControlPID=0\n",
            "ActiveState=inactive\nMainPID=0\nControlPID=123\n",
        ]:
            with self.subTest(state=state):
                machine = Machine(state)
                with self.assertRaises(AssertionError):
                    run_cleanup(machine)
                self.assertFalse(machine.stopped)
        with self.assertRaisesRegex(RuntimeError, "strict stop failed"):
            run_cleanup(Machine(inactive, stop_error=True))

    def test_generator_identity_tracks_the_canonical_pinned_invocation(self):
        source = (hosted.ROOT / "simit.toml").read_text()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "simit.toml").write_text(source)
            self.assertEqual(hosted.generator_revision(root), "bd04eedca7cdb76042fb3d4eeda9658721988fa2")
            replacement = "1234567890abcdef1234567890abcdef12345678"
            (root / "simit.toml").write_text(source.replace("bd04eedca7cdb76042fb3d4eeda9658721988fa2", replacement))
            self.assertEqual(hosted.generator_revision(root), replacement)
            for invalid in ["main", "1234567", "", replacement.upper()]:
                (root / "simit.toml").write_text(source.replace("bd04eedca7cdb76042fb3d4eeda9658721988fa2", invalid))
                with self.subTest(invalid=invalid), self.assertRaises(RuntimeError):
                    hosted.generator_revision(root)

    def test_foreground_receipt_rejects_partial_return_unbacked_transfer_and_new_work(
        self,
    ):
        receipt = {
            "kernel": "6.18.48",
            "pageReturnGuard": self.guard_evidence(),
            "outsideRunnerPreparationDenied": True,
            "userCreatedPreparationHelperDenied": True,
            "poolRetirement": {"removedDomainReleased": True, "liveOwnerCleared": True,
                "descendantRetainedBytes": 96 * 1048576, "finalBytes": 0},
            "abandonedPreparation": {
                "sigkillWhileBrokerOffline": True,
                "waitMilliseconds": 3600000,
                "restartClearedBarrier": True,
                "existingWorkRetainedBytes": 32 * 1048576,
                "payloadDidNotExecute": True,
            },
            "chargeOwner": {
                "schemaVersion": 2,
                "kernel": "fixture",
                "frozenMigration": True,
                "originalOwnerDenied": True,
                "offlineFallbackDenied": True,
                "helperFallbackDenied": True,
                "helperDenial": {
                    "granted": False,
                    "waiting": "ancestor_headroom",
                    "requestedBytes": 8 * 1048576,
                    "observationsBefore": {"helper": {"memory": 124 * 1048576}},
                    "helperEvents": {"oom": "0", "oom_kill": "0"},
                    "inventoryStatusReplies": 2,
                    "inventoryMaxStatusLatencyMs": 100,
                },
                "fallbackCharge": {
                    "granted": True,
                    "resident_bytes": 2 * 1048576,
                    "uniqueResidentBytes": 2 * 1048576,
                    "chargedBytes": {"helper": 2 * 1048576},
                    "chargeInodes": {"helper": 123},
                    "helperEvents": {"oom": "0", "oom_kill": "0"},
                    "inventoryStatusReplies": 2,
                    "inventoryMaxStatusLatencyMs": 100,
                },
                "onlineCharge": {
                    "granted": True,
                    "resident_bytes": 2 * 1048576,
                    "uniqueResidentBytes": 2 * 1048576,
                    "chargedBytes": {"original": 2 * 1048576},
                    "chargeInodes": {"original": 456},
                    "helperEvents": {"oom": "0", "oom_kill": "0"},
                    "inventoryStatusReplies": 2,
                    "inventoryMaxStatusLatencyMs": 100,
                },
                "batchBytes": 2 * 1048576,
                "fallbackResidentBytes": 2 * 1048576,
                "onlineResidentBytes": 2 * 1048576,
                "fallbackBefore": {"memory": 0},
                "fallbackAfter": {"memory": 2 * 1048576},
                "onlineBefore": {"memory": 0},
                "onlineAfter": {"memory": 2 * 1048576},
                "before": {
                    "destination": {"swap": 0},
                    "a": {"swap": 32 * 1048576, "cached": 0},
                    "b": {"swap": 32 * 1048576, "cached": 0},
                },
                "oom": 0,
                "oomKill": 0,
            },
            "nativeNamespaceCompletion": self.namespace_evidence(),
            "namespaceRunnerBacking": {
                "aggregateBytesPerUser": 64 * 1048576,
                "reservedBytes": 128 * 1048576,
                "concurrentRunners": 2,
                "innerCommittedBytes": 64 * 1048576,
                "restartPreserved": True,
                "changedCeilingDenied": True,
                "removedUserRetained": True,
                "excessReserveDenied": True,
                "retiredUserReleasedAfterCleanup": True,
            },
            "burstManagerResponsiveness": {"responses": 3, "elapsedSeconds": 0.2},
            "preparationHelperBacking": {
                "concurrentHelpers": 2, "aggregateBytesPerUser": 64 * 1048576,
                "reservedBytes": 128 * 1048576, "restartPreserved": True,
                "payloadsEnteredAfterDrain": True,
                "existingWorkCompleted": True,
            },
            "pageReturn": {
                "schemaVersion": 2,
                "beforeSwapBytes": 64 * 1048576,
                "afterSwapBytes": 64 * 1048576,
                "beforeCachedBytes": 0,
                "afterCachedBytes": 64 * 1048576,
                "beforeReturnBytes": 64 * 1048576,
                "afterReturnBytes": 0,
                "mappingBytes": 64 * 1048576,
                "beforePresentBytes": 0,
                "beforeSwappedBytes": 64 * 1048576,
                "afterPresentBytes": 64 * 1048576,
                "afterSwappedBytes": 0,
                "dataIntact": True,
                "waitExitCode": 75,
                "interruptedBatchBytes": 2 * 1048576,
                "unreadBatchSettlementDenied": True,
                "replacementInvocationReclaimed": True,
                "replacementBatchBytes": 2 * 1048576,
            },
            "deviceReturn": {
                "postScanNativeDemandDenied": True,
                "beforeUsedKiB": 65536,
                "afterUsedKiB": 0,
                "restoredPriority": 10,
                "interruptedRestored": True,
                "brokerUnavailableRestored": True,
                "wrongActivePriorityDenied": True,
                "pinnedAliasRestored": True,
            },
            "completion": {
                "parentAndEscrowBytes": 128 * 1048576,
                "transferredBytes": 128 * 1048576,
                "postParentBytes": 96 * 1048576,
                "outerRetainedAcrossRestart": True,
                "ownersFinishedWithDescendant": True,
                "newSerialGranted": False,
                "gameMemoryBytes": 64 * 1048576,
            },
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "amc-foreground-evidence.json"
            path.write_text(json.dumps(receipt))
            hosted.verify_foreground_evidence(path)
            hosted.verify_foreground_evidence(path, "6.18")
            for value in [False, None]:
                path.write_text(json.dumps({**receipt, "outsideRunnerPreparationDenied": value}))
                with self.subTest(outsideRunnerPreparationDenied=value), self.assertRaises(RuntimeError):
                    hosted.verify_foreground_evidence(path)
            for value in (False, None, "true", 1):
                path.write_text(json.dumps({**receipt, "userCreatedPreparationHelperDenied": value}))
                with self.subTest(userCreatedPreparationHelperDenied=value), self.assertRaises(RuntimeError):
                    hosted.verify_foreground_evidence(path)
            for value in (False, None, "true", 1):
                broken = json.loads(json.dumps(receipt))
                broken["deviceReturn"]["pinnedAliasRestored"] = value
                path.write_text(json.dumps(broken))
                with self.subTest(pinnedAliasRestored=value), self.assertRaises(RuntimeError):
                    hosted.verify_foreground_evidence(path)
            path.write_text(json.dumps(receipt))
            with self.assertRaises(RuntimeError):
                hosted.verify_foreground_evidence(path, "7.2")
            current = {**receipt, "kernel": "7.2.8-cachyos-lto"}
            path.write_text(json.dumps(current))
            hosted.verify_foreground_evidence(path, "7.2")
            for kernel in ["6.18.48", "7.20.8", "", None]:
                path.write_text(json.dumps({**receipt, "kernel": kernel}))
                with self.subTest(kernel=kernel), self.assertRaises(RuntimeError):
                    hosted.verify_foreground_evidence(path, "7.2")
            for key, value in [
                ("frozenMigration", False),
                ("originalOwnerDenied", False),
                ("offlineFallbackDenied", False),
                ("helperFallbackDenied", False),
                ("fallbackResidentBytes", 0),
                ("fallbackCharge", {}),
                ("onlineCharge", {}),
                ("helperDenial", {}),
                ("oomKill", 1),
            ]:
                broken = json.loads(json.dumps(receipt))
                broken["chargeOwner"][key] = value
                path.write_text(json.dumps(broken))
                with self.subTest(charge=key), self.assertRaises(RuntimeError):
                    hosted.verify_foreground_evidence(path)
            for batch, key, value in [
                ("helperDenial", "granted", True),
                (
                    "helperDenial",
                    "observationsBefore",
                    {"helper": {"memory": 100 * 1048576}},
                ),
                ("helperDenial", "helperEvents", {"oom": "1", "oom_kill": "0"}),
                ("helperDenial", "inventoryStatusReplies", 0),
                ("fallbackCharge", "inventoryMaxStatusLatencyMs", 2000),
                ("onlineCharge", "inventoryStatusReplies", None),
                ("fallbackCharge", "chargedBytes", {"unbacked": 2 * 1048576}),
                ("fallbackCharge", "chargeInodes", {}),
                ("fallbackCharge", "uniqueResidentBytes", 1048576),
                ("onlineCharge", "chargedBytes", {"helper": 2 * 1048576}),
                ("onlineCharge", "chargedBytes", {"original": 1048576}),
            ]:
                broken = json.loads(json.dumps(receipt))
                broken["chargeOwner"][batch][key] = value
                path.write_text(json.dumps(broken))
                with (
                    self.subTest(batch=batch, key=key),
                    self.assertRaises(RuntimeError),
                ):
                    hosted.verify_foreground_evidence(path)
            for section, key, value in [
                ("poolRetirement", "removedDomainReleased", False),
                ("poolRetirement", "liveOwnerCleared", False),
                ("poolRetirement", "descendantRetainedBytes", 0),
                ("poolRetirement", "finalBytes", 1),
                ("abandonedPreparation", "sigkillWhileBrokerOffline", False),
                ("abandonedPreparation", "waitMilliseconds", 60000),
                ("abandonedPreparation", "restartClearedBarrier", False),
                ("abandonedPreparation", "existingWorkRetainedBytes", 0),
                ("abandonedPreparation", "payloadDidNotExecute", False),
                ("pageReturn", "beforeSwapBytes", 0),
                ("pageReturn", "schemaVersion", 1),
                ("pageReturn", "beforeReturnBytes", 0),
                ("pageReturn", "afterReturnBytes", 4096),
                ("pageReturn", "afterCachedBytes", 0),
                ("pageReturn", "afterCachedBytes", 65 * 1048576),
                ("pageReturn", "mappingBytes", 0),
                ("pageReturn", "beforeSwappedBytes", 0),
                ("pageReturn", "afterPresentBytes", 63 * 1048576),
                ("pageReturn", "afterSwappedBytes", 4096),
                ("pageReturn", "dataIntact", False),
                ("pageReturn", "waitExitCode", 0),
                ("pageReturn", "interruptedBatchBytes", 0),
                ("pageReturn", "unreadBatchSettlementDenied", False),
                ("pageReturn", "replacementInvocationReclaimed", False),
                ("pageReturn", "replacementBatchBytes", 0),
                ("namespaceRunnerBacking", "reservedBytes", 0),
                ("namespaceRunnerBacking", "aggregateBytesPerUser", 0),
                ("namespaceRunnerBacking", "concurrentRunners", 1),
                ("namespaceRunnerBacking", "innerCommittedBytes", 0),
                ("namespaceRunnerBacking", "restartPreserved", False),
                ("namespaceRunnerBacking", "changedCeilingDenied", False),
                ("namespaceRunnerBacking", "removedUserRetained", False),
                ("namespaceRunnerBacking", "excessReserveDenied", False),
                ("namespaceRunnerBacking", "retiredUserReleasedAfterCleanup", False),
                ("burstManagerResponsiveness", "responses", 0),
                ("burstManagerResponsiveness", "elapsedSeconds", 2),
                ("deviceReturn", "postScanNativeDemandDenied", False),
                ("preparationHelperBacking", "concurrentHelpers", 1),
                ("preparationHelperBacking", "aggregateBytesPerUser", 0),
                ("preparationHelperBacking", "reservedBytes", 0),
                ("preparationHelperBacking", "restartPreserved", False),
                ("preparationHelperBacking", "payloadsEnteredAfterDrain", False),
                ("preparationHelperBacking", "existingWorkCompleted", False),
                ("deviceReturn", "afterUsedKiB", 65536),
                ("deviceReturn", "restoredPriority", -1),
                ("deviceReturn", "interruptedRestored", False),
                ("deviceReturn", "brokerUnavailableRestored", False),
                ("deviceReturn", "wrongActivePriorityDenied", False),
                ("completion", "transferredBytes", 224 * 1048576),
                ("completion", "postParentBytes", 0),
                ("completion", "outerRetainedAcrossRestart", False),
                ("completion", "ownersFinishedWithDescendant", False),
                ("completion", "newSerialGranted", True),
                ("completion", "gameMemoryBytes", 0),
            ]:
                broken = json.loads(json.dumps(receipt))
                broken[section][key] = value
                path.write_text(json.dumps(broken))
                with (
                    self.subTest(section=section, key=key),
                    self.assertRaises(RuntimeError),
                ):
                    hosted.verify_foreground_evidence(path)

            for section in receipt:
                broken = dict(receipt)
                del broken[section]
                path.write_text(json.dumps(broken))
                with self.subTest(missing=section), self.assertRaises(RuntimeError):
                    hosted.verify_foreground_evidence(path)
            for variant, modes in receipt["nativeNamespaceCompletion"].items():
                for mode in modes:
                    for key, value in [
                        ("rapid_jobs", []),
                        ("startup_loss", {}),
                        ("host_runner_loss", {}),
                    ]:
                        broken = json.loads(json.dumps(receipt))
                        broken["nativeNamespaceCompletion"][variant][mode][key] = value
                        path.write_text(json.dumps(broken))
                        with (
                            self.subTest(variant=variant, mode=mode, key=key),
                            self.assertRaises(RuntimeError),
                        ):
                            hosted.verify_foreground_evidence(path)

    def guard_evidence(self):
        guards = {path: {"schemaVersion": 1, "missingGuardDenied": True,
                       "postScanChargeOwnerDenied": True,
                      "duplicateGuardHeld": True, "targetMigrationDeniedAfterGrant": True,
                      "readerMigrationDeniedAfterGrant": True, "brokerRestartProtected": True,
                      "helperLossReleased": True, "residentBytes": 2 * 1048576,
                      "directBytes": 2 * 1048576 if path == "direct" else 0,
                      "cacheBytes": 2 * 1048576 if path == "cache" else 0,
                      "targetInode": 123, "readerInode": 456,
                      "chargedBytesByInode": {str(123 if path == "direct" else 456): 2 * 1048576},
                      "oom": 0, "oomKill": 0} for path in ["direct", "cache"]}
        guards["lifetime"] = dict.fromkeys(["sharedMmMigrationDenied", "conflictingGuardDenied",
            "cloneIntoCgroupDenied", "privateForkReset", "ownerTurnoverPinnedCharge", "offliningDenied",
            "controllerDisableDenied", "unchangedControllersAllowed", "execCookieChanged", "obsoleteMmReadEmpty", "lastCloseReleased"], True)
        return guards

    def test_guard_receipt_requires_both_fault_paths_and_denies_weakened_native_proof(self):
        valid = self.guard_evidence()
        hosted.verify_kernel_guard_evidence(valid)
        for path in ("direct", "cache"):
            incomplete = dict(valid)
            del incomplete[path]
            with self.subTest(missing=path), self.assertRaises(RuntimeError):
                hosted.verify_kernel_guard_evidence(incomplete)
            for field, value in [("schemaVersion", 0), ("missingGuardDenied", False),
                                  ("postScanChargeOwnerDenied", False),
                                 ("duplicateGuardHeld", False), ("targetMigrationDeniedAfterGrant", False),
                                 ("readerMigrationDeniedAfterGrant", False), ("brokerRestartProtected", False),
                                 ("helperLossReleased", False), ("residentBytes", 4096),
                                 ("directBytes", 4096), ("cacheBytes", 4096), ("readerInode", 123),
                                 ("targetInode", 0), ("chargedBytesByInode", {"unbacked": 2 * 1048576}),
                                 ("oom", 1), ("oomKill", 1)]:
                invalid = json.loads(json.dumps(valid))
                invalid[path][field] = value
                with self.subTest(path=path, field=field), self.assertRaises(RuntimeError):
                    hosted.verify_kernel_guard_evidence(invalid)
        for field in valid["lifetime"]:
            invalid = json.loads(json.dumps(valid))
            invalid["lifetime"][field] = False
            with self.subTest(lifetime=field), self.assertRaises(RuntimeError):
                hosted.verify_kernel_guard_evidence(invalid)

    def namespace_evidence(self):
        run = {
            "rapid_jobs": [
                {"exit": code, "streams": "preserved"} for code in [0, 42, 0, 42]
            ],
            "startup_loss": {"disconnect": "no-exec", "invalid-ack": "no-exec"},
            "host_runner_loss": {
                "disconnect": "no-exec",
                "wrong-pid": "no-exec",
                "wrong-cgroup": "no-exec",
            },
        }
        return {
            variant: {
                mode: json.loads(json.dumps(run)) for mode in ["native", "admitted"]
            }
            for variant in ["private-pid", "private-pid-user"]
        }

    def test_supervision_exports_are_complete_and_have_no_oom_events(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            names = (
                "trace.jsonl",
                "recovery.json",
                "status.json",
                "replay.json",
                "replay-input.json",
                "oom.json",
                "heartbeat.json",
                "cold-status.json",
                "admission-failures.json",
                "admission-observations.json",
                "replacement-status.json",
                "domain-budget-status.json",
                "host-budget-status.json",
                "foreign-status.json",
            )
            for name in names:
                (path / name).write_text("{}")
            clean = {
                phase: {"oom": 0, "oom_kill": 0, "host_oom_kill": 0}
                for phase in ("initial", "final")
            }
            (path / "oom.json").write_text(json.dumps(clean))
            heartbeat = [
                {"inhibit": True, "age_ms": 0, "granted": False},
                {"inhibit": False, "age_ms": 4000, "granted": False},
                {"inhibit": False, "age_ms": 0, "granted": True},
            ]
            (path / "heartbeat.json").write_text(json.dumps(heartbeat))
            self.admission_evidence(path)
            self.recovery_evidence(path)
            cold = {
                "recovery": {"active": None, "attempts": [{"domain": "cold-backend"}]}
            }
            (path / "cold-status.json").write_text(json.dumps(cold))
            binding = {
                "schemaVersion": 1,
                "traceSha256": hashlib.sha256(
                    (path / "trace.jsonl").read_bytes()
                ).hexdigest(),
                "traceBytes": (path / "trace.jsonl").stat().st_size,
                "replaySha256": hashlib.sha256(
                    (path / "replay.json").read_bytes()
                ).hexdigest(),
            }
            (path / "replay-input.json").write_text(json.dumps(binding))
            hosted.verify_supervision_evidence(path)
            for name in ("trace.jsonl", "replay.json"):
                contents = (path / name).read_text()
                (path / name).write_text(contents + "\n{}\n")
                with self.subTest(stale_replay=name), self.assertRaises(RuntimeError):
                    hosted.verify_supervision_evidence(path)
                (path / name).write_text(contents)
            for name in names:
                contents = (path / name).read_text()
                (path / name).unlink()
                with self.subTest(missing=name), self.assertRaises(RuntimeError):
                    hosted.verify_supervision_evidence(path)
                (path / name).write_text(contents)
            for index in range(3):
                broken = json.loads(json.dumps(heartbeat))
                broken[index]["granted"] = not broken[index]["granted"]
                (path / "heartbeat.json").write_text(json.dumps(broken))
                with self.subTest(heartbeat=index), self.assertRaises(RuntimeError):
                    hosted.verify_supervision_evidence(path)
            (path / "heartbeat.json").write_text(json.dumps(heartbeat))
            for recovery in [
                {},
                {"active": None, "attempts": []},
                {"active": "tripped", "attempts": cold["recovery"]["attempts"]},
                {"active": None, "attempts": cold["recovery"]["attempts"] * 2},
            ]:
                (path / "cold-status.json").write_text(
                    json.dumps({"recovery": recovery})
                )
                with self.subTest(cold=recovery), self.assertRaises(RuntimeError):
                    hosted.verify_supervision_evidence(path)
            (path / "cold-status.json").write_text(json.dumps(cold))
            for phase, counters in clean.items():
                for counter in counters:
                    dirty = json.loads(json.dumps(clean))
                    dirty[phase][counter] = 1
                    (path / "oom.json").write_text(json.dumps(dirty))
                    with (
                        self.subTest(phase=phase, counter=counter),
                        self.assertRaises(RuntimeError),
                    ):
                        hosted.verify_supervision_evidence(path)

    def report(self, directory, names, outcome=None):
        root = ET.Element("testsuites")
        suite = ET.SubElement(root, "testsuite")
        for name in names:
            case = ET.SubElement(suite, "testcase", name=name)
            if outcome:
                ET.SubElement(case, outcome)
        path = Path(directory) / "junit.xml"
        ET.ElementTree(root).write(path)
        return path

    def admission_evidence(self, path):
        entries = []

        def sample(case):
            valid = case == "valid"
            if valid:
                entries.append(
                    {
                        "id": str(len(entries)),
                        "memory_bytes": 64 * 1024**2,
                        "granted": True,
                    }
                )
            return {
                "case": case,
                "granted": valid,
                "samples": 20,
                "elapsedSeconds": 2.1,
                "committedBytes": sum(r["memory_bytes"] for r in entries),
                "retained": entries.copy(),
                "waiting": None if valid else "unknown",
            }

        heartbeats = [
            sample(case)
            for case in [
                "valid",
                "missing",
                "malformed",
                "oversized",
                "writable",
                "foreign-owner",
                "symlink",
                "expired",
                "future",
                "changed-boot",
                "inhibit",
                "degraded",
                "valid",
            ]
        ]
        memory = [
            sample(case)
            for case in [
                "missing-meminfo",
                "malformed-meminfo",
                "missing-memory-psi",
                "malformed-memory-psi",
                "valid",
            ]
        ]
        (path / "admission-failures.json").write_text(json.dumps(heartbeats))
        (path / "admission-observations.json").write_text(json.dumps(memory))
        return heartbeats, memory

    def test_native_denial_requires_retained_ceilings_multiple_ticks_and_restored_progress(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            heartbeats, memory = self.admission_evidence(path)
            hosted.verify_admission_failures(path)
            for filename, original in [
                ("admission-failures.json", heartbeats),
                ("admission-observations.json", memory),
            ]:
                for field, value in [
                    ("granted", True),
                    ("samples", 1),
                    ("elapsedSeconds", 0.1),
                    ("committedBytes", 0),
                    ("retained", []),
                    ("waiting", None),
                ]:
                    broken = json.loads(json.dumps(original))
                    index = 1 if filename == "admission-failures.json" else 0
                    broken[index][field] = value
                    (path / filename).write_text(json.dumps(broken))
                    with (
                        self.subTest(file=filename, field=field),
                        self.assertRaises(RuntimeError),
                    ):
                        hosted.verify_admission_failures(path)
                broken = json.loads(json.dumps(original))
                index = 1 if filename == "admission-failures.json" else 0
                extra = dict(broken[index]["retained"][0], id="unexpected-async-grant")
                broken[index]["retained"].append(extra)
                broken[index]["committedBytes"] += extra["memory_bytes"]
                (path / filename).write_text(json.dumps(broken))
                with (
                    self.subTest(asynchronous_grant=filename),
                    self.assertRaises(RuntimeError),
                ):
                    hosted.verify_admission_failures(path)
                (path / filename).write_text(json.dumps(original[:-1]))
                with (
                    self.subTest(missing_recovery=filename),
                    self.assertRaises(RuntimeError),
                ):
                    hosted.verify_admission_failures(path)
                (path / filename).write_text(json.dumps(original))

    def recovery_evidence(self, path):
        identity = {"invocation": "a" * 32}
        original = {"identity": identity, "phase": {"phase": "cooling"}}
        status = {
            "inhibit": True,
            "recovery": {
                "active": {"identity": identity, "phase": {"phase": "tripped"}},
                "attempts": [{"domain": "replacement"}],
            },
        }
        replacement = {
            "original": original,
            "replacementInvocation": "b" * 32,
            "status": status,
        }
        (path / "replacement-status.json").write_text(json.dumps(replacement))
        for name, domains, domain_limit, host_limit in [
            ("domain", ["budget-a", "budget-a"], 2, 4),
            ("host", ["budget-b", "budget-a"], 3, 2),
        ]:
            attempts = [
                {"domain": domain, "unix_ms": index}
                for index, domain in enumerate(domains)
            ]
            state = {
                "inhibit": True,
                "policy": {
                    "domain_recovery_limit": domain_limit,
                    "host_recovery_limit": host_limit,
                },
                "recovery": {
                    "active": {"identity": identity, "phase": {"phase": "tripped"}},
                    "attempts": attempts,
                },
            }
            receipt = {
                "beforeRestart": state,
                "afterRestart": state,
                "failedInvocation": identity["invocation"],
                "completedRecoveries": [
                    {"recovery": {"active": None, "attempts": attempts[:index]}}
                    for index in (1, 2)
                ],
            }
            (path / (name + "-budget-status.json")).write_text(json.dumps(receipt))
        foreign = {
            "invocation": "c" * 32,
            "finalInvocation": "c" * 32,
            "starts": 1,
            "childAlive": True,
        }
        (path / "foreign-status.json").write_text(json.dumps(foreign))

    def test_recovery_receipts_reject_replacement_budget_reset_and_foreign_disruption(
        self,
    ):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            self.recovery_evidence(path)
            hosted.verify_recovery_failures(path)
            mutations = [
                (
                    "replacement-status.json",
                    lambda r: r.update(replacementInvocation="a" * 32),
                ),
                (
                    "replacement-status.json",
                    lambda r: r["status"]["recovery"]["active"].update(
                        identity={"invocation": "b" * 32}
                    ),
                ),
                (
                    "replacement-status.json",
                    lambda r: r["status"].update(inhibit=False),
                ),
                (
                    "domain-budget-status.json",
                    lambda r: r["afterRestart"]["recovery"]["attempts"].pop(),
                ),
                (
                    "domain-budget-status.json",
                    lambda r: r["afterRestart"]["policy"].update(
                        domain_recovery_limit=3
                    ),
                ),
                (
                    "host-budget-status.json",
                    lambda r: r["afterRestart"]["recovery"]["attempts"][1].update(
                        domain="budget-b"
                    ),
                ),
                (
                    "host-budget-status.json",
                    lambda r: r["completedRecoveries"][0]["recovery"].update(
                        active="cooling"
                    ),
                ),
                (
                    "host-budget-status.json",
                    lambda r: r.update(failedInvocation="b" * 32),
                ),
                ("foreign-status.json", lambda r: r.update(finalInvocation="d" * 32)),
                ("foreign-status.json", lambda r: r.update(childAlive=False)),
            ]
            for filename, mutate in mutations:
                original = (path / filename).read_text()
                broken = json.loads(original)
                mutate(broken)
                (path / filename).write_text(json.dumps(broken))
                with (
                    self.subTest(file=filename, mutation=mutate),
                    self.assertRaises(RuntimeError),
                ):
                    hosted.verify_recovery_failures(path)
                (path / filename).write_text(original)

    def test_complete_successful_native_execution_is_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            hosted.verify_native_report(
                self.report(directory, hosted.NATIVE_CASES | {"main"})
            )

    def test_supervision_requires_its_own_recovery_and_shadow_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            hosted.verify_native_report(
                self.report(directory, hosted.SUPERVISION_CASES),
                hosted.SUPERVISION_CASES,
            )
            with self.assertRaises(RuntimeError):
                hosted.verify_native_report(
                    self.report(directory, hosted.NATIVE_CASES),
                    hosted.SUPERVISION_CASES,
                )
            for name in hosted.SUPERVISION_CASES:
                with self.subTest(missing=name), self.assertRaises(RuntimeError):
                    hosted.verify_native_report(
                        self.report(directory, hosted.SUPERVISION_CASES - {name}),
                        hosted.SUPERVISION_CASES,
                    )

    def test_missing_report_cases_and_unsuccessful_cases_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            for names in (
                set(),
                {"main"},
                hosted.NATIVE_CASES - {next(iter(hosted.NATIVE_CASES))},
            ):
                with self.subTest(names=names), self.assertRaises(RuntimeError):
                    hosted.verify_native_report(self.report(directory, names))
            for outcome in ("failure", "error", "skipped"):
                with self.subTest(outcome=outcome), self.assertRaises(RuntimeError):
                    hosted.verify_native_report(
                        self.report(directory, hosted.NATIVE_CASES, outcome)
                    )
            with self.assertRaises(FileNotFoundError):
                hosted.verify_native_report(Path(directory) / "absent.xml")


if __name__ == "__main__":
    unittest.main()
