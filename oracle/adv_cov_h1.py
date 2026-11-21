#!/usr/bin/env python3
"""Coverage-driven adversarial dataset for csv.rs / ts.rs / ids.rs / keys.rs paths that the
generator, fuzz and existing adv-* sets never execute (h1, task 43). All inputs are within rules.md.
Usage: adv_cov_h1.py OUT_DIR"""
import json
import os
import random
import sys

out = sys.argv[1]
R = random.Random(43)
os.makedirs(out, exist_ok=True)
for d in ("events", "clicks", "links"):
    os.makedirs(os.path.join(out, d), exist_ok=True)

EV_H = "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
CL_H = "click_id,user_id,campaign_id,ts"
LK_H = "user_id_a,user_id_b"

# window 2026-10-01 .. 2026-10-15; products/campaigns with some quoted ids (F)
with open(os.path.join(out, "manifest.json"), "w") as f:
    json.dump({"window_start": "2026-10-01T00:00:00Z", "window_end": "2026-10-15T00:00:00Z"}, f)
with open(os.path.join(out, "products.csv"), "w") as f:
    f.write("product_id,category\n" + "".join(
        (f'"p{i}",cat{i % 5}\n' if i % 3 == 0 else f"p{i},\"cat{i % 5}\"\n" if i % 3 == 1 else f"p{i},cat{i % 5}\n")
        for i in range(40)))
with open(os.path.join(out, "campaigns.csv"), "w") as f:
    f.write("campaign_id,channel\n" + "".join(
        (f'"c_{i}",ch{i % 3}\n' if i % 2 else f"c_{i},ch{i % 3}\n") for i in range(12)))

ev, cl = [], []
eid = 0


def E(user, ts, ing, typ="view", prod="p1", price=""):
    global eid
    eid += 1
    ev.append(f"e{eid},{user},{ts},{ing},{typ},{prod},{price}")
    return f"e{eid}"


def C(cid, user, camp, ts):
    cl.append(f"{cid},{user},{camp},{ts}")


GOOD = "2026-10-03T10:00:00Z"
ING = "2026-10-03T10:05:00Z"
users = [f"u{i}" for i in range(30)]

# background valid traffic so persons/sessions/campaign rows exist
for i in range(400):
    u = R.choice(users)
    day = R.randint(1, 14)
    h, m = R.randint(0, 23), R.randint(0, 59)
    ts = f"2026-10-{day:02d}T{h:02d}:{m:02d}:00Z"
    typ = R.choice(["view", "view", "cart", "purchase"])
    E(u, ts, f"2026-10-{day:02d}T{h:02d}:{m:02d}:30Z", typ, f"p{R.randint(0, 45)}", str(R.randint(1, 5000)) if typ == "purchase" else "")
for i in range(150):
    day = R.randint(1, 14)
    C(f"k{i}", R.choice(users), f"c_{R.randint(0, 13)}", f"2026-10-{day:02d}T{R.randint(0, 23):02d}:00:00Z")

# A: 20-byte F1 with a non-digit (single ASCII byte) in every digit position, events and clicks
DIGPOS = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
for i in DIGPOS:
    for c in "O/:a A\t~":
        t = GOOD[:i] + c + GOOD[i + 1:]
        E(R.choice(users), t, ING)
        C(f"ka{i}_{ord(c)}", R.choice(users), "c_1", t)
# separators replaced by digits / other chars (length stays 20)
for i, c in [(4, "0"), (7, "1"), (10, "1"), (13, "0"), (16, "0"), (10, " "), (19, "0")]:
    t = GOOD[:i] + c + GOOD[i + 1:]
    E(R.choice(users), t, ING)
    C(f"ks{i}_{ord(c)}", R.choice(users), "c_2", t)

# B: F2 offsets with non-digit bytes in the offset (and valid neighbours for contrast)
for off in ["+0a:00", "+a5:00", "-05:0x", "+05:3 ", "+ 5:30", "-0:530", "+05:30", "-00:00", "+23:59", "-10:00"]:
    t = "2026-10-03T10:00:00" + off
    E(R.choice(users), t, "2026-10-03T12:00:00Z")
    C(f"kb{len(cl)}", R.choice(users), "c_3", t)

# C: ingest_ts years outside the 1900..2299 table (valid F1), on duplicated events so the
# dedup winner depends on exact ingest values; also kept events with an ingest far in the past
for k, ings in enumerate([
    ["0001-01-01T00:00:00Z", "2026-10-03T10:01:00Z"],
    ["1899-12-31T23:59:59Z", "1899-12-31T23:59:58Z", "2026-10-03T10:00:30Z"],
    ["1600-02-29T12:00:00Z", "1700-03-01T00:00:00Z"],
    ["2300-01-01T00:00:00Z", "2026-10-03T10:02:00Z"],
    ["9999-12-31T23:59:59Z", "2400-02-29T00:00:00Z"],
    ["1000-06-15T08:30:00Z", "1000-06-15T08:29:59Z"],
]):
    eidk = f"edup{k}"
    for j, ing in enumerate(ings):
        u = f"u{k}" if j == 0 else f"u{(k + j * 7) % 30}"  # copies name different users
        ev.append(f"{eidk},{u},2026-10-0{3 + k % 5}T1{k}:00:00Z,{ing},{'purchase' if j % 2 else 'view'},p{k},{100 + j}")

# E: long lines (> 128 bytes) through the general split path: valid quoted, bad inner quote
pad = "x" * 150
E("u1", GOOD, ING, "view", "p" + pad)  # unknown product, long, valid
ev.append(f'"e_long1","u2","{GOOD}","{ING}","purchase","p{pad}","250"')
ev.append(f'e_long2,"u3"x,{GOOD},{ING},view,p{pad},')  # quote not around whole field -> drop
ev.append(f'e_long3,"u"3",{GOOD},{ING},view,p{pad},')  # inner quote -> drop
ev.append(f'e_long4,u4,{GOOD},{ING},view,"p{pad}"",')  # bad trailing -> drop
cl.append(f'"k_long1","u5","c_4","{GOOD}"' + "")
cl.append(f'k_long2,"u{pad}",c_4,{GOOD}')
cl.append(f'k_long3,"u6"{pad},c_4,{GOOD}')  # bad -> drop
E("u" + pad, GOOD, ING)  # long user id (wide key) in events


def write(name, header, rows, nfiles):
    R.shuffle(rows)
    parts = [[] for _ in range(nfiles)]
    for r in rows:
        parts[R.randrange(nfiles)].append(r)
    for i, p in enumerate(parts):
        with open(os.path.join(out, name, f"part-{i:05d}.csv"), "w", newline="") as f:
            f.write(header + "\n" + "".join(x + "\n" for x in p))


write("events", EV_H, ev, 3)
write("clicks", CL_H, cl, 2)

# D: link files whose LAST line has no trailing newline and ends with IDs of various lengths
# (pack_scalar path: field within 16 bytes of the end of the mapping); IDs also used in events
tails = ["u7", "abcdefghij", "abcdefghijk", "u8longidx12", "U9", "u-10", "u 11", "ü12", "a" * 40, "zz"]
for i, t in enumerate(tails):
    # this id also has events, and the link joins it to a background user
    E(t, f"2026-10-0{1 + i % 9}T0{i % 10}:00:00Z", f"2026-10-0{1 + i % 9}T0{i % 10}:00:10Z")
    with open(os.path.join(out, "links", f"tail-{i:02d}.csv"), "w", newline="") as f:
        f.write(LK_H + "\n" + f"u{20 + i % 10},{users[i]}\n" + f"{users[(i + 3) % 30]},{t}")  # no final \n
# rewrite events (the tail ids added events); keep one more file without trailing newline too
write("events", EV_H, ev, 3)
with open(os.path.join(out, "events", "part-00099.csv"), "w", newline="") as f:
    f.write(EV_H + "\n" + f"e_tail,abcdefghijk,{GOOD},{ING},view,p2,")  # last event line w/o \n
with open(os.path.join(out, "clicks", "part-00099.csv"), "w", newline="") as f:
    f.write(CL_H + "\n" + f"k_tail,u1,c_1,2026-10-03T09:00:00Z")
print(f"{len(ev)} events, {len(cl)} clicks written to {out}", file=sys.stderr)
