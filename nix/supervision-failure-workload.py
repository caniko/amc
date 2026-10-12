"""Small VM backend with explicit failure injection and observable descendants."""

import pathlib
import signal
import subprocess
import sys
import time

name = sys.argv[1]
root = pathlib.Path("/var/lib/amc-fixture")
counter = root / (name + "-starts")
counter.write_text(str(int(counter.read_text()) + 1 if counter.exists() else 1))
if (root / (name + "-fail")).exists():
    sys.exit(42)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
child = subprocess.Popen([sys.executable, "-c", "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(600)"])
(root / (name + "-child")).write_text(str(child.pid))
time.sleep(600)
