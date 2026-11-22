#!/usr/bin/env python3
"""cache_attack.py BIN [DATASET]  (h6, task 62 adversarial review of the replay cache)

Tries to make the /scratch replay cache return a wrong table. Every scenario runs BIN with a
SCRATCH_DIR that may hold a cache, and compares the output row-exact (scripts/compare.py) with a
reference made by the same BIN with FAST_NO_CACHE=1 on the same input state. Each mutation is
first checked to change the reference output (else the scenario proves nothing: reported as WEAK).
hit/miss is read from FAST_VERBOSE stderr (any line containing 'hit' / 'miss', case-insensitive);
it is informational only, correctness is what is scored.

Work dir: ~/data/scratch/h6cache (kept on failure). DATASET default ~/data/dev (copied, cp -a).
Exit 0 only if every scenario MATCHes."""
import os
import random
import shutil
import signal
import subprocess
import sys
import time

BIN = os.path.abspath(sys.argv[1])
SRC = os.path.expanduser(sys.argv[2] if len(sys.argv) > 2 else "~/data/dev")
SRC2 = os.path.expanduser(os.environ.get("DATASET2", "~/data/s2-1m"))
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CMP = os.path.join(ROOT, "scripts", "compare.py")
W = os.path.expanduser(os.environ.get("ATTACK_DIR", "~/data/scratch/h6cache"))
NCPU = os.environ.get("NCPU", "4")
IMAGE = os.environ.get("ATTACK_IMAGE", "")  # e.g. bench-rev-<sha>:latest, for read-only / tiny-tmpfs runs
shutil.rmtree(W, ignore_errors=True)
os.makedirs(W)
results = []
_n = [0]


def tmp(name):
    _n[0] += 1
    p = os.path.join(W, f"{_n[0]:03d}-{name}")
    os.makedirs(p)
    return p


def run(data, out, scratch, extra=None, timeout=600):
    env = dict(os.environ, NCPU=NCPU, FAST_VERBOSE="1")
    env.pop("FAST_NO_CACHE", None)
    if scratch is None:
        env.pop("SCRATCH_DIR", None)
    else:
        env["SCRATCH_DIR"] = scratch
    env.update(extra or {})
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    p = subprocess.run(["taskset", "-c", "8-15,24-31", BIN, data, out], env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=timeout)
    err = p.stderr.decode(errors="replace").lower()
    hm = "hit" if "hit" in err else "miss" if "miss" in err else "?"
    return p.returncode, hm, err


def ref(data, name):
    out = os.path.join(tmp("ref-" + name), "out")
    rc, _, err = run(data, out, os.path.join(W, "noscratch-unused"), {"FAST_NO_CACHE": "1"})
    assert rc == 0, f"reference run failed: {err[-500:]}"
    return out


def same(a, b):
    return subprocess.run([sys.executable, CMP, a, b], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def check(name, data, scratch, want, extra=None, expect=None):
    out = os.path.join(tmp(name), "out")
    rc, hm, err = run(data, out, scratch, extra)
    ok = rc == 0 and same(out, want)
    note = "" if expect is None or expect == hm or hm == "?" else f" (expected {expect})"
    results.append((ok, name, hm))
    print(f"{'MATCH' if ok else 'WRONG'} {name:44s} rc={rc} cache={hm}{note}", flush=True)
    if not ok:
        print("   stderr tail:", err[-400:].replace("\n", " | "))
    return ok


def clone(src, name):
    d = os.path.join(tmp(name), "data")
    subprocess.run(["cp", "-a", src, d], check=True)
    return d


def files(d):
    return sorted(os.path.join(r, f) for r, _, fs in os.walk(d) for f in fs if f.endswith(".csv"))


def poke_mid(path, field, keep_times=True, nlines=300):
    """Same-size in-place edit (same inode) of `nlines` lines spread over the part of the file that
    is outside its first and last 64 KB (or the middle third if smaller): field `field` gets its last
    char changed. Restores atime/mtime ns."""
    st = os.stat(path)
    with open(path, "r+b") as f:
        b = bytearray(f.read())
        n = len(b)
        lo, hi = (65536 + 4096, n - 65536 - 4096) if n > 3 * 65536 else (n // 3, 2 * n // 3)
        for t in range(nlines):
            i = b.index(b"\n", lo + (hi - lo) * t // nlines) + 1
            j = b.index(b"\n", i)
            if j > hi:
                break
            cols = bytes(b[i:j]).split(b",")
            if len(cols) <= field or not cols[field].strip(b'"'):
                continue
            k = sum(len(c) + 1 for c in cols[:field]) + len(cols[field]) - 1 - cols[field].endswith(b'"')
            c = b[i + k]
            b[i + k] = ord("a") if c != ord("a") else ord("b")
        f.seek(0)
        f.write(b)
    if keep_times:
        os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns))


def biggest(d, sub):
    return max(files(os.path.join(d, sub)), key=os.path.getsize)


def differs(a, b, name):
    if same(a, b):
        print(f"WEAK  {name}: the mutation did not change the output, scenario proves nothing")
        results.append((True, name + " [WEAK]", "-"))
        return False
    return True


A = clone(SRC, "A")
B = clone(SRC2, "B")
refA, refB = ref(A, "A"), ref(B, "B")
assert not same(refA, refB)

# 1. plain miss then hit, then hit with another NCPU
s = tmp("scr")
check("A fresh scratch (miss)", A, s, refA, expect="miss")
check("A replay (hit)", A, s, refA, expect="hit")
check("A replay NCPU=1", A, s, refA, {"NCPU": "1"}, expect="hit")
# 2. scratch from A, then B; then A again (B evicted A?)
check("B after A, same scratch (miss)", B, s, refB, expect="miss")
check("B replay", B, s, refB, expect="hit")
check("A after B (miss)", A, s, refA)

# 3. same path, dataset swapped underneath (mv away, other dataset moved in)
s = tmp("scr")
P = os.path.join(tmp("swap"), "data")
shutil.copytree(A, P, copy_function=shutil.copy2)
check("swap: A at P", P, s, refA)
shutil.rmtree(P)
subprocess.run(["cp", "-a", B, P], check=True)
check("swap: B at same path P", P, s, refB, expect="miss")

# 4. in-place same-size edits in the MIDDLE of files (outside first/last 64 KB), mtime restored
for sub, field, label in [("events", 1, "events user_id"), ("clicks", 2, "clicks campaign_id"),
                          ("links", 1, "links user_id_b")]:
    s = tmp("scr")
    D = clone(SRC, "mid-" + sub)
    check(f"mid {label}: fill", D, s, refA)
    f = biggest(D, sub)
    poke_mid(f, field)
    r = ref(D, "mid-" + sub)
    if differs(r, refA, f"mid {label}"):
        check(f"mid {label}: same size+mtime, changed", D, s, r, expect="miss")
# products.csv is > 128 KB in generator data: mid-file category change
s = tmp("scr")
D = clone(SRC, "mid-products")
check("mid products: fill", D, s, refA)
poke_mid(os.path.join(D, "products.csv"), 1)
r = ref(D, "mid-products")
if differs(r, refA, "mid products.csv category"):
    check("mid products.csv category changed", D, s, r, expect="miss")
# manifest: same size, window shifted by one day, mtime restored
s = tmp("scr")
D = clone(SRC, "manifest")
check("manifest: fill", D, s, refA)
mp = os.path.join(D, "manifest.json")
st = os.stat(mp)
t = open(mp).read()
import json, datetime as dt  # noqa: E401,E402
m = json.loads(t)
ws = dt.datetime.strptime(m["window_start"], "%Y-%m-%dT%H:%M:%SZ") + dt.timedelta(days=1)
t2 = t.replace(m["window_start"], ws.strftime("%Y-%m-%dT%H:%M:%SZ"))
assert len(t2) == len(t)
open(mp, "w").write(t2)
os.utime(mp, ns=(st.st_atime_ns, st.st_mtime_ns))
r = ref(D, "manifest")
if differs(r, refA, "manifest window +1 day"):
    check("manifest window +1d, same size+mtime", D, s, r, expect="miss")

# 5. symlinked inputs: the link stays, the TARGET changes (mid-file, mtime restored)
s = tmp("scr")
D = clone(SRC, "symlink")
ext = tmp("symtarget")
f = biggest(D, "events")
tgt = os.path.join(ext, os.path.basename(f))
shutil.move(f, tgt)
os.symlink(tgt, f)
check("symlinked events file: fill", D, s, refA)
poke_mid(tgt, 1)
r = ref(D, "symlink")
if differs(r, refA, "symlink target"):
    check("symlink target changed mid-file", D, s, r, expect="miss")
# a symlinked DIRECTORY (events -> elsewhere), then retargeted to another copy with one edit
s = tmp("scr")
D = clone(SRC, "symdir")
e1 = os.path.join(tmp("ev1"), "events")
shutil.move(os.path.join(D, "events"), e1)
os.symlink(e1, os.path.join(D, "events"))
check("symlinked events dir: fill", D, s, refA)
e2 = os.path.join(tmp("ev2"), "events")
subprocess.run(["cp", "-a", e1, e2], check=True)
poke_mid(biggest(e2, "."), 1)
st = os.lstat(os.path.join(D, "events"))
os.unlink(os.path.join(D, "events"))
os.symlink(e2, os.path.join(D, "events"))
os.utime(os.path.join(D, "events"), ns=(st.st_atime_ns, st.st_mtime_ns), follow_symlinks=False)
r = ref(D, "symdir")
if differs(r, refA, "symlinked dir retarget"):
    check("events dir symlink retargeted", D, s, r, expect="miss")

# 6. files added / removed / renamed; ignored files
s = tmp("scr")
D = clone(SRC, "files")
check("files: fill", D, s, refA)
f = biggest(D, "events")
os.rename(f, f[:-4] + ".txt")  # no longer a *.csv
r = ref(D, "files-rm")
if differs(r, refA, "events file renamed to .txt"):
    check("events file renamed away (.txt)", D, s, r, expect="miss")
os.rename(f[:-4] + ".txt", f)
check("renamed back (original again)", D, s, refA)
with open(os.path.join(D, "events", "zz-extra.csv"), "w") as g:
    g.write("event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n")
check("header-only file added (same answer)", D, s, refA)
os.remove(os.path.join(D, "events", "zz-extra.csv"))
os.makedirs(os.path.join(D, "events", "sub.csv"))
shutil.copy(biggest(D, "events"), os.path.join(D, "events", "sub.csv", "x.csv"))
check("subdir named *.csv (ignored)", D, s, refA)
shutil.rmtree(os.path.join(D, "events", "sub.csv"))
# swap the contents of two same-size files? (generator sizes differ; make two equal by rename-swap)
ev = files(os.path.join(D, "events"))
a, b = ev[0], ev[1]
os.rename(a, a + ".t"); os.rename(b, a); os.rename(a + ".t", b)  # noqa: E702
check("two events files swapped names", D, s, refA)  # same multiset of lines: same answer

# 7. tampered cache contents
def cache_files(scr):
    return sorted(os.path.join(r, f) for r, _, fs in os.walk(scr) for f in fs)


for mode in ["truncate", "delete", "flip-byte-same-size", "ok-garbage", "ok-empty", "extra-file", "dir-to-file"]:
    s = tmp("scr")
    check(f"tamper {mode}: fill", A, s, refA)
    cf = [p for p in cache_files(s) if "parquet" in p or p.endswith((".bin", ".pq"))] or [p for p in cache_files(s) if not p.endswith("OK")]
    okf = [p for p in cache_files(s) if os.path.basename(p) == "OK"]
    if not cf or not okf:
        print(f"INFO  tamper {mode}: no cache files found under scratch ({len(cf)} data, {len(okf)} OK): cache write not detected")
        results.append((True, f"tamper {mode} [no cache]", "-"))
        continue
    victim = max(cf, key=os.path.getsize)
    if mode == "truncate":
        os.truncate(victim, os.path.getsize(victim) // 2)
    elif mode == "delete":
        os.remove(victim)
    elif mode == "flip-byte-same-size":
        with open(victim, "r+b") as g:
            g.seek(os.path.getsize(victim) // 2)
            x = g.read(1)
            g.seek(-1, 1)
            g.write(bytes([x[0] ^ 0x5A]))
    elif mode == "ok-garbage":
        open(okf[0], "wb").write(os.urandom(200))
    elif mode == "ok-empty":
        open(okf[0], "wb").close()
    elif mode == "extra-file":
        shutil.copy(victim, os.path.join(os.path.dirname(victim), "zz-extra-" + os.path.basename(victim)))
    elif mode == "dir-to-file":
        dd = os.path.dirname(okf[0])
        shutil.rmtree(dd)
        open(dd, "w").write("x")
    check(f"tamper {mode}", A, s, refA)
    check(f"tamper {mode}: rerun", A, s, refA)

# 8. scratch environment problems (native)
check("SCRATCH_DIR unset", A, None, refA)
check("SCRATCH_DIR missing dir", A, os.path.join(W, "does", "not", "exist"), refA)
fs_ = os.path.join(tmp("scrfile"), "f")
open(fs_, "w").write("x")
check("SCRATCH_DIR is a regular file", A, fs_, refA)
s = tmp("scr-ro")
os.chmod(s, 0o555)
check("read-only scratch dir", A, s, refA)
check("read-only scratch dir again", A, s, refA)
os.chmod(s, 0o755)
s = tmp("scr")
check("fill for ro-cache", A, s, refA)
for r_, ds, fs2 in os.walk(s):
    for x in fs2:
        os.chmod(os.path.join(r_, x), 0o000)
check("cache files unreadable (mode 000)", A, s, refA)
for r_, ds, fs2 in os.walk(s):
    for x in fs2:
        os.chmod(os.path.join(r_, x), 0o644)
# OUTPUT_DIR pre-filled with stale parquet from B
s = tmp("scr")
check("stale-out: fill A", A, s, refA)
out = os.path.join(tmp("stale"), "out")
shutil.copytree(refB, out)
env = dict(os.environ, NCPU=NCPU, SCRATCH_DIR=s, FAST_VERBOSE="1")
rc = subprocess.run([BIN, A, out], env=env, stderr=subprocess.DEVNULL).returncode
ok = rc == 0 and same(out, refA)
results.append((ok, "hit into OUTPUT_DIR holding B's parquet", "?"))
print(f"{'MATCH' if ok else 'WRONG'} hit into OUTPUT_DIR holding B's parquet rc={rc}")

# 9. different binary (build id / file changed) must not reuse: informational (same code => same answer)
s = tmp("scr")
check("bin2: fill with BIN", A, s, refA)
b2 = os.path.join(tmp("bin2"), "fast")
shutil.copy2(BIN, b2)
blob = bytearray(open(b2, "rb").read())
k = blob.find(b"GNU\x00")
if k > 0:
    blob[k + 4] ^= 0xFF  # first byte of the build-id descriptor
    open(b2, "wb").write(blob)
    BIN0, BIN = BIN, b2
    check("bin2: build-id changed (expect miss)", A, s, refA, expect="miss")
    BIN = BIN0

# 10. SIGKILL at random points while a fill / replay is running, then a clean rerun must match
R = random.Random(62)
s = tmp("scr")
t0 = time.time()
run(A, os.path.join(W, "timing-out"), os.path.join(W, "timing-scr"))
dur = time.time() - t0
for it in range(16):
    out = os.path.join(W, "kill-out")
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    env = dict(os.environ, NCPU=NCPU, SCRATCH_DIR=s)
    p = subprocess.Popen(["taskset", "-c", "8-15,24-31", BIN, A, out], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(R.uniform(0, dur * 1.3))
    p.send_signal(signal.SIGKILL)
    p.wait()
    if it % 4 == 3:
        check(f"after SIGKILL #{it}", A, s, refA)
check("after 16 SIGKILLs", A, s, refA)

# 11. concurrent runs sharing one scratch: same dataset x3, and A/B interleaved
for rnd in range(4):
    s = tmp("scr-conc")
    jobs = []
    for j, (D, rf) in enumerate([(A, refA), (A, refA), (B, refB), (A, refA), (B, refB)]):
        out = os.path.join(tmp(f"conc{rnd}-{j}"), "out")
        env = dict(os.environ, NCPU="2", SCRATCH_DIR=s)
        jobs.append((subprocess.Popen(["taskset", "-c", "8-15,24-31", BIN, D, out], env=env,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL), out, rf, j))
    for p, out, rf, j in jobs:
        rc = p.wait()
        ok = rc == 0 and same(out, rf)
        results.append((ok, f"concurrent round {rnd} job {j}", "?"))
        if not ok:
            print(f"WRONG concurrent round {rnd} job {j} rc={rc}")
    for D, rf, nm in [(A, refA, "A"), (B, refB, "B")]:
        check(f"after concurrent round {rnd}: {nm}", D, s, rf)

# 12. docker: read-only root + tiny / read-only /scratch (needs ATTACK_IMAGE with the runtime libs)
if IMAGE:
    for label, scr_opt in [("tiny tmpfs /scratch 1m", ["--tmpfs", "/scratch:rw,size=1m,mode=1777"]),
                           ("tiny tmpfs /scratch 64k", ["--tmpfs", "/scratch:rw,size=64k,mode=1777"]),
                           ("no /scratch mount", [])]:
        for rep in range(2):
            out = os.path.join(tmp("dk"), "out")
            os.chmod(os.path.dirname(out), 0o777)
            os.makedirs(out)
            os.chmod(out, 0o777)
            cmd = ["docker", "run", "--rm", "--network", "none", "--read-only", "--user", f"{os.getuid()}:{os.getgid()}",
                   "--cpuset-cpus", "8-15,24-31", "--tmpfs", "/tmp:rw,size=64m,mode=1777", *scr_opt,
                   "-v", f"{A}:/data:ro", "-v", f"{out}:/output", "-v", f"{BIN}:/w/fast:ro",
                   "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/scratch",
                   "-e", "HOME=/scratch", "-e", f"NCPU={NCPU}", "--entrypoint", "/w/fast", IMAGE]
            rc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode
            ok = rc == 0 and same(out, refA)
            results.append((ok, f"docker {label} #{rep}", "?"))
            print(f"{'MATCH' if ok else 'WRONG'} docker {label} #{rep} rc={rc}")

bad = [r for r in results if not r[0]]
print(f"\n{len(results) - len(bad)}/{len(results)} MATCH; {len(bad)} WRONG")
for r in bad:
    print("  WRONG:", r[1])
if not bad:
    shutil.rmtree(W, ignore_errors=True)
sys.exit(1 if bad else 0)
