"""Disposable native VM target; never used on an operator workstation."""

import ctypes
import mmap
from pathlib import Path
import json
import os
import sys
import time


def snapshot():
    target = json.loads(Path("/tmp/page-target-range.json").read_text())
    pid, address, size = target["pid"], target["address"], target["bytes"]
    assert 0 < size <= 64 * 1024 * 1024 and size % mmap.PAGESIZE == 0
    placement = Path(f"/proc/{pid}/cgroup").read_text().strip().removeprefix("0::")
    group = Path("/sys/fs/cgroup") / placement.lstrip("/")
    for _ in range(10):
        before = int((group / "memory.swap.current").read_text())
        stats = dict(
            line.split() for line in (group / "memory.stat").read_text().splitlines()
        )
        cached = int(stats["swapcached"])
        after = int((group / "memory.swap.current").read_text())
        if before == after and cached <= after:
            break
        time.sleep(0.01)
    else:
        raise AssertionError("unstable native swap observation")
    with open(f"/proc/{pid}/pagemap", "rb", buffering=0) as pagemap:
        entries = os.pread(
            pagemap.fileno(), size // mmap.PAGESIZE * 8, address // mmap.PAGESIZE * 8
        )
    assert len(entries) == size // mmap.PAGESIZE * 8
    present = swapped = 0
    for offset in range(0, len(entries), 8):
        entry = int.from_bytes(entries[offset : offset + 8], sys.byteorder)
        present += bool(entry & (1 << 63))
        swapped += bool(entry & (1 << 62))
    # Inventory only PTE flags, never read payload bytes. This distinguishes an
    # accounting tail from swapped pages outside the known disposable mapping.
    inventory = {"privateReadable": 0, "other": 0}
    short_reads = scanned = 0
    mappings = Path(f"/proc/{pid}/maps").read_text().splitlines()
    assert len(mappings) <= 8192
    with open(f"/proc/{pid}/pagemap", "rb", buffering=0) as pagemap:
        for mapping in mappings:
            fields = mapping.split()
            first, last = (int(value, 16) for value in fields[0].split("-"))
            category = (
                "privateReadable"
                if fields[1].startswith("r") and fields[1].endswith("p")
                else "other"
            )
            for address in range(first, last, mmap.PAGESIZE * 512):
                count = min((last - address) // mmap.PAGESIZE, 512)
                scanned += count
                assert scanned <= 8388608
                entries = os.pread(
                    pagemap.fileno(), count * 8, address // mmap.PAGESIZE * 8
                )
                if len(entries) != count * 8:
                    short_reads += 1
                    continue
                inventory[category] += (
                    sum(
                        bool(
                            int.from_bytes(entries[i : i + 8], sys.byteorder)
                            & (1 << 62)
                        )
                        for i in range(0, len(entries), 8)
                    )
                    * mmap.PAGESIZE
                )
    return {
        "swapBytes": after,
        "cachedBytes": cached,
        "returnBytes": after - cached,
        "memoryCurrentBytes": int((group / "memory.current").read_text()),
        "mappingBytes": size,
        "presentBytes": present * mmap.PAGESIZE,
        "swappedBytes": swapped * mmap.PAGESIZE,
        "mmSwappedBytes": inventory,
        "pagemapShortReads": short_reads,
    }


if sys.argv[1:] == ["--snapshot"]:
    print(json.dumps(snapshot()))
    raise SystemExit(0)

size = 64 * 1024 * 1024
memory = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for offset in range(0, size, mmap.PAGESIZE):
    memory[offset] = 7
address = ctypes.addressof(ctypes.c_char.from_buffer(memory))
libc = ctypes.CDLL(None, use_errno=True)
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
assert libc.madvise(address, size, 21) == 0  # Linux MADV_PAGEOUT
Path("/tmp/page-target-range.json").write_text(
    json.dumps({"pid": os.getpid(), "address": address, "bytes": size})
)
Path("/tmp/page-target-ready").touch()
while not Path("/tmp/page-target-probe").exists():
    time.sleep(0.1)
assert all(memory[offset] == 7 for offset in range(0, size, mmap.PAGESIZE))
Path("/tmp/page-target-intact").touch()
while True:
    time.sleep(1)
