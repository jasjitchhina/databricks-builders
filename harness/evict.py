#!/usr/bin/env python3
"""evict.py [--stat] PATH... — drop every regular file under PATH from the page cache
(posix_fadvise DONTNEED; no root needed). --stat only reports how much is resident (mincore)."""
import ctypes, mmap, os, sys

libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p]
PAGE = os.sysconf('SC_PAGE_SIZE')


def resident(fd, size):
    if size == 0:
        return 0
    addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
    if addr in (None, ctypes.c_void_p(-1).value):
        return 0
    try:
        n = (size + PAGE - 1) // PAGE
        vec = ctypes.create_string_buffer(n)
        if libc.mincore(ctypes.c_void_p(addr), size, vec) != 0:
            return 0
        return sum(1 for x in vec.raw if x & 1) * PAGE
    finally:
        libc.munmap(ctypes.c_void_p(addr), size)


def files(paths):
    for p in paths:
        if os.path.isfile(p):
            yield p
        for dp, _, fs in os.walk(p):
            for f in fs:
                yield os.path.join(dp, f)


stat = sys.argv[1:2] == ['--stat']
n = tot = before = after = 0
for p in files(sys.argv[2:] if stat else sys.argv[1:]):
    try:
        fd = os.open(p, os.O_RDONLY)
    except OSError:
        continue
    try:
        sz = os.fstat(fd).st_size
        before += resident(fd, sz)
        if not stat:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            after += resident(fd, sz)
        n += 1; tot += sz
    finally:
        os.close(fd)
G = 2 ** 30
if stat:
    print(f"{n} files {tot/G:.2f} GiB, resident {before/G:.2f} GiB", file=sys.stderr)
else:
    print(f"evicted {n} files {tot/G:.2f} GiB: resident {before/G:.2f} -> {after/G:.2f} GiB", file=sys.stderr)
