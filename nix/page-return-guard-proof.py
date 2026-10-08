"""Disposable VM: force migration AFTER grant on direct and swap-cache paths."""

import ctypes
import errno
import json
import mmap
import os
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

MIB = 1048576
ROOT = Path("/sys/fs/cgroup/amc-guard-proof")
SOCKET = "/run/amc-guard/admission.sock"
META = Path("/tmp/guard-target.json")
READY = Path("/tmp/guard-granted.json")
PROCEED = Path("/tmp/guard-read")
RESULT = Path("/tmp/guard-result.json")


def wait(predicate):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("kernel guard proof timed out")


def request(payload):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(30)
        connection.connect(SOCKET)
        connection.sendall((json.dumps(payload) + "\n").encode())
        return json.loads(connection.makefile().readline())


def host_ready():
    try:
        return request({"op": "status", "version": 1})["error"] is None
    except (ConnectionRefusedError, FileNotFoundError):
        return False


def fdinfo(fd):
    return dict(line.split(":", 1) for line in Path(f"/proc/self/fdinfo/{fd}").read_text().splitlines())


def helper(killed):
    target = json.loads(META.read_text())
    start = int(Path(f"/proc/{target['pid']}/stat").read_text().rsplit(") ", 1)[1].split()[19])
    payload = {"op": "acquire_page_return", "version": 1, "pid": target["pid"],
               "start_ticks": start, "address": target["address"] + (2 * MIB if killed else 0), "bytes": 2 * MIB}
    missing = request(payload)
    assert missing["granted"] is False and "kernel mm guards" in missing["error"], missing
    reader = os.open("/proc/self/amc_mem", os.O_RDONLY | os.O_CLOEXEC)
    memory = os.open(f"/proc/{target['pid']}/amc_mem", os.O_RDONLY | os.O_CLOEXEC)
    duplicate = os.dup(memory)
    os.close(memory)
    memory = duplicate
    target_proof, reader_proof = fdinfo(memory), fdinfo(reader)
    assert int(target_proof["amc_guard_version"]) == int(reader_proof["amc_guard_version"]) == 1
    assert int(target_proof["amc_guard_memcg_ino"]) == (ROOT / "backed").stat().st_ino
    before = [int(target_proof["amc_swapin_direct_pages"]), int(reader_proof["amc_swapin_cache_pages"])]
    reply = request(payload)
    assert reply["error"] is None and reply["granted"], reply
    READY.write_text(json.dumps({"helperPid": os.getpid(), "targetPid": target["pid"], "duplicateGuardHeld": True}))
    wait(PROCEED.exists)
    assert not killed, "killed helper must never be released to read"
    for offset in range(0, 2 * MIB, mmap.PAGESIZE):
        assert len(os.pread(memory, mmap.PAGESIZE, target["address"] + offset)) == mmap.PAGESIZE
    settled = request({"op": "finish_page_return", "version": 1})
    assert settled["error"] is None and settled["granted"] and settled["resident_bytes"] == 2 * MIB, settled
    charged = {}
    with open(f"/proc/{target['pid']}/pagemap", "rb", buffering=0) as pagemap, open("/proc/kpagecgroup", "rb", buffering=0) as owners:
        for offset in range(0, 2 * MIB, mmap.PAGESIZE):
            pte = int.from_bytes(os.pread(pagemap.fileno(), 8, (target["address"] + offset) // mmap.PAGESIZE * 8), sys.byteorder)
            assert pte & ((1 << 63) | (1 << 62)) == 1 << 63
            pfn = pte & ((1 << 55) - 1)
            assert pfn > 0
            inode = int.from_bytes(os.pread(owners.fileno(), 8, pfn * 8), sys.byteorder)
            charged[str(inode)] = charged.get(str(inode), 0) + mmap.PAGESIZE
    proof = {"schemaVersion": 1, "missingGuardDenied": True, "duplicateGuardHeld": True,
             "residentBytes": settled["resident_bytes"], "chargedBytesByInode": charged,
             "targetInode": (ROOT / "backed").stat().st_ino,
             "readerInode": Path("/sys/fs/cgroup/system.slice/page-return.service").stat().st_ino,
             "directBytes": (int(fdinfo(memory)["amc_swapin_direct_pages"]) - before[0]) * mmap.PAGESIZE,
             "cacheBytes": (int(fdinfo(reader)["amc_swapin_cache_pages"]) - before[1]) * mmap.PAGESIZE}
    events = dict(line.split() for line in Path("/sys/fs/cgroup/system.slice/page-return.service/memory.events").read_text().splitlines())
    assert events["oom"] == events["oom_kill"] == "0", events
    proof["oom"] = proof["oomKill"] = 0
    RESULT.write_text(json.dumps(proof))
    os.close(memory)
    os.close(reader)


def denied_migration(pid):
    try:
        (ROOT / "unbacked" / "cgroup.procs").write_text(str(pid))
    except OSError as error:
        assert error.errno == errno.EBUSY, error
        return
    raise AssertionError("post-grant migration bypassed the kernel guard")


def run_helper(killed=False):
    for path in [READY, PROCEED, RESULT]:
        path.unlink(missing_ok=True)
    wait(lambda: request({"op": "status", "version": 1}).get("recovery") is None)
    subprocess.run(["systemd-run", "--collect", "--unit=page-return", "--property=MemoryMax=128M",
                    "--property=MemorySwapMax=0", "--property=RuntimeMaxSec=45", "--", "python3", __file__,
                    "--helper-kill" if killed else "--helper"], check=True)
    wait(READY.exists)
    ready = json.loads(READY.read_text())
    denied_migration(ready["targetPid"])
    denied_migration(ready["helperPid"])
    # The helper-owned descriptors protect the whole read interval even when
    # the broker loses every descriptor and replays its durable lease.
    subprocess.run(["systemctl", "restart", "guard-owner-host"], check=True)
    wait(host_ready)
    replayed = request({"op": "status", "version": 1})["recovery"]
    assert replayed["identity"]["pid"] == ready["helperPid"] and replayed["return_bytes"] == 2 * MIB, replayed
    assert replayed["action"]["guards"] is not None, replayed
    denied_migration(ready["targetPid"])
    denied_migration(ready["helperPid"])
    if killed:
        os.kill(ready["helperPid"], signal.SIGKILL)
        wait(lambda: request({"op": "status", "version": 1}).get("recovery") is None)
        (ROOT / "released" / "cgroup.procs").write_text(str(ready["targetPid"]))
        (ROOT / "backed" / "cgroup.procs").write_text(str(ready["targetPid"]))
        return True
    PROCEED.touch()
    wait(RESULT.exists)
    wait(lambda: request({"op": "status", "version": 1}).get("recovery") is None)
    result = json.loads(RESULT.read_text())
    result.update({"targetMigrationDeniedAfterGrant": True, "readerMigrationDeniedAfterGrant": True,
                   "brokerRestartProtected": True})
    return result


def prove_path(path):
    ROOT.mkdir()
    (ROOT / "cgroup.subtree_control").write_text("+memory")
    for name in ["original", "backed", "unbacked", "released"]:
        group = ROOT / name
        group.mkdir()
        (group / "memory.max").write_text(str(1 if name == "unbacked" else 128 * MIB))
        (group / "memory.swap.max").write_text(str(128 * MIB))
    ready_read, ready_write = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready_read)
        (ROOT / "original" / "cgroup.procs").write_text(str(os.getpid()))
        memory = mmap.mmap(-1, 4 * MIB, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
        address = ctypes.addressof(ctypes.c_char.from_buffer(memory))
        libc = ctypes.CDLL(None, use_errno=True)
        libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
        assert libc.madvise(address, len(memory), 15) == 0  # MADV_NOHUGEPAGE
        for offset in range(0, len(memory), mmap.PAGESIZE):
            memory[offset] = 7
        assert libc.madvise(address, len(memory), 21) == 0  # MADV_PAGEOUT
        META.write_text(json.dumps({"pid": os.getpid(), "address": address}))
        os.write(ready_write, b"ready")
        while True:
            time.sleep(1)
    os.close(ready_write)
    try:
        assert os.read(ready_read, 5) == b"ready"
        try:
            (ROOT / "original" / "memory.reclaim").write_text(f"{8 * MIB} swappiness=max")
        except OSError as error:
            if error.errno != errno.EAGAIN:
                raise
        def nonresident():
            stat = dict(line.split() for line in (ROOT / "original" / "memory.stat").read_text().splitlines())
            return int((ROOT / "original" / "memory.swap.current").read_text()) >= 4 * MIB and int(stat["swapcached"]) == 0
        wait(nonresident)
        (ROOT / "backed" / "cgroup.procs").write_text(str(pid))
        (ROOT / "backed" / "cgroup.freeze").write_text("1")
        wait(lambda: "frozen 1" in (ROOT / "backed" / "cgroup.events").read_text())
        (ROOT / "original").rmdir()
        policy = json.loads(Path("/etc/amc-test-host-policy.json").read_text())
        policy["swap_recovery"]["page_cgroups"] = ["/amc-guard-proof"]
        Path("/tmp/guard-policy.json").write_text(json.dumps(policy))
        subprocess.run(["systemd-run", "--unit=guard-owner-host", "--", "amc", "admission", "host-serve",
                        "--policy", "/tmp/guard-policy.json", "--socket", SOCKET, "--state", f"/var/lib/amc-guard-{path}"], check=True)
        wait(host_ready)
        result = run_helper()
        expected = "directBytes" if path == "direct" else "cacheBytes"
        other = "cacheBytes" if path == "direct" else "directBytes"
        assert result[expected] == 2 * MIB and result[other] == 0, result
        inode = result["targetInode"] if path == "direct" else result["readerInode"]
        assert result["chargedBytesByInode"] == {str(inode): 2 * MIB}, result
        result["helperLossReleased"] = run_helper(killed=True)
        return result
    finally:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        os.close(ready_read)
        subprocess.run(["systemctl", "stop", "guard-owner-host", "page-return"], check=False)
        for name in ["original", "backed", "unbacked", "released"]:
            if (ROOT / name).exists():
                (ROOT / name).rmdir()
        ROOT.rmdir()


def proof():
    receipt = {"cache": prove_path("cache")}
    subprocess.run(["modprobe", "zram"], check=True)
    Path("/sys/block/zram0/disksize").write_text(str(128 * MIB))
    subprocess.run(["mkswap", "/dev/zram0"], check=True)
    subprocess.run(["swapon", "--priority", "100", "/dev/zram0"], check=True)
    try:
        receipt["direct"] = prove_path("direct")
        lifetime = subprocess.run(["amc-guard-lifetime-proof"], check=False, capture_output=True, text=True, timeout=100)
        if lifetime.returncode != 0:
            print(lifetime.stderr, file=sys.stderr)
        lifetime.check_returncode()
        receipt["lifetime"] = json.loads(lifetime.stdout)
    finally:
        subprocess.run(["swapoff", "/dev/zram0"], check=True)
        Path("/sys/block/zram0/reset").write_text("1")
    Path("/tmp/page-return-guard-evidence.json").write_text(json.dumps(receipt))


if sys.argv[1:] in (["--helper"], ["--helper-kill"]):
    helper(sys.argv[1] == "--helper-kill")
else:
    proof()
