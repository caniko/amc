"""Raw MangoHud regression inputs; no game or manager access."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("frame_report", Path(__file__).with_name("frame-report.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class FrameReportTests(unittest.TestCase):
    def report(self, content, **options):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "frames.csv"
            path.write_text(content)
            return module.report(path, start_seconds=1, end_seconds=3, stall_ms=25, log_interval_ms=0, **options)

    def test_window_tails_stalls_and_units(self):
        value = self.report("os,cpu\nLinux,example\nfps,frametime,elapsed\n100,10,500000000\n100,10,1000000000\n20,50,2000000000\n40,25,3000000000\n50,20,3500000000\n")
        self.assertEqual(value["frameCount"], 3)
        self.assertEqual(value["p99Ms"], 50)
        self.assertEqual(value["stalls"], 1)
        self.assertEqual(value["stallsPerSecond"], .5)
        self.assertEqual(value["windowSeconds"], 2)

    def test_malformed_and_incomplete_evidence_is_rejected(self):
        for content in (
            "Average FPS,Average Frame Time\n100,10\n",
            "fps,frametime,elapsed\n100,nan,0\n100,10,4000000000\n",
            "fps,frametime,elapsed\n100,0,0\n100,10,4000000000\n",
            "fps,frametime,elapsed\n100,10,0\n100,10,0\n",
            "fps,frametime,elapsed\n100,10,0\n100,10,2000000000\n",
            "fps,frametime,elapsed\n100,10,2000000000\n100,10,4000000000\n",
            "fps,frametime,elapsed,elapsed\n100,10,0,0\n",
            "fps,frametime,elapsed\n100,10,-1\n",
            "fps,frametime,elapsed\n100,10,0,extra\n",
            "fps,frametime,elapsed\n100,10,0\n\n100,10,4000000000\n",
        ):
            with self.subTest(content=content), self.assertRaises(ValueError):
                self.report(content)

    def test_periodic_logging_and_unbounded_input_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "frames.csv"
            path.write_text("fps,frametime,elapsed\n100,10,0\n100,10,4000000000\n")
            with self.assertRaises(ValueError):
                module.report(path, start_seconds=1, end_seconds=3, stall_ms=25, log_interval_ms=100)
            with self.assertRaises(ValueError):
                module.report(path, start_seconds=1, end_seconds=3, stall_ms=25, log_interval_ms=0, max_bytes=1)


if __name__ == "__main__":
    unittest.main()
