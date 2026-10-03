import importlib.util
import json
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

spec = importlib.util.spec_from_file_location("hosted_ci", Path(__file__).with_name("hosted-ci.py"))
hosted = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hosted)


class NativeEvidenceTests(unittest.TestCase):
    def test_supervision_exports_are_complete_and_have_no_oom_events(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            names = ("trace.jsonl", "recovery.json", "status.json", "replay.json", "oom.json")
            for name in names:
                (path / name).write_text("{}")
            clean = {phase: {"oom": 0, "oom_kill": 0, "host_oom_kill": 0} for phase in ("initial", "final")}
            (path / "oom.json").write_text(json.dumps(clean))
            hosted.verify_supervision_evidence(path)
            for name in names:
                contents = (path / name).read_text()
                (path / name).unlink()
                with self.subTest(missing=name), self.assertRaises(RuntimeError):
                    hosted.verify_supervision_evidence(path)
                (path / name).write_text(contents)
            for phase, counters in clean.items():
                for counter in counters:
                    dirty = json.loads(json.dumps(clean))
                    dirty[phase][counter] = 1
                    (path / "oom.json").write_text(json.dumps(dirty))
                    with self.subTest(phase=phase, counter=counter), self.assertRaises(RuntimeError):
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

    def test_complete_successful_native_execution_is_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            hosted.verify_native_report(self.report(directory, hosted.NATIVE_CASES | {"main"}))

    def test_supervision_requires_its_own_recovery_and_shadow_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            hosted.verify_native_report(self.report(directory, hosted.SUPERVISION_CASES), hosted.SUPERVISION_CASES)
            with self.assertRaises(RuntimeError):
                hosted.verify_native_report(self.report(directory, hosted.NATIVE_CASES), hosted.SUPERVISION_CASES)
            for name in hosted.SUPERVISION_CASES:
                with self.subTest(missing=name), self.assertRaises(RuntimeError):
                    hosted.verify_native_report(self.report(directory, hosted.SUPERVISION_CASES - {name}), hosted.SUPERVISION_CASES)

    def test_missing_report_cases_and_unsuccessful_cases_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            for names in (set(), {"main"}, hosted.NATIVE_CASES - {next(iter(hosted.NATIVE_CASES))}):
                with self.subTest(names=names), self.assertRaises(RuntimeError):
                    hosted.verify_native_report(self.report(directory, names))
            for outcome in ("failure", "error", "skipped"):
                with self.subTest(outcome=outcome), self.assertRaises(RuntimeError):
                    hosted.verify_native_report(self.report(directory, hosted.NATIVE_CASES, outcome))
            with self.assertRaises(FileNotFoundError):
                hosted.verify_native_report(Path(directory) / "absent.xml")


if __name__ == "__main__":
    unittest.main()
