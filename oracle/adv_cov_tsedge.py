#!/usr/bin/env python3
"""Timestamp edge set (h6, announce 58): F1/F2/F3 calendar, offset and epoch-length edges, as the
event ts, as a click ts, and as the ts of the kept or a dropped dedup copy. Each case gets its own
users so that a wrongly accepted (or wrongly rejected) value changes output rows:
  t<i>  purchase at ts=s, ingest = parsed(s)+24h exactly when s is valid (late-drop boundary),
        plus clicks at a fixed earlier time (c_1) and at ts=s (c_2; ties the session start).
  v<i>  anchor purchase at the end of the window, click at ts=s (c_3): credit / assist only if valid.
  w<i>  dedup: kept copy (earliest ingest) has ts=s, later copy is valid -> no rescue.
  x<i>  dedup: kept copy valid, later copy has ts=s and another user -> ignored.
Usage: adv_cov_tsedge.py OUT_DIR YEAR   (window YEAR-02-22 .. +14 days; pick leap/non-leap years)"""
import datetime as dt
import os
import random
import sys

out, Y = sys.argv[1], int(sys.argv[2])
R = random.Random(58 + Y)
for d in ("events", "clicks", "links"):
    os.makedirs(os.path.join(out, d), exist_ok=True)
UTC = dt.timezone.utc
E0 = dt.datetime(1970, 1, 1, tzinfo=UTC)
WS = dt.datetime(Y, 2, 22, tzinfo=UTC)
WE = WS + dt.timedelta(days=14)
DAY, WEEK = 86_400_000, 7 * 86_400_000


def ms(t):
    return (t - E0) // dt.timedelta(milliseconds=1)


def f1(m):
    return (E0 + dt.timedelta(milliseconds=m)).strftime("%Y-%m-%dT%H:%M:%SZ")


def leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


ws, we = ms(WS), ms(WE)
L = leap(Y)
fb = f"{Y:04d}-02"
mr = f"{Y:04d}-03"
V = ms(dt.datetime(Y, 2, 28, 12, tzinfo=UTC))  # a valid in-window instant (whole second)
with open(os.path.join(out, "manifest.json"), "w") as f:
    f.write('{"window_start": "%s", "window_end": "%s"}' % (f1(ws), f1(we)))
with open(os.path.join(out, "products.csv"), "w") as f:
    f.write("product_id,category\n" + "".join(f"p{i},cat{i % 3}\n" for i in range(5)))
with open(os.path.join(out, "campaigns.csv"), "w") as f:
    f.write("campaign_id,channel\nc_1,search\nc_2,email\nc_3,social\nc_4,display\n")

# ---- edge strings. Validity is decided by the oracle / xcheck, not here.
S = []
# F1 calendar
S += [f"{fb}-29T12:00:00Z", f"{fb}-30T12:00:00Z", f"{fb}-31T12:00:00Z", f"{mr}-00T12:00:00Z",
      f"{Y:04d}-00-28T12:00:00Z", f"{Y:04d}-13-01T12:00:00Z", f"{fb}-00T12:00:00Z",
      f"{fb}-28T24:00:00Z", f"{fb}-28T23:60:00Z", f"{fb}-28T23:59:60Z", f"{fb}-28T23:59:59Z",
      f"{mr}-01T00:00:00Z", f"{fb}-28T25:00:00Z", f"{fb}-28T12:99:00Z", f"{fb}-28T12:00:99Z",
      f"{mr}-32T00:00:00Z", f"{fb}-28t12:00:00Z", f"{fb}-28T12:00:00z", f"{fb}-28 12:00:00Z",
      f"{fb}-28T12:00:00", f"{fb}-28T12:00:00.000Z", f"{fb}-28T12:00:00Z ", f" {fb}-28T12:00:00Z",
      f"+{fb}-28T12:00:00Z", f"{fb}-28T12:00:00Z\r", f"{Y:04d}-2-28T12:00:00Z", f"{fb}-28T12:00:0Z",
      f"{fb}-28T1:00:00Z"]
# window edges as F1
S += [f1(ws), f1(ws - 1000), f1(we), f1(we - 1000), f1(ws - WEEK), f1(ws - WEEK - 1000)]
# F2 offsets
S += [f"{fb}-29T00:30:00+01:00", f"{fb}-28T23:30:00-01:00", f"{mr}-01T00:00:00+24:00",
      f"{mr}-01T00:00:00-24:00", f"{mr}-01T00:00:00+23:59", f"{mr}-01T00:00:00-23:59",
      f"{mr}-01T00:00:00+05:60", f"{mr}-01T00:00:00-00:00", f"{mr}-01T00:00:00+00:00",
      f"{mr}-01T00:00:00+00:60", f"{mr}-01T00:00:00+99:00", f"{mr}-01T00:00:00-00:59",
      f"{fb}-28T24:00:00+01:00", f"{fb}-28T12:00:00+0200", f"{fb}-28T12:00:00+02:000",
      f"{fb}-28T12:00:00+02-00", f"{fb}-28T12:00:00Z+01:00", f"{fb}-28T12:00:00 02:00",
      f"{fb}-28T12:00:00+2:00", f"{fb}-28T12:00:00-02:00\r", f"{fb}-30T00:30:00+01:00",
      f"{mr}-00T23:30:00-01:00", f"{fb}-28T23:59:60-01:00",
      f"{fb}-22T00:30:00+01:00", f"{fb}-21T23:30:00-01:00", f"{fb}-15T00:30:00+01:00",
      f"{fb}-14T23:30:00-01:00", f1(we)[:-1] + "-00:00", f1(we - 3_600_000)[:-1] + "+00:00",
      f1(we)[:-1] + "+00:01", f"{mr}-02T00:59:59+01:00", f"{fb}-28T12:00:00+12:00",
      f"{fb}-28T12:00:00-10:00"]
# F3 epoch
for base in (V, V + 1, ws, ws - 1, we - 1, we, ws - WEEK, ws - WEEK - 1):
    S += [str(base)]
s13 = str(V)
S += [s13.zfill(n) for n in (14, 15, 16, 17, 18, 19, 20, 21, 25, 40)]
S += [str(2**64 + V), str(2**63 + V), str(2**65 + V), str(10**19 + V), str(2**64 - 1),
      str(2**63 - 1), str(2**63), str(2**64), str(2**128 + V), str(V)[:-1], str(V)[:-2],
      str(V)[:-1].zfill(13), "0", "0" * 30, "+" + s13, "-" + s13, "-0", "+0", s13 + " ", " " + s13,
      s13 + "Z", s13 + ".0", s13[:5] + "e" + s13[6:], s13 + "\r", "1.79e12", "", s13[:6] + " " + s13[7:],
      str(V - 1800 * 1000), str(V + 999)]
S = list(dict.fromkeys(S))

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle  # noqa: E402  (only to place ingest_ts at exactly +24 h)

ev, cl, crlf_cl = [], [], []
ING_FIXED = f1(V + 3600_000)
q = lambda s, i: f'"{s}"' if i % 4 == 3 else s  # some fields quoted


def ingest_for(s):
    # exactly 24 h after the value when it parses as a plain F1/F2/F3 instant (else a fixed time)
    v = oracle.parse_ts(s.encode())
    if v is None or v % 1000 or not (-62135596800000 <= v + DAY <= 253402300799000):
        return ING_FIXED
    return f1(v + DAY)


for i, s in enumerate(S):
    ing = ingest_for(s)
    ev.append(f"e_t{i},t{i},{q(s, i)},{ing},purchase,p{i % 5},{100 + i}")
    cl.append(f"k_t{i}a,t{i},c_1,{f1(ws - 3 * DAY)}")
    cl.append(f"k_t{i}b,t{i},c_2,{q(s, i)}")
    ev.append(f"e_v{i},v{i},{f1(we - DAY)},{f1(we - DAY)},purchase,p{(i + 1) % 5},{200 + i}")
    cl.append(f"k_v{i},v{i},c_3,{q(s, i + 1)}")
    # dedup: kept copy carries s
    ev.append(f"e_w{i},w{i},{q(s, i)},{f1(V)},view,p1,")
    ev.append(f"e_w{i},w{i},{f1(V)},{f1(V + 1000)},purchase,p2,{300 + i}")
    # dedup: kept copy valid, later copy carries s with another user
    ev.append(f"e_x{i},x{i},{f1(V - 60_000)},{f1(V)},cart,p3,")
    ev.append(f"e_x{i},y{i},{q(s, i)},{f1(V + 2000)},purchase,p4,{400 + i}")
    # same values through a CRLF click file: every ts there ends in \r -> all bad
    crlf_cl.append(f"k_r{i},t{i},c_4,{s}")

# one person, the same instant in three forms (one session), 30-min gap boundaries across forms
g = V - 6 * 3_600_000
ev += [f"e_g1,g1,{f1(g)},{f1(g)},view,p0,", f"e_g2,g1,{g},{f1(g)},cart,p1,",
       f"e_g3,g1,{f1(g + 3_600_000)[:-1]}+01:00,{f1(g)},purchase,p2,500",
       f"e_g4,g1,{str(g + 1_800_000).zfill(16)},{f1(g)},view,p3,",
       f"e_g5,g1,{f1(g + 3_600_000)},{f1(g)},view,p3,",
       f"e_g6,g1,{g + 5_400_001},{f1(g)},purchase,p4,600",
       f"e_g7,g1,{f1(g + 5_400_001 + 1_800_000 - 1)[:-1]}Z,{f1(g)},view,p0,"]
cl += [f"k_g1,g1,c_1,{str(g).zfill(19)}", f"k_g2,g1,c_2,{f1(g)}", f"k_g3,g1,c_3,{f1(g - 3_600_000)[:-1]}-01:00"]
# a session starting 23:59:59 UTC written with a next-day local date, continuing past midnight
d = ms(dt.datetime(Y, 3, 1, 23, 59, 59, tzinfo=UTC))
ev += [f"e_d1,d1,{mr}-02T00:59:59+01:00,{f1(d)},purchase,p1,700",
       f"e_d2,d1,{d + 600_000},{f1(d)},purchase,p2,800"]
# late-event boundary through offsets: ingest = ts + 24 h (kept) and + 24 h + 1 s (dropped)
for k, off in enumerate(["+05:30", "-09:45", "+12:00", "-10:00", "-00:00"]):
    hh, mm = int(off[1:3]), int(off[4:6])
    utc = V - (1 if off[0] == "+" else -1) * (hh * 60 + mm) * 60_000
    ev.append(f"e_l{k}a,l{k},{f1(V)[:-1]}{off},{f1(utc + DAY)},purchase,p{k},{900 + k}")
    ev.append(f"e_l{k}b,l{k},{f1(V + 7_200_000)[:-1]}{off},{f1(utc + 7_200_000 + DAY + 1000)},purchase,p{k},{950 + k}")


def write(name, header, rows, nfiles, nl="\n", last_nl=True):
    R.shuffle(rows)
    parts = [[] for _ in range(nfiles)]
    for r in rows:
        parts[R.randrange(nfiles)].append(r)
    for i, p in enumerate(parts):
        body = header + nl + nl.join(p) + (nl if last_nl or i else "")
        with open(os.path.join(out, name, f"{name}-{i:03d}.csv"), "w", newline="") as f:
            f.write(body)


write("events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", ev, 3, last_nl=False)
write("clicks", "click_id,user_id,campaign_id,ts", cl, 3, last_nl=False)
with open(os.path.join(out, "clicks", "crlf.csv"), "w", newline="") as f:
    f.write("click_id,user_id,campaign_id,ts\r\n" + "".join(x + "\r\n" for x in crlf_cl))
# F3 values as the very last bytes of a file (no newline), short file
with open(os.path.join(out, "clicks", "z-tail.csv"), "w", newline="") as f:
    f.write(f"click_id,user_id,campaign_id,ts\nk_z,v0,c_4,{str(we - DAY - 1).zfill(17)}")
with open(os.path.join(out, "events", "z-tail.csv"), "w", newline="") as f:
    f.write(f"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\ne_z,z1,x,{f1(V)},view,p1,\ne_z2,z1,{V},{f1(V)},view,p1,")
with open(os.path.join(out, "links", "links.csv"), "w", newline="") as f:
    f.write("user_id_a,user_id_b\nt0,v1\nw2,x3\n")
print(f"{len(S)} edge strings, {len(ev)} events, {len(cl) + len(crlf_cl)} clicks -> {out}", file=sys.stderr)
