import importlib.util
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

spec = importlib.util.spec_from_file_location("hosted_ci", Path(__file__).with_name("hosted-ci.py"))
hosted = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hosted)


class NativeEvidenceTests(unittest.TestCase):
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
