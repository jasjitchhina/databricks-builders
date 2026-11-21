#!/usr/bin/env python3
"""check_invariants.py DATA_DIR — asserts the data guarantees rules.md makes (so answers are order-independent):
equal (event_id, ingest_ts) copies identical; click_id copies identical; ingest_ts valid F1; clean dims."""
import os, re, sys
D = sys.argv[1]
F1 = re.compile(rb'^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$', re.A)
def rows(folder, n):
    for fn in sorted(os.listdir(folder)):
        if not fn.endswith('.csv'): continue
        lines = open(os.path.join(folder, fn), 'rb').read().split(b'\n')
        if lines and lines[-1] == b'': lines.pop()
        for ln in lines[1:]:
            out = []
            for p in ln.split(b','):
                if b'"' in p:
                    if len(p) >= 2 and p[:1] == b'"' and p[-1:] == b'"' and b'"' not in p[1:-1]: out.append(p[1:-1])
                    else: out = None; break
                else: out.append(p)
            if out is not None and len(out) == n: yield out
bad = 0
seen = {}
for r in rows(os.path.join(D, 'events'), 7):
    if not r[0]: continue
    if not F1.match(r[3]): print('bad ingest_ts', r); bad += 1
    k = (r[0], r[3])
    if seen.setdefault(k, r) != r: print('equal-ingest copies differ', seen[k], r); bad += 1
seen = {}
for r in rows(os.path.join(D, 'clicks'), 4):
    if not r[0]: continue
    if seen.setdefault(r[0], r) != r: print('click copies differ', seen[r[0]], r); bad += 1
for f in ('products.csv', 'campaigns.csv'):
    ks = [l.split(b',')[0] for l in open(os.path.join(D, f), 'rb').read().split(b'\n')[1:] if l]
    if len(ks) != len(set(ks)): print('dup key in', f); bad += 1
print(('OK ' if not bad else 'VIOLATIONS %d ' % bad) + D)
sys.exit(1 if bad else 0)
