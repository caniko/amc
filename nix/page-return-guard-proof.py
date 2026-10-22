"""Disposable VM: force migration AFTER grant on direct and swap-cache paths."""

import builtins
import ctypes
import errno
import json
import logging
import mmap
import os
import signal
import socket
import stat
import subprocess
import sys
import time
from contextlib import contextmanager
from itertools import islice
from pathlib import Path

MIB = 1048576
ROOT = Path("/sys/fs/cgroup/amc-guard-proof")
SOCKET = "/run/amc-guard/admission.sock"
META = Path("/tmp/guard-target.json")
READY = Path("/tmp/guard-granted.json")
PROCEED = Path("/tmp/guard-read")
RESULT = Path("/tmp/guard-result.json")
logger = logging.getLogger(__name__)


def cleanup_diagnostics(phase):
    """Bounded, read-only observations; failures must not replace the cause."""
    report = {"phase": phase}
    try:
        report["swaps"] = Path("/proc/swaps").read_text()[:4096]
        report["target"] = META.read_text()[:512] if META.exists() else None
        if report["target"] is not None:
            target = Path("/proc") / str(json.loads(report["target"])["pid"]) / "stat"
            report["targetState"] = target.read_text()[:512] if target.exists() else "exited"
        report["services"] = subprocess.check_output(
            ["systemctl", "show", "guard-owner-host", "page-return", "dev-zram0.swap",
             "--property=Id,LoadState,ActiveState,MainPID,ControlPID,Result"],
            text=True, timeout=2,
        )[:4096]
        report["deviceHolders"] = [p.name for p in islice(Path("/sys/block/zram0/holders").iterdir(), 32)]
        device = Path("/dev/zram0").stat().st_rdev
        handles = report["openHandles"] = []
        deadline = time.monotonic() + 2
        for process in islice((p for p in Path("/proc").iterdir() if p.name.isdecimal()), 512):
            if time.monotonic() >= deadline or len(handles) >= 32:
                break
            try:
                for fd in islice((process / "fd").iterdir(), 128):
                    if time.monotonic() >= deadline or len(handles) >= 32:
                        break
                    try:
                        metadata = fd.stat()
                        link = os.readlink(fd)
                        if (stat.S_ISBLK(metadata.st_mode) and metadata.st_rdev == device) or link.endswith("/amc_mem"):
                            handles.append({"pid": process.name, "fd": fd.name, "path": link[:256],
                                            "comm": (process / "comm").read_text()[:128],
                                            "state": (process / "stat").read_text()[:512],
                                            "cgroup": (process / "cgroup").read_text()[:512]})
                    except (FileNotFoundError, ProcessLookupError):
                        continue  # An observed process/descriptor exited during sampling.
            except (FileNotFoundError, ProcessLookupError):
                continue
    except Exception as error:
        logger.exception("guard fixture diagnostics failed in phase %s", phase)
        report["diagnosticError"] = f"{type(error).__name__}: {error}"
    return json.dumps(report, sort_keys=True)


@contextmanager
def cleanup_scope(phase, steps):
    """Unwind only acquired fixture resources and retain every failure."""
    owner = os.getpid()
    primary = None
    try:
        yield
    except BaseException as error:
        if os.getpid() != owner:
            raise  # A forked target must never clean up its parent's resources.
        primary = error
        primary.add_note(f"guard proof phase={phase}: primary failure")
        if isinstance(primary, (subprocess.CalledProcessError, subprocess.TimeoutExpired)):
            primary.add_note(f"stdout={str(primary.stdout)[:4096]}; stderr={str(primary.stderr)[:4096]}")
    failures = []
    for name, action in reversed(list(steps.items())):
        try:
            action()
        except Exception as error:
            logger.exception("guard fixture cleanup failed: phase=%s step=%s", phase, name)
            error.add_note(f"guard proof phase={phase}: cleanup step={name}")
            error.add_note(cleanup_diagnostics(f"{phase}/{name}"))
            failures.append(error)
    if failures:
        raise builtins.BaseExceptionGroup("guard proof primary/cleanup failures", ([primary] if primary else []) + failures)
    if primary is not None:
        raise primary


def service_state(unit):
    text = subprocess.check_output(
        ["systemctl", "show", unit, "--property=LoadState,ActiveState,MainPID,ControlPID,Result,ExecMainStatus"],
        text=True, timeout=5,
    )
    return dict(line.split("=", 1) for line in text.splitlines())


def stop_fixture_service(unit):
    state = service_state(unit)
    if state["LoadState"] != "not-found":
        subprocess.run(["systemctl", "stop", unit], check=True, timeout=10)
        state = service_state(unit)
    assert state["ActiveState"] in ("inactive", "failed") and state["MainPID"] == state["ControlPID"] == "0", state
    events = Path("/sys/fs/cgroup/system.slice") / unit / "cgroup.events"
    assert not events.exists() or "populated 0" in events.read_text().splitlines(), (unit, state)
    # A deliberately killed transient unit stays loaded in the failed state.
    # Its outcome has been checked above/by the caller; release its name before
    # the next proof starts a fresh helper or broker with the same unit name.
    if state["ActiveState"] == "failed":
        subprocess.run(["systemctl", "reset-failed", unit], check=True, timeout=10)
    wait(lambda: service_state(unit)["LoadState"] == "not-found")


def reap_target(pid):
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass  # Still waitpid: an already-exited child must be reaped.
    reaped, _status = os.waitpid(pid, 0)
    assert reaped == pid


def reset_zram(timeout=5):
    assert all(line.split()[0] != "/dev/zram0" for line in Path("/proc/swaps").read_text().splitlines()[1:]), "zram0 is still active swap"
    deadline = time.monotonic() + timeout
    first_busy = None
    while True:
        try:
            # Unbuffered write avoids a second flush/close error hiding the first.
            with Path("/sys/block/zram0/reset").open("wb", buffering=0) as reset:
                assert reset.write(b"1") == 1
        except OSError as error:
            if error.errno != errno.EBUSY:
                error.add_note(cleanup_diagnostics("zram-reset-error"))
                raise
            if first_busy is None:
                first_busy = cleanup_diagnostics("zram-reset-busy")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                error.add_note(f"zram reset EBUSY exhausted {timeout}s bound; first observation: {first_busy}")
                error.add_note(cleanup_diagnostics("zram-reset-exhausted"))
                raise
            time.sleep(min(0.05, remaining))  # Retry only the failed reset, until its deadline.
        else:
            if first_busy is not None:
                print("zram reset recovered: " + first_busy, file=sys.stderr)
            assert Path("/sys/block/zram0/disksize").read_text().strip() == "0"
            return


def deactivate_zram():
    subprocess.run(["swapoff", "/dev/zram0"], check=True, timeout=10)
    assert all(line.split()[0] != "/dev/zram0" for line in Path("/proc/swaps").read_text().splitlines()[1:]), "swapoff left zram0 active"


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


def stale_helper():
    target = json.loads(META.read_text())
    start = int(Path(f"/proc/{target['pid']}/stat").read_text().rsplit(") ", 1)[1].split()[19])
    reader = os.open("/proc/self/amc_mem", os.O_RDONLY | os.O_CLOEXEC)
    memory = os.open(f"/proc/{target['pid']}/amc_mem", os.O_RDONLY | os.O_CLOEXEC)
    try:
        reply = request({"op": "acquire_page_return", "version": 1, "pid": target["pid"],
                         "start_ticks": start, "address": target["address"], "bytes": 2 * MIB})
        assert reply["granted"] is False and "native recovery demand or frontier" in reply["error"], reply
        RESULT.write_text(json.dumps(reply))
    finally:
        os.close(memory)
        os.close(reader)


def deny_changed_owner(steps):
    observed = Path("/run/amc-host/amc-page-inventory-observed")
    observed.unlink(missing_ok=True)
    RESULT.unlink(missing_ok=True)
    Path("/run/amc-host/amc-page-inventory-pause").touch()
    subprocess.run(["systemd-run", "--unit=page-return", "--property=MemoryMax=128M",
                      "--property=MemorySwapMax=0", "--property=RemainAfterExit=yes", "--", "python3", __file__, "--helper-stale"], check=True)
    steps["helper service"] = lambda: stop_fixture_service("page-return.service")
    wait(observed.exists)
    original = ROOT / "original"
    (original / "memory.max").write_text((original / "memory.current").read_text().strip())
    wait(RESULT.exists)
    assert request({"op": "status", "version": 1}).get("recovery") is None
    # Retain the completed helper until its exit status and explicit cleanup
    # are verified, just as in the whole-device stale-demand fixture.
    wait(lambda: subprocess.check_output(["systemctl", "show", "page-return", "--property=SubState", "--value"], text=True).strip() == "exited")
    result = subprocess.check_output(["systemctl", "show", "page-return", "--property=Result,ExecMainStatus"], text=True)
    assert dict(line.split("=", 1) for line in result.splitlines()) == {"Result": "success", "ExecMainStatus": "0"}, result
    subprocess.run(["systemctl", "stop", "page-return"], check=True)
    (original / "memory.max").write_text(str(128 * MIB))
    RESULT.unlink()
    return True


def run_helper(steps, killed=False):
    for path in [READY, PROCEED, RESULT]:
        path.unlink(missing_ok=True)
    wait(lambda: request({"op": "status", "version": 1}).get("recovery") is None)
    subprocess.run(["systemd-run", "--unit=page-return", "--property=MemoryMax=128M",
                    "--property=MemorySwapMax=0", "--property=RuntimeMaxSec=45", "--property=RemainAfterExit=yes", "--", "python3", __file__,
                    "--helper-kill" if killed else "--helper"], check=True)
    steps["helper service"] = lambda: stop_fixture_service("page-return.service")
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
        wait(lambda: service_state("page-return.service")["MainPID"] == "0")
        exited = service_state("page-return.service")
        assert exited["MainPID"] == "0" and exited["Result"] == "signal" and exited["ExecMainStatus"] == str(signal.SIGKILL), exited
        stop_fixture_service("page-return.service")
        (ROOT / "released" / "cgroup.procs").write_text(str(ready["targetPid"]))
        (ROOT / "backed" / "cgroup.procs").write_text(str(ready["targetPid"]))
        return True
    PROCEED.touch()
    wait(RESULT.exists)
    wait(lambda: request({"op": "status", "version": 1}).get("recovery") is None)
    wait(lambda: service_state("page-return.service")["MainPID"] == "0")
    exited = service_state("page-return.service")
    assert exited["MainPID"] == "0" and exited["Result"] == "success" and exited["ExecMainStatus"] == "0", exited
    stop_fixture_service("page-return.service")
    result = json.loads(RESULT.read_text())
    result.update({"targetMigrationDeniedAfterGrant": True, "readerMigrationDeniedAfterGrant": True,
                   "brokerRestartProtected": True})
    return result


def remove_group(group):
    if group.exists():
        group.rmdir()


def cleanup_lifetime():
    root = Path("/sys/fs/cgroup/amc-guard-lifetime")
    if not root.exists():
        return
    # The native proof may exit before it reaps its reparented CLONE_VM child.
    # This root was absent before our invocation; never kill outside its tree.
    steps = {"lifetime root": root.rmdir}
    for name in ["original", "backed", "outside", "released"]:
        steps[f"lifetime {name}"] = lambda group=root / name: remove_group(group)
    with cleanup_scope("lifetime descendants", steps):
        (root / "cgroup.kill").write_text("1")
        wait(lambda: "populated 0" in (root / "cgroup.events").read_text().splitlines())


def prove_path(path):
    steps = {}
    with cleanup_scope(path, steps):
        ROOT.mkdir()
        steps["root cgroup"] = ROOT.rmdir
        (ROOT / "cgroup.subtree_control").write_text("+memory")
        for name in ["original", "backed", "unbacked", "released"]:
            group = ROOT / name
            group.mkdir()
            steps[f"cgroup {name}"] = lambda group=group: remove_group(group)
            (group / "memory.max").write_text(str(1 if name == "unbacked" else 128 * MIB))
            (group / "memory.swap.max").write_text(str(128 * MIB))
        ready_read, ready_write = os.pipe()
        steps["ready read"] = lambda: os.close(ready_read)
        steps["ready write"] = lambda: os.close(ready_write)
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
        steps["target process"] = lambda: reap_target(pid)
        os.close(ready_write)
        del steps["ready write"]
        assert os.read(ready_read, 5) == b"ready"
        # Keep the Python target from refaulting its interpreter/stack during
        # reclaim, including across migration into the eventual guarded group.
        (ROOT / "original" / "cgroup.freeze").write_text("1")
        wait(lambda: "frozen 1" in (ROOT / "original" / "cgroup.events").read_text().splitlines())
        (ROOT / "backed" / "cgroup.freeze").write_text("1")
        def nonresident():
            # Swap writeback completes asynchronously, so one reclaim pass can
            # leave pages in the swap cache. Reclaim again until none remain.
            try:
                (ROOT / "original" / "memory.reclaim").write_text(f"{8 * MIB} swappiness=max")
            except OSError as error:
                if error.errno != errno.EAGAIN:
                    raise
            stat = dict(line.split() for line in (ROOT / "original" / "memory.stat").read_text().splitlines())
            return int((ROOT / "original" / "memory.swap.current").read_text()) >= 4 * MIB and int(stat["swapcached"]) == 0
        wait(nonresident)
        (ROOT / "backed" / "cgroup.procs").write_text(str(pid))
        wait(lambda: "frozen 1" in (ROOT / "backed" / "cgroup.events").read_text())
        policy = json.loads(Path("/etc/amc-test-host-policy.json").read_text())
        policy["swap_recovery"]["page_cgroups"] = ["/amc-guard-proof"]
        Path("/tmp/guard-policy.json").write_text(json.dumps(policy))
        # The public launcher prepends real systemd, masking the post-scan
        # observer. Use the same native engine as the other inventory fixtures.
        subprocess.run(["systemd-run", "--unit=guard-owner-host", "--setenv=PATH=/etc/amc-test-observer/bin:/run/current-system/sw/bin", "--", "/etc/amc-fixture-native-engine", "admission", "host-serve",
                        "--policy", "/tmp/guard-policy.json", "--socket", SOCKET, "--state", f"/var/lib/amc-guard-{path}"], check=True)
        steps["broker service"] = lambda: stop_fixture_service("guard-owner-host.service")
        wait(host_ready)
        stale_denied = deny_changed_owner(steps)
        (ROOT / "original").rmdir()
        result = run_helper(steps)
        result["postScanChargeOwnerDenied"] = stale_denied
        expected = "directBytes" if path == "direct" else "cacheBytes"
        other = "cacheBytes" if path == "direct" else "directBytes"
        assert result[expected] == 2 * MIB and result[other] == 0, result
        inode = result["targetInode"] if path == "direct" else result["readerInode"]
        assert result["chargedBytesByInode"] == {str(inode): 2 * MIB}, result
        result["helperLossReleased"] = run_helper(steps, killed=True)
        return result


def proof():
    receipt = {"cache": prove_path("cache")}
    subprocess.run(["modprobe", "zram"], check=True)
    # Refuse an existing device before acquiring it; cleanup owns only this setup.
    assert Path("/sys/block/zram0/disksize").read_text().strip() == "0"
    steps = {}
    with cleanup_scope("direct device", steps):
        Path("/sys/block/zram0/disksize").write_text(str(128 * MIB))
        steps["zram reset"] = reset_zram
        subprocess.run(["mkswap", "/dev/zram0"], check=True)
        subprocess.run(["swapon", "--priority", "100", "/dev/zram0"], check=True)
        steps["swapoff"] = deactivate_zram
        receipt["direct"] = prove_path("direct")
        assert not Path("/sys/fs/cgroup/amc-guard-lifetime").exists(), "lifetime fixture already exists"
        steps["lifetime descendants"] = cleanup_lifetime
        lifetime = subprocess.run(["amc-guard-lifetime-proof"], check=True, capture_output=True, text=True, timeout=100)
        receipt["lifetime"] = json.loads(lifetime.stdout)
    Path("/tmp/page-return-guard-evidence.json").write_text(json.dumps(receipt))


if __name__ == "__main__":
    if sys.argv[1:] == ["--helper-stale"]:
        stale_helper()
    elif sys.argv[1:] in (["--helper"], ["--helper-kill"]):
        helper(sys.argv[1] == "--helper-kill")
    else:
        proof()
