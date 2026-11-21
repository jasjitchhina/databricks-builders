#!/usr/bin/env python3
"""cmp_oracle.py DATA_DIR ORACLE_DUMP FAST_DUMP: diff h3 clean stage vs oracle dumps."""
import sys
data, od, fd = sys.argv[1:4]
def unq(f):
    return f[1:-1] if len(f) >= 2 and f[:1] == b'"' and f[-1:] == b'"' else f
prod = {}
with open(f"{data}/products.csv", "rb") as f:
    next(f)
    for line in f:
        a = line.rstrip(b"\n").split(b",")
        prod[unq(a[0])] = unq(a[1])
def rows(p):
    with open(p, "rb") as f:
        next(f)
        return [l.rstrip(b"\n") for l in f]
ev = []
persons = set()
for l in rows(f"{od}/events_clean.csv"):
    eid, person, ts, typ, pid, price = l.split(b",")
    persons.add(person)
    if typ == b"purchase":
        ev.append(b",".join([person, ts, price, prod.get(pid, b"unknown")]))
    else:
        ev.append(b",".join([person, ts, b"0", b""]))
cl = []
for l in rows(f"{od}/clicks_clean.csv"):
    cid, person, camp, ts = l.split(b",")
    if person in persons:
        cl.append(l)
ok = True
for name, mine, theirs in [("events", sorted(open(f"{fd}/events_fast.csv","rb").read().splitlines()), sorted(ev)),
                           ("clicks", sorted(open(f"{fd}/clicks_fast.csv","rb").read().splitlines()), sorted(cl))]:
    a, b = set(mine), set(theirs)
    if mine == theirs:
        print(f"{name}: MATCH ({len(mine)} rows)")
    else:
        ok = False
        print(f"{name}: DIFF mine={len(mine)} oracle={len(theirs)} only_mine={len(a-b)} only_oracle={len(b-a)}")
        for x in list(a - b)[:5]: print("  mine  :", x)
        for x in list(b - a)[:5]: print("  oracle:", x)
sys.exit(0 if ok else 1)
