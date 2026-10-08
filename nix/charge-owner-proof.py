"""Disposable VM only: mixed original charges and offlined-owner fallback."""

import ctypes
import errno
import json
import mmap
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time

MIB = 1048576
ROOT = Path("/sys/fs/cgroup/amc-charge-proof")
SOCKET = "/run/amc-charge/admission.sock"


def request(payload):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(SOCKET)
        connection.sendall((json.dumps(payload) + "\n").encode())
        reply = json.loads(connection.makefile().readline())
    assert reply["error"] is None, reply
    return reply


def batch():
    target = json.loads(Path("/tmp/charge-ranges.json").read_text())
    address = target[sys.argv[2]]
    pid = target["pid"]
    start = int(Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[19])
    reply = request(
        {
            "op": "acquire_page_return",
            "version": 1,
            "pid": pid,
            "start_ticks": start,
            "address": address,
            "bytes": 2 * MIB,
        }
    )
    if reply["granted"]:
        with open(f"/proc/{pid}/mem", "rb", buffering=0) as memory:
            for offset in range(0, 2 * MIB, mmap.PAGESIZE):
                assert (
                    len(os.pread(memory.fileno(), mmap.PAGESIZE, address + offset))
                    == mmap.PAGESIZE
                )
        reply = request({"op": "finish_page_return", "version": 1})
        assert reply["granted"] and reply["resident_bytes"] == 2 * MIB, reply
    Path("/tmp/charge-batch.json").write_text(json.dumps(reply))


def observe(group):
    stats = dict(
        line.split() for line in (group / "memory.stat").read_text().splitlines()
    )
    return {
        "memory": int((group / "memory.current").read_text()),
        "swap": int((group / "memory.swap.current").read_text()),
        "cached": int(stats["swapcached"]),
    }


def wait(predicate):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("charge-owner native setup timed out")


def run_batch(which):
    wait(lambda: request({"op": "status", "version": 1}).get("recovery") is None)
    subprocess.run(
        [
            "systemd-run",
            "--unit=page-return",
            "--property=MemoryMax=128M",
            "--property=MemorySwapMax=0",
            "--wait",
            "--",
            "python3",
            __file__,
            "--batch",
            which,
        ],
        check=True,
    )
    return json.loads(Path("/tmp/charge-batch.json").read_text())


def proof():
    ROOT.mkdir()
    (ROOT / "cgroup.subtree_control").write_text("+memory")
    (ROOT / "memory.max").write_text(str(512 * MIB))
    for name in ("a", "b", "destination", "moved"):
        group = ROOT / name
        group.mkdir()
        (group / "memory.max").write_text(str(128 * MIB))
        (group / "memory.swap.max").write_text(str(128 * MIB))
    read, write = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read)
        libc = ctypes.CDLL(None, use_errno=True)
        libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
        memories = []
        ranges = {"pid": os.getpid()}
        for name in ("a", "b"):
            (ROOT / name / "cgroup.procs").write_text(str(os.getpid()))
            memory = mmap.mmap(
                -1, 32 * MIB, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS
            )
            for offset in range(0, len(memory), mmap.PAGESIZE):
                memory[offset] = 7
            address = ctypes.addressof(ctypes.c_char.from_buffer(memory))
            assert libc.madvise(address, len(memory), 21) == 0
            ranges[name] = address
            memories.append(memory)
        (ROOT / "destination" / "cgroup.procs").write_text(str(os.getpid()))
        Path("/tmp/charge-ranges.json").write_text(json.dumps(ranges))
        os.write(write, b"ready")
        while True:
            time.sleep(1)
    os.close(write)
    try:
        assert os.read(read, 5) == b"ready"
        for name in ("a", "b"):
            try:
                (ROOT / name / "memory.reclaim").write_text(
                    f"{48 * MIB} swappiness=max"
                )
            except OSError as error:
                if error.errno != errno.EAGAIN:
                    raise
            wait(
                lambda: (
                    observe(ROOT / name)["swap"] >= 32 * MIB
                    and observe(ROOT / name)["cached"] == 0
                )
            )
        before = {name: observe(ROOT / name) for name in ("a", "b", "destination")}
        assert before["destination"]["swap"] == 0, before
        # Freezing an mm does not prevent a privileged cgroup migration.
        (ROOT / "destination" / "cgroup.freeze").write_text("1")
        wait(lambda: "frozen 1" in (ROOT / "destination" / "cgroup.events").read_text())
        (ROOT / "moved" / "cgroup.freeze").write_text("1")
        (ROOT / "moved" / "cgroup.procs").write_text(str(pid))
        wait(lambda: "frozen 1" in (ROOT / "moved" / "cgroup.events").read_text())
        assert (
            str(ROOT).removeprefix("/sys/fs/cgroup") + "/moved"
            in Path(f"/proc/{pid}/cgroup").read_text()
        )
        assert observe(ROOT / "moved")["swap"] == 0
        policy = json.loads(Path("/etc/amc-test-host-policy.json").read_text())
        policy["swap_recovery"]["page_cgroups"] = ["/amc-charge-proof"]
        Path("/tmp/charge-policy.json").write_text(json.dumps(policy))
        subprocess.run(
            [
                "systemd-run",
                "--unit=charge-owner-host",
                "--",
                "amc",
                "admission",
                "host-serve",
                "--policy",
                "/tmp/charge-policy.json",
                "--socket",
                SOCKET,
                "--state",
                "/var/lib/amc-charge",
            ],
            check=True,
        )
        wait(lambda: Path(SOCKET).exists())
        (ROOT / "a" / "memory.max").write_text(str(observe(ROOT / "a")["memory"] + MIB))
        denied_origin = run_batch("a")
        assert (
            not denied_origin["granted"]
            and denied_origin["waiting"] == "ancestor_headroom"
        ), denied_origin
        (ROOT / "a" / "memory.max").write_text(str(128 * MIB))
        # The original memcg becomes offline while its swapped mm survives.
        (ROOT / "a").rmdir()
        moved_before = observe(ROOT / "moved")
        (ROOT / "moved" / "memory.max").write_text(str(moved_before["memory"] + MIB))
        denied_fallback = run_batch("a")
        assert (
            not denied_fallback["granted"]
            and denied_fallback["waiting"] == "ancestor_headroom"
        ), denied_fallback
        (ROOT / "moved" / "memory.max").write_text(str(128 * MIB))
        assert run_batch("a")["resident_bytes"] == 2 * MIB
        wait(
            lambda: (
                observe(ROOT / "moved")["memory"] >= moved_before["memory"] + 2 * MIB
            )
        )
        moved_after = observe(ROOT / "moved")
        b_before = observe(ROOT / "b")
        assert run_batch("b")["resident_bytes"] == 2 * MIB
        wait(lambda: observe(ROOT / "b")["memory"] >= b_before["memory"] + 2 * MIB)
        b_after = observe(ROOT / "b")
        events = dict(
            line.split() for line in (ROOT / "memory.events").read_text().splitlines()
        )
        assert int(events["oom"]) == 0 and int(events["oom_kill"]) == 0, events
        receipt = {
            "kernel": os.uname().release,
            "before": before,
            "frozenMigration": True,
            "originalOwnerDenied": True,
            "offlineFallbackDenied": True,
            "batchBytes": 2 * MIB,
            "fallbackResidentBytes": 2 * MIB,
            "onlineResidentBytes": 2 * MIB,
            "fallbackBefore": moved_before,
            "fallbackAfter": moved_after,
            "onlineBefore": b_before,
            "onlineAfter": b_after,
            "oom": int(events["oom"]),
            "oomKill": int(events["oom_kill"]),
        }
        Path("/tmp/charge-owner-evidence.json").write_text(json.dumps(receipt))
    finally:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        os.close(read)
        subprocess.run(
            ["systemctl", "stop", "page-return", "charge-owner-host"], check=False
        )
        for group in ROOT.iterdir():
            if group.is_dir():
                group.rmdir()
        ROOT.rmdir()


if sys.argv[1:2] == ["--batch"]:
    batch()
else:
    proof()
