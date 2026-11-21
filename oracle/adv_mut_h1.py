#!/usr/bin/env python3
"""Datasets that kill mutants surviving every other set (h1 mutation testing).
Usage: adv_mut_h1.py OUT_A OUT_B
 A: window 2026-12-25..2027-01-08 so mis-accepted month 13 / hour 24 / second 60 F1 values land
    IN the window; plus lone-quote CSV lines.   B: window inside Feb 2100 (not a leap year)."""
import json
import os
import sys


def ds(out, ws, we, ev, cl, links=()):
    for d in ("events", "clicks", "links"):
        os.makedirs(os.path.join(out, d), exist_ok=True)
    json.dump({"window_start": ws, "window_end": we}, open(os.path.join(out, "manifest.json"), "w"))
    open(os.path.join(out, "products.csv"), "w").write("product_id,category\np1,shoes\np2,books\n")
    open(os.path.join(out, "campaigns.csv"), "w").write("campaign_id,channel\nc_1,search\nc_2,social\n")
    open(os.path.join(out, "events", "part-0.csv"), "w", newline="").write(
        "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n" + "".join(x + "\n" for x in ev))
    open(os.path.join(out, "clicks", "part-0.csv"), "w", newline="").write(
        "click_id,user_id,campaign_id,ts\n" + "".join(x + "\n" for x in cl))
    open(os.path.join(out, "links", "part-0.csv"), "w", newline="").write(
        "user_id_a,user_id_b\n" + "".join(x + "\n" for x in links))


a, b = sys.argv[1], sys.argv[2]

# ---- A. Each bad F1 is a purchase by its own user, with a valid view 10 min later that would
# merge into the same session (changing sessions/events/revenue) if the bad row were kept; and a
# click at the bad timestamp that would credit the valid session.
ev, cl = [], []
bad = [
    ("mm13", "2026-13-02T10:00:00Z", "2027-01-02T10:10:00Z", "2027-01-02T10:01:00Z"),  # month 13 -> 2027-01-02
    ("hh24", "2026-12-31T24:00:00Z", "2027-01-01T00:10:00Z", "2027-01-01T00:01:00Z"),  # hour 24 -> next day 00:00
    ("ss60", "2026-12-31T23:59:60Z", "2027-01-01T00:10:00Z", "2027-01-01T00:01:00Z"),  # second 60 -> next minute
    ("ss60b", "2027-01-01T23:59:60Z", "2027-01-02T00:10:00Z", "2027-01-02T00:01:00Z"),
    ("hh24b", "2027-01-03T24:00:00Z", "2027-01-04T00:20:00Z", "2027-01-04T00:01:00Z"),
    ("dd32", "2026-12-32T10:00:00Z", "2027-01-01T10:10:00Z", "2027-01-01T10:01:00Z"),
]
for i, (tag, t, tgood, ing) in enumerate(bad):
    u = f"u_{tag}"
    ev.append(f"eb{i},{u},{t},{ing},purchase,p1,{1000 + i}")
    ev.append(f"eg{i},{u},{tgood},{tgood},view,p2,")
    cl.append(f"kb{i},{u},c_{1 + i % 2},{t}")
    # same bad timestamps in F2 form (offset -00:00): same validator path for the civil part
    ev.append(f"ec{i},{u}_f2,{t[:19]}-00:00,{ing},purchase,p2,{2000 + i}")
    ev.append(f"ed{i},{u}_f2,{tgood},{tgood},view,p1,")
# valid neighbours (control)
ev.append("ectl,u_ctl,2027-01-01T00:00:00Z,2027-01-01T00:00:00Z,purchase,p1,500")
ev.append("ectl2,u_ctl,2026-12-31T23:59:59Z,2026-12-31T23:59:59Z,view,p1,")
cl.append("kctl,u_ctl,c_2,2026-12-31T23:00:00Z")
# lone-quote field + one stray quote elsewhere: quote count 2 => must still be DROPPED (malformed)
ev.append('elq1,u_lq,2027-01-02T10:00:00Z,2027-01-02T10:00:00Z,purchase,p1"x,"')
ev.append('elq2,u_lq,2027-01-02T10:05:00Z,2027-01-02T10:05:00Z,view,p1,"')
ev.append('"elq3,u_lq,2027-01-02T10:06:00Z,2027-01-02T10:06:00Z,view,p1,"')
ev.append('elq4,",2027-01-02T10:07:00Z,2027-01-02T10:07:00Z,view,"p1,')
ev.append("elq5,u_lq,2027-01-02T10:20:00Z,2027-01-02T10:20:00Z,view,p2,")  # valid anchor
cl.append('klq1,u_lq,c_1",2027-01-02T09:00:00Z,"')
cl.append('klq2,",c_1,"2027-01-02T09:00:00Z')
cl.append('klq3,u_lq,",c_2"')
ds(a, "2026-12-25T00:00:00Z", "2027-01-08T00:00:00Z", ev, cl)

# ---- B. 2100 is NOT a leap year: 2100-02-29 is a bad timestamp
ev, cl = [], []
for i, t in enumerate(["2100-02-29T10:00:00Z", "2100-02-29T23:59:59Z", "2100-02-30T10:00:00Z", "2100-02-29T10:00:00+01:00"]):
    u = f"u_feb{i}"
    ev.append(f"ef{i},{u},{t},2100-03-01T00:00:00Z,purchase,p1,{300 + i}")
    ev.append(f"eg{i},{u},2100-03-01T00:20:00Z,2100-03-01T00:20:00Z,view,p2,")
    cl.append(f"kf{i},{u},c_1,{t}")
ev.append("ectl,u_ctl,2100-02-28T23:59:59Z,2100-02-28T23:59:59Z,purchase,p2,700")
ev.append("ectl2,u_ctl,2100-03-01T00:00:00Z,2100-03-01T00:00:00Z,view,p1,")
cl.append("kctl,u_ctl,c_2,2100-02-28T12:00:00Z")
ds(b, "2100-02-22T00:00:00Z", "2100-03-08T00:00:00Z", ev, cl)
print("ok", file=sys.stderr)
