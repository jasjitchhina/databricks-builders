#!/usr/bin/env python3
"""Mutation testing of rule-critical logic in fast/ (h1, task: mutation testing).
Usage: mutate.py BASE_REV [MUTANT_ID ...]
Each mutant = one exact-string replacement in a pristine `git archive BASE_REV fast` copy. It is
built (shared incremental target dir) and attacked in tiers, cheapest first; the first failing tier
KILLS it:  T0 cargo test --lib (unit tests: kernels, differential fuzz vs scalar/spec) | T1 36 fixtures |
T2 every 1M/adv set with an oracle answer | T3 300 fuzz seeds | T4 20M sets. MUT_NO_T0=1 skips T0.
Builds and tests take the shared bench lock (flock -s /tmp/bench.lock).
Results append to $MUT_LOG (default ~/data/scratch/mutants.tsv). Run under taskset -c 8-15,24-31."""
import os
import shutil
import subprocess
import sys

HOME = os.path.expanduser("~")
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
W = os.path.join(HOME, "data/scratch/mut")
LOG = os.environ.get("MUT_LOG", os.path.join(HOME, "data/scratch/mutants.tsv"))

# (id, file, old, new, occurrence) -- occurrence: which match to replace (0 = must be unique)
M = [
    # sessions (rules: gap > 30 min starts a session; equal ts share)
    ("gap_ge", "engine.rs", "if e.ts() - prev > GAP_MS {", "if e.ts() - prev >= GAP_MS {", 0),
    ("gap_const+1", "lib.rs", "GAP_MS: u32 = 1_800_000", "GAP_MS: u32 = 1_800_001", 0),
    ("gap_const-1", "lib.rs", "GAP_MS: u32 = 1_800_000", "GAP_MS: u32 = 1_799_999", 0),
    ("gap_from_start", "engine.rs", "            prev = e.ts();\n            if e.price() > 0 {", "            if e.price() > 0 {", 0),
    # attribution: clicks with ts <= start and start - ts <= 7d; latest; tie smallest click_id
    ("click_le_start", "engine.rs", "pc[hi].ts() <= start {", "pc[hi].ts() < start {", 0),
    ("click_7d_lt", "engine.rs", "while lo < hi && pc[lo].ts() < wlo {", "while lo < hi && pc[lo].ts() <= wlo {", 0),
    ("week+1", "lib.rs", "WEEK_MS: u32 = 604_800_000", "WEEK_MS: u32 = 604_800_001", 0),
    ("week-1", "lib.rs", "WEEK_MS: u32 = 604_800_000", "WEEK_MS: u32 = 604_799_999", 0),
    ("tie_largest_cid", "engine.rs", "if pc[q].cid(part) < pc[best].cid(part) {", "if pc[q].cid(part) > pc[best].cid(part) {", 0),
    ("tie_no_scan", "engine.rs", "q > lo && pc[q - 1].ts() == t", "q > hi && pc[q - 1].ts() == t", 0),
    ("credit_earliest", "engine.rs", "let t = pc[hi - 1].ts();\n            let mut best = hi - 1;\n            let mut q = hi - 1;",
     "let t = pc[lo].ts();\n            let mut best = lo;\n            let mut q = lo;", 0),
    ("purchasing_any", "engine.rs", "ca.purchasing += (rev > 0) as u32;", "ca.purchasing += (n > 1) as u32;", 0),
    # top_category: max summed price, tie byte-wise smallest
    ("top_tie_largest", "engine.rs", "self.rank[k as usize] < self.rank[b as usize]", "self.rank[k as usize] > self.rank[b as usize]", 0),
    ("top_ge", "engine.rs", "if s > sb || (s == sb", "if s >= sb || (s == sb", 0),
    ("top_min", "engine.rs", "if s > sb || (s == sb", "if s < sb || (s == sb", 0),
    # event validation
    ("ev_win_start_le", "clean.rs", "        if t < ws || t >= we {\n            break 'v;", "        if t <= ws || t >= we {\n            break 'v;", 0),
    ("ev_win_end_gt", "clean.rs", "        if t < ws || t >= we {\n            break 'v;", "        if t < ws || t > we {\n            break 'v;", 0),
    ("late_ge", "clean.rs", "ingest - t > DAY_MS {", "ingest - t >= DAY_MS {", 0),
    ("late_abs", "clean.rs", "ingest - t > DAY_MS {", "(ingest - t).abs() > DAY_MS {", 0),
    ("late_plus1s", "clean.rs", "ingest - t > DAY_MS {", "ingest - t > DAY_MS + 1000 {", 0),
    ("no_prod_check", "clean.rs", "if user.is_empty() || prod.is_empty() {\n            break 'v;", "if user.is_empty() {\n            break 'v;", 0),
    ("price_max_ge", "clean.rs", "if v > MAX_PRICE {", "if v >= MAX_PRICE {", 0),
    ("price_zero_ok", "clean.rs", "    if v == 0 {\n        None", "    if v == u64::MAX {\n        None", 0),
    ("unknown_cat0", "clean.rs", ".map_or(self.unknown_cat, |v| v as u16)", ".map_or(0, |v| v as u16)", 0),
    # dedup: smallest ingest_ts wins, before validation
    ("dedup_latest", "clean.rs", "if r.ingest < old.ingest {", "if r.ingest > old.ingest {", 0),
    ("dedup_le", "clean.rs", "if r.ingest < old.ingest {", "if r.ingest <= old.ingest {", 0),  # equivalent under rules (equal copies identical)
    # clicks
    ("cl_win_start_le", "clean.rs", "if t < cl_lo || t >= we {", "if t <= cl_lo || t >= we {", 0),
    ("cl_win_end_gt", "clean.rs", "if t < cl_lo || t >= we {", "if t < cl_lo || t > we {", 0),
    ("cl_lo_plus1", "clean.rs", "    let cl_lo = base;", "    let cl_lo = base + 1;", 0),
    ("cl_no_user_check", "clean.rs", "if cid.is_empty() || user.is_empty() {\n                        return;", "if cid.is_empty() {\n                        return;", 0),
    # links: name = byte-wise smallest; empty side dropped
    ("link_name_max", "clean.rs", "if K::less(keys[ra as usize], keys[rb as usize]) {", "if K::less(keys[rb as usize], keys[ra as usize]) {", 0),
    ("link_allow_empty", "clean.rs", "if !a.is_empty() && !b.is_empty() && a != b {", "if !a.is_empty() && a != b {", 0),
    # timestamps
    ("f2_sign", "ts.rs", "return Some(if b[19] == b'+' { t - off } else { t + off });", "return Some(if b[19] == b'+' { t + off } else { t - off });", 1),
    ("f2_oh24", "ts.rs", "if oh > 23 || om > 59 {", "if oh > 24 || om > 59 {", 1),
    ("f2_om60", "ts.rs", "if oh > 23 || om > 59 {", "if oh > 23 || om > 60 {", 1),
    ("f1_hh24", "ts.rs", "let hi = _mm_setr_epi16(99, 99, 12, 31, 23, 59, 59, 0);", "let hi = _mm_setr_epi16(99, 99, 12, 31, 24, 59, 59, 0);", 0),
    ("f1_ss60", "ts.rs", "let hi = _mm_setr_epi16(99, 99, 12, 31, 23, 59, 59, 0);", "let hi = _mm_setr_epi16(99, 99, 12, 31, 23, 59, 60, 0);", 0),
    ("f1_mm13", "ts.rs", "let hi = _mm_setr_epi16(99, 99, 12, 31, 23, 59, 59, 0);", "let hi = _mm_setr_epi16(99, 99, 13, 31, 23, 59, 59, 0);", 0),
    ("f1_mlen_ge", "ts.rs", "        if dd > t.mlen[i] as i64 {\n            return None;\n        }\n        t.days[i] as i64 + dd - 1\n    } else {\n        return finish_civil",
     "        if dd > t.mlen[i] as i64 + 1 {\n            return None;\n        }\n        t.days[i] as i64 + dd - 1\n    } else {\n        return finish_civil", 0),
    ("leap_400", "ts.rs", "y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)", "y % 4 == 0", 1),
    # CSV dialect
    ("csv_lone_quote", "csv.rs", "let w = ((n >= 2) & (first == b'\"') & (last == b'\"')) as usize;", "let w = ((n >= 1) & (first == b'\"') & (last == b'\"')) as usize;", 0),
    ("csv_stray_quote_ok", "csv.rs", "if wsum == q.count_ones() {", "if wsum <= q.count_ones() {", 0),
    ("csv_extra_fields_ok", "csv.rs", "if commas.count_ones() as usize == N - 1 {", "if commas.count_ones() as usize >= N - 1 {", 0),
]

EQUIVALENT = {"dedup_le": "equal-ingest copies are identical by rules.md, so keeping the later one is legal"}


def sh(cmd, timeout=None, **kw):
    try:
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout, **kw)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return 124, "TIMEOUT"


def oracle_sets():
    out = []
    for d in sorted(os.listdir(os.path.join(HOME, "data/oracle"))):
        if os.path.isdir(os.path.join(HOME, "data/oracle", d, "out", "user_daily")) and os.path.isdir(os.path.join(HOME, "data", d)):
            out.append(d)
    return out


def main():
    rev = sys.argv[1]
    want = set(sys.argv[2:])
    base = os.path.join(W, "base")
    work = os.path.join(W, "work")
    tgt = os.path.join(W, "target")
    if not os.path.isdir(base):
        os.makedirs(base)
        sh(f"git -C {REPO} archive {rev} fast | tar -x -C {base}")
    if not os.path.isdir(work):
        shutil.copytree(base, work)
    sets = oracle_sets()
    small = [s for s in sets if not any(x in s for x in ("20m", "60m", "100m"))]
    big = [s for s in sets if "20m" in s]
    env = f"PYTHONPATH={HOME}/pylib"
    for mid, f, old, new, occ in M:
        if want and mid not in want:
            continue
        src = os.path.join(base, "fast/src", f)
        dst = os.path.join(work, "fast/src", f)
        text = open(src).read()
        n = text.count(old)
        if (occ == 0 and n != 1) or (occ > 0 and n < occ):
            print(f"{mid}: pattern count {n} (occ {occ}) -- SKIP (fix spec)", flush=True)
            continue
        if occ == 0:
            mut = text.replace(old, new, 1)
        else:
            i = -1
            for _ in range(occ):
                i = text.index(old, i + 1)
            mut = text[:i] + new + text[i + len(old):]
        # restore every file, then mutate one
        for g in os.listdir(os.path.join(base, "fast/src")):
            p = os.path.join(base, "fast/src", g)
            if os.path.isfile(p):
                shutil.copy(p, os.path.join(work, "fast/src", g))
        open(dst, "w").write(mut)
        rc, o = sh(f"cd {work}/fast && CARGO_TARGET_DIR={tgt} flock -s /tmp/bench.lock cargo build --release --offline --bin fast 2>&1", timeout=900)
        if rc != 0:
            res, why = "BUILD_FAIL", o.strip().splitlines()[-1][:120]
        else:
            b = os.path.join(W, f"bin-{mid}")
            shutil.copy(os.path.join(tgt, "release/fast"), b)
            res, why = "SURVIVED", ""
            if not os.environ.get("MUT_NO_T0"):
                # T0: unit tests (cheapest kill for kernel mutants: ts/csv/keys differential tests)
                rc, o = sh(f"cd {work}/fast && CARGO_TARGET_DIR={tgt} flock -s /tmp/bench.lock cargo test --release --offline --lib 2>&1", timeout=1800)
                if rc != 0:
                    fails = [l.split()[1] for l in o.splitlines() if l.startswith("test ") and l.rstrip().endswith("FAILED")]
                    res, why = "KILLED_T0", ("unit " + ",".join(fails))[:120] if fails else "unit tests: " + (o.strip().splitlines() or ["?"])[-1][:100]
            if res == "SURVIVED":
                rc, o = sh(f"{env} python3 {REPO}/scripts/run_fixtures.py {REPO}/fixtures -- timeout 60 {b}", timeout=900)
            if res == "SURVIVED" and rc != 0:
                fails = [l.split()[1] for l in o.splitlines() if l.startswith("FAIL")]
                res, why = "KILLED_T1", "fixtures " + ",".join(sorted(set(fails)))[:100]
            if res == "SURVIVED":
                rc, o = sh(f"{HERE}/stress_sets.sh {b} 8 {' '.join(small)}", timeout=3600)
                if rc != 0:
                    res, why = "KILLED_T2", " ".join(l.split()[1] for l in o.splitlines() if not l.startswith("ok"))[:120]
            if res == "SURVIVED":
                fd = os.path.join(W, f"fuzz-{mid}")
                shutil.rmtree(fd, ignore_errors=True)
                rc, o = sh(f"seq 0 5 | FUZZ_DIR={fd} xargs -P 6 -I{{}} {HERE}/fuzz_target.sh 7{{}}00 7{{}}49 {b}", timeout=3600)
                nf = len(os.listdir(fd)) if os.path.isdir(fd) else 0
                if nf:
                    res, why = "KILLED_T3", f"{nf} fuzz seeds differ, e.g. {sorted(os.listdir(fd))[:3]}"
            if res == "SURVIVED":
                rc, o = sh(f"{HERE}/stress_sets.sh {b} 8 {' '.join(big)}", timeout=3600)
                if rc != 0:
                    res, why = "KILLED_T4", " ".join(l.split()[1] for l in o.splitlines() if not l.startswith("ok"))[:120]
            if res == "SURVIVED" and mid in EQUIVALENT:
                res, why = "EQUIVALENT", EQUIVALENT[mid]
            os.remove(b)
        line = f"{mid}\t{f}\t{res}\t{why}"
        print(line, flush=True)
        with open(LOG, "a") as lf:
            lf.write(line + "\n")
    # restore pristine work tree
    for g in os.listdir(os.path.join(base, "fast/src")):
        p = os.path.join(base, "fast/src", g)
        if os.path.isfile(p):
            shutil.copy(p, os.path.join(work, "fast/src", g))


if __name__ == "__main__":
    main()
