import hashlib
import importlib.util
import json
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "hosted_ci", Path(__file__).with_name("hosted-ci.py")
)
hosted = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hosted)


class NativeEvidenceTests(unittest.TestCase):
    def test_foreground_receipt_rejects_partial_return_unbacked_transfer_and_new_work(
        self,
    ):
        receipt = {
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
                },
                "fallbackCharge": {
                    "granted": True,
                    "resident_bytes": 2 * 1048576,
                    "uniqueResidentBytes": 2 * 1048576,
                    "chargedBytes": {"helper": 2 * 1048576},
                    "chargeInodes": {"helper": 123},
                    "helperEvents": {"oom": "0", "oom_kill": "0"},
                },
                "onlineCharge": {
                    "granted": True,
                    "resident_bytes": 2 * 1048576,
                    "uniqueResidentBytes": 2 * 1048576,
                    "chargedBytes": {"original": 2 * 1048576},
                    "chargeInodes": {"original": 456},
                    "helperEvents": {"oom": "0", "oom_kill": "0"},
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
            },
            "deviceReturn": {
                "beforeUsedKiB": 65536,
                "afterUsedKiB": 0,
                "restoredPriority": 10,
                "interruptedRestored": True,
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
                ("deviceReturn", "afterUsedKiB", 65536),
                ("deviceReturn", "restoredPriority", -1),
                ("deviceReturn", "interruptedRestored", False),
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
