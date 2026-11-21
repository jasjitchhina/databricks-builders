#!/usr/bin/env python3
"""Tiny, very dirty random datasets that stress the edges in rules.md.
Usage: fuzz.py SEED OUT_DIR"""
import json
import os
import random
import sys
import datetime as dt

seed, out = int(sys.argv[1]), sys.argv[2]
R = random.Random(seed)
# random window start midnight 2025..2028, incl. around Feb 29 2028 sometimes
if R.random() < 0.3:
    ws = dt.datetime(2028, 2, R.randint(20, 28), tzinfo=dt.timezone.utc)
else:
    ws = dt.datetime(2025, 1, 1, tzinfo=dt.timezone.utc) + dt.timedelta(days=R.randint(0, 3 * 365))
we = ws + dt.timedelta(days=14)
WS = int(ws.timestamp() * 1000)
WE = int(we.timestamp() * 1000)
DAY, MIN = 86_400_000, 60_000

users = [f"u{i}" for i in range(R.randint(2, 12))] + ["U1", " u1", "u1 ", "x"]
prods = [f"p{i}" for i in range(6)]
cats = ["a", "b", "c", "unknown_x"]
camps = [f"c_{i}" for i in range(5)]


def iso(ms, form=None):
    form = form or R.choice(["F1", "F1", "F2", "F3", "F3"])
    if form == "F3" or ms % 1000:
        s = str(ms)
        return "0" * R.choice([0, 0, 0, 3]) + s
    t = dt.datetime.fromtimestamp(ms / 1000, dt.timezone.utc)
    if form == "F1":
        return t.strftime("%Y-%m-%dT%H:%M:%SZ")
    off = R.randint(-10 * 60, 12 * 60) // 15 * 15
    lt = t + dt.timedelta(minutes=off)
    sign = "+" if off >= 0 else "-"
    if off == 0 and R.random() < 0.5:
        sign = "-"
    return lt.strftime("%Y-%m-%dT%H:%M:%S") + f"{sign}{abs(off)//60:02d}:{abs(off)%60:02d}"


def bad_ts(ms):
    t = dt.datetime.fromtimestamp(ms // 1000, dt.timezone.utc)
    return R.choice([
        "", t.strftime("%Y-%m-%d %H:%M:%SZ"), t.strftime("%Y-%m-%dT%H:%M:%S.123Z"), t.strftime("%Y-%m-%dt%H:%M:%Sz"),
        t.strftime("%Y-%m-%dT%H:%M:%S"), t.strftime("%Y-%m-%dT%H:%M:%S+0200"), t.strftime("%Y-%m-%dT%H:%M:%S+24:00"),
        t.strftime("%Y-%m-%dT%H:%M:%S-05:60"), t.strftime("%Y-13-%dT%H:%M:%SZ"), t.strftime("%Y-02-30T%H:%M:%SZ"),
        "2027-02-29T10:00:00Z", "2028-02-29T10:00:00Z", t.strftime("%Y-%m-%dT24:%M:%SZ"),
        t.strftime("%Y-%m-%dT%H:%M:60Z"), f"{ms}.0", "1.79e12", "-5", "86400000", "1" * 21, "٢٥٠",
        str(WS - 1), str(WE), str(WS), str(WE - 1), iso(WE - 1000, "F2"), iso(WS, "F2"), "+" + str(ms),
        str(ms) + "\r", " " + str(ms), "２" + str(ms)[1:], str(WS - 7 * DAY), str(WS - 7 * DAY - 1),
    ])


def q(v):
    return f'"{v}"' if R.random() < 0.2 else v


def line(fields):
    s = ",".join(q(f) for f in fields)
    r = R.random()
    if r < 0.01:
        return ""
    if r < 0.02:
        return s + ",x"
    if r < 0.03:
        return s.replace(",", ",\"", 1)
    if r < 0.04:
        return s + "\r"
    if r < 0.045:
        return ",".join(fields[:-1])
    return s


def write(path, header, rows):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", newline="") as f:
        f.write(header + "\n" + "".join(r + "\n" for r in rows))


def split(name, header, rows):
    k = R.randint(1, 3)
    R.shuffle(rows)
    parts = [[] for _ in range(k)]
    for r in rows:
        parts[R.randrange(k)].append(r)
    for i, p in enumerate(parts):
        write(os.path.join(out, name, f"part-{i:05d}.csv"), header, p)
    if R.random() < 0.3:
        write(os.path.join(out, name, "part-99999.csv"), header, [])


# events: clustered times so gaps near 30 min are common
ev = []
eid = 0
anchors = [WS + R.randint(-DAY, 14 * DAY + DAY) for _ in range(6)] + [WS, WE - 1, WS - 1, WE]
for _ in range(R.randint(20, 250)):
    eid += 1
    u = R.choice(users)
    base = R.choice(anchors)
    t = base + R.choice([0, 30 * MIN, 30 * MIN + 1, 30 * MIN - 1, 60 * MIN, R.randint(0, 120) * MIN,
                         R.randint(0, 3 * 3600_000), -30 * MIN, 1, -1])
    ing = t + R.choice([1000, 60_000, 24 * 3600_000, 24 * 3600_000 + 1000, 24 * 3600_000 + 1, 5 * DAY, -1000])
    typ = R.choice(["view"] * 5 + ["cart"] * 2 + ["purchase"] * 3 + ["Purchase", "cart ", "click", ""])
    price = R.choice(["100", "0250", "10000000", "10000001", "0", "", "19.99", "-5", "+5", "２５０", "1", "500"])
    pid = R.choice(prods + ["p9", "P1", "", " p1"])
    tsv = iso(t) if R.random() > 0.08 else bad_ts(t)
    fields = [f"e{eid}", u, tsv, iso(ing, "F1") if ing % 1000 == 0 else iso(ing - ing % 1000, "F1"), typ, pid, price]
    ev.append(line(fields))
    for _ in range(R.choice([0, 0, 0, 1, 2])):  # duplicates
        f2 = list(fields)
        later = ing - ing % 1000 + R.choice([0, 1000, 600_000, -1000])
        f2[3] = iso(later, "F1")
        if R.random() < 0.3:
            f2[1] = R.choice(users)
        if R.random() < 0.3:
            f2[2] = iso(t) if R.random() < 0.5 else bad_ts(t)
        if later == ing - ing % 1000:
            f2 = list(fields)  # equal ingest -> identical copy (rules guarantee)
        ev.append(line(f2))
split("events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", ev)

cl = []
for i in range(R.randint(0, 80)):
    u = R.choice(users)
    base = R.choice(anchors) + R.choice([0, -1, 1, -7 * DAY, -7 * DAY - 1, -R.randint(0, 7 * DAY), -3600_000])
    fields = [f"k{i}", u, R.choice(camps + ["c_9", "C_1", ""]),
              iso(base) if R.random() > 0.08 else bad_ts(base)]
    cl.append(line(fields))
    if R.random() < 0.05:
        cl.append(line(fields))
    if R.random() < 0.1:  # same-ms tie with another click id
        cl.append(line([f"j{i}", u, R.choice(camps), fields[3]]))
split("clicks", "click_id,user_id,campaign_id,ts", cl)

lk = []
pool = users + [f"z{i}" for i in range(4)]
for _ in range(R.randint(0, 15)):
    a, b = R.choice(pool), R.choice(pool)
    lk.append(line([a, b]))
    if R.random() < 0.1:
        lk.append(line([b, a]))
    if R.random() < 0.05:
        lk.append(line([a, ""]))
if R.random() < 0.2:
    lk.append("user_id_a,user_id_b")
split("links", "user_id_a,user_id_b", lk)

write(os.path.join(out, "products.csv"), "product_id,category",
      [f"{p},{R.choice(cats)}" for p in prods])
write(os.path.join(out, "campaigns.csv"), "campaign_id,channel", [f"{c},ch{i % 3}" for i, c in enumerate(camps)])
with open(os.path.join(out, "manifest.json"), "w") as f:
    json.dump({"window_start": ws.strftime("%Y-%m-%dT%H:%M:%SZ"), "window_end": we.strftime("%Y-%m-%dT%H:%M:%SZ")}, f)
