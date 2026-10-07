"""Disposable native VM target; never used on an operator workstation."""
import ctypes
import mmap
from pathlib import Path
import json
import os
import time

size = 64 * 1024 * 1024
memory = mmap.mmap(-1, size, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
for offset in range(0, size, mmap.PAGESIZE):
    memory[offset] = 7
address = ctypes.addressof(ctypes.c_char.from_buffer(memory))
libc = ctypes.CDLL(None, use_errno=True)
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
assert libc.madvise(address, size, 21) == 0  # Linux MADV_PAGEOUT
Path('/tmp/page-target-range.json').write_text(json.dumps({'pid': os.getpid(), 'address': address}))
Path('/tmp/page-target-ready').touch()
while not Path('/tmp/page-target-probe').exists():
    time.sleep(0.1)
assert all(memory[offset] == 7 for offset in range(0, size, mmap.PAGESIZE))
Path('/tmp/page-target-intact').touch()
while True:
    time.sleep(1)
