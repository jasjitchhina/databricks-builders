#!/usr/bin/env python3
"""
ORACLE: slow, deliberately literal reference implementation of rules.md.

Usage: oracle.py DATA_DIR OUT_DIR          (or env DATA_DIR / OUTPUT_DIR)
Optional: DUMP_DIR=dir  -> writes stage dumps (sorted CSV, raw bytes, no quoting):
  persons.csv        user_id,person
  events_clean.csv   event_id,person,ts_ms,event_type,product_id,price_cents
  clicks_clean.csv   click_id,person,campaign_id,ts_ms
  sessions.csv       person,start_ms,n_events,revenue,top_category,credited_campaign,assist_campaigns

Everything is handled as bytes so every comparison is byte-wise, exactly as rules.md says.
Plain Python + pyarrow (only for writing Parquet). No CSV library is used: the dialect is parsed by hand.
"""
import bisect
import json
import os
import re
import sys
from collections import defaultdict

DAY_MS = 86_400_000
GAP_MS = 1_800_000
WEEK_MS = 7 * DAY_MS
LATE_MS = 24 * 3_600_000


# ---------------------------------------------------------------- CSV dialect
def split_line(line, nfields):
    """Return list of values (bytes, or None for null) or None if the line is malformed.

    Rules: only \\n ends a line (caller split on it, \\r stays in the field). Every comma separates
    fields. A field may be wrapped in double quotes; a quote anywhere else -> malformed. Wrong field
    count -> malformed. Empty line -> malformed. Empty field ("" or nothing) -> null.
    """
    if line == b"":
        return None
    parts = line.split(b",")
    if len(parts) != nfields:
        return None
    out = []
    for p in parts:
        if b'"' in p:
            if len(p) >= 2 and p[0:1] == b'"' and p[-1:] == b'"' and b'"' not in p[1:-1]:
                p = p[1:-1]
            else:
                return None
        out.append(p if p != b"" else None)
    return out


def read_table(path, nfields):
    """Yield parsed rows of one CSV file; the first line is the header and is skipped.
    Later lines equal to the header are ordinary data lines (fixture L3)."""
    with open(path, "rb") as f:
        data = f.read()
    if data == b"":
        return
    lines = data.split(b"\n")
    if data.endswith(b"\n"):
        lines.pop()  # the terminator of the last line, not an empty line
    for line in lines[1:]:
        row = split_line(line, nfields)
        if row is not None:
            yield row


def csv_files(folder):
    if not os.path.isdir(folder):
        return []
    return sorted(os.path.join(folder, n) for n in os.listdir(folder)
                  if n.endswith(".csv") and os.path.isfile(os.path.join(folder, n)))


# ---------------------------------------------------------------- timestamps
F1 = re.compile(rb"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})Z")
F2 = re.compile(rb"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})([+-])([0-9]{2}):([0-9]{2})")
F3 = re.compile(rb"[0-9]+")


def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


MDAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]


def days_from_civil(y, m, d):
    # Howard Hinnant's algorithm, proleptic Gregorian
    y -= m <= 2
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    doy = (153 * (m + (-3 if m > 2 else 9)) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def civil_ms(y, mo, d, h, mi, s):
    if not (1 <= mo <= 12):
        return None
    dim = 29 if (mo == 2 and is_leap(y)) else MDAYS[mo - 1]
    if not (1 <= d <= dim) or h > 23 or mi > 59 or s > 59:
        return None
    return ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + s * 1000


def parse_ts(v):
    """bytes -> int epoch ms, or None if bad (window not checked here)."""
    if v is None:
        return None
    m = F1.fullmatch(v)
    if m:
        return civil_ms(*(int(x) for x in m.groups()))
    m = F2.fullmatch(v)
    if m:
        g = m.groups()
        base = civil_ms(*(int(x) for x in g[:6]))
        oh, om = int(g[7]), int(g[8])
        if base is None or oh > 23 or om > 59:
            return None
        off = (oh * 60 + om) * 60_000
        return base - off if g[6] == b"+" else base + off
    if F3.fullmatch(v):
        return int(v)  # arbitrary length; out-of-range values fail the window check
    return None


def parse_iso_window(s):
    v = parse_ts(s.encode())
    if v is None:
        raise ValueError(f"bad manifest timestamp {s!r}")
    return v


PRICE = re.compile(rb"[0-9]+")


# ---------------------------------------------------------------- main
def main():
    args = sys.argv[1:]
    data = args[0] if len(args) > 0 else os.environ.get("DATA_DIR", "/data")
    out = args[1] if len(args) > 1 else os.environ.get("OUTPUT_DIR", "/output")
    dump = os.environ.get("DUMP_DIR")

    with open(os.path.join(data, "manifest.json")) as f:
        man = json.load(f)
    ws = parse_iso_window(man["window_start"])
    we = parse_iso_window(man["window_end"])

    # ---- reference tables (said to be clean; parsed with the same dialect anyway)
    categories = {}
    for r in read_table(os.path.join(data, "products.csv"), 2):
        categories[r[0]] = r[1]
    channels = {}
    for r in read_table(os.path.join(data, "campaigns.csv"), 2):
        channels[r[0]] = r[1]

    # ---- links: union-find, name = byte-wise smallest ID
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    def union(a, b):
        for x in (a, b):
            if x not in parent:
                parent[x] = x
        ra, rb = find(a), find(b)
        if ra != rb:
            if rb < ra:
                ra, rb = rb, ra
            parent[rb] = ra  # root is always the smallest ID of its set

    for path in csv_files(os.path.join(data, "links")):
        for a, b in read_table(path, 2):
            if a is None or b is None:
                continue
            union(a, b)

    def person(uid):
        return find(uid) if uid in parent else uid

    # ---- events: parse -> dedup (min ingest_ts) -> validate -> late
    kept = {}  # event_id -> (ingest_ms, row)
    for path in csv_files(os.path.join(data, "events")):
        for r in read_table(path, 7):
            eid = r[0]
            if eid is None:
                continue
            ing = parse_ts(r[3])
            if ing is None:
                raise ValueError(f"ingest_ts not parseable in well-formed line: {r!r} ({path})")
            prev = kept.get(eid)
            if prev is None or ing < prev[0]:
                kept[eid] = (ing, r)
            # equal ingest_ts: copies are identical per rules.md, keep the first one read

    raw_uids = set()
    events = []  # (person, ts, event_id, event_type, product_id, price or None)
    for eid, (ing, r) in kept.items():
        _, uid, ts_raw, _, etype, pid, price_raw = r
        if uid is None or pid is None:
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (ws <= ts < we):
            continue
        if etype not in (b"view", b"cart", b"purchase"):
            continue
        price = None
        if etype == b"purchase":
            if price_raw is None or not PRICE.fullmatch(price_raw):
                continue
            price = int(price_raw)
            if not (1 <= price <= 10_000_000):
                continue
        if ing - ts > LATE_MS:
            continue
        events.append((person(uid), ts, eid, etype, pid, price))
        raw_uids.add(uid)
    del kept

    # ---- clicks: parse -> dedup -> validate
    seen_clicks = {}
    for path in csv_files(os.path.join(data, "clicks")):
        for r in read_table(path, 4):
            if r[0] is None:
                continue
            if r[0] not in seen_clicks:
                seen_clicks[r[0]] = r
    clicks_by_person = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    clicks_clean = []
    for cid, (_, uid, camp, ts_raw) in seen_clicks.items():  # click_id,user_id,campaign_id,ts
        if uid is None:
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (ws - WEEK_MS <= ts < we):
            continue
        if camp is None or camp not in channels:
            continue
        p = person(uid)
        raw_uids.add(uid)
        clicks_by_person[p].append((ts, cid, camp))
        clicks_clean.append((cid, p, camp, ts))
    del seen_clicks
    for lst in clicks_by_person.values():
        lst.sort()
    click_ts = {p: [c[0] for c in lst] for p, lst in clicks_by_person.items()}

    # ---- sessionize
    by_person = defaultdict(list)
    for e in events:
        by_person[e[0]].append(e)

    sessions = []  # (person, start, n_events, revenue, top_cat, credited, assists(sorted tuple))
    for p, evs in by_person.items():
        evs.sort(key=lambda e: e[1])
        groups = []
        cur = None
        last_ts = None
        for e in evs:
            if cur is None or e[1] - last_ts > GAP_MS:
                cur = []
                groups.append(cur)
            cur.append(e)
            last_ts = e[1]
        cl = clicks_by_person.get(p, [])
        cts = click_ts.get(p, [])
        for g in groups:
            start = g[0][1]
            revenue = 0
            cat_rev = defaultdict(int)
            for e in g:
                if e[3] == b"purchase":
                    revenue += e[5]
                    cat_rev[categories.get(e[4], b"unknown")] += e[5]
            top = None
            if cat_rev:
                best = max(cat_rev.values())
                top = min(c for c, v in cat_rev.items() if v == best)
            lo = bisect.bisect_left(cts, start - WEEK_MS)
            hi = bisect.bisect_right(cts, start)
            cand = cl[lo:hi]
            if cand:
                tmax = max(c[0] for c in cand)
                credited = min(c[1] for c in cand if c[0] == tmax)
                credited_camp = next(c[2] for c in cand if c[0] == tmax and c[1] == credited)
                assists = tuple(sorted({c[2] for c in cand}))
            else:
                credited_camp = b"organic"
                assists = ()
            sessions.append((p, start, len(g), revenue, top, credited_camp, assists, cat_rev))

    # ---- aggregate
    ud = {}  # (person, day) -> [sessions, events, revenue, cat_rev dict]
    cd = {}  # (campaign, day) -> [sessions, purchasing, revenue, assisted]
    for p, start, n, rev, top, camp, assists, cat_rev in sessions:
        day = start // DAY_MS  # floor division: correct for negative too (not reachable)
        u = ud.setdefault((p, day), [0, 0, 0, defaultdict(int)])
        u[0] += 1
        u[1] += n
        u[2] += rev
        for c, v in cat_rev.items():
            u[3][c] += v
        c = cd.setdefault((camp, day), [0, 0, 0, 0])
        c[0] += 1
        c[2] += rev
        for a in assists:
            cc = cd.setdefault((a, day), [0, 0, 0, 0])
            cc[3] += 1
    # purchasing sessions: a session with at least one purchase (revenue > 0 since price >= 1)
    for p, start, n, rev, top, camp, assists, cat_rev in sessions:
        if cat_rev:
            cd[(camp, start // DAY_MS)][1] += 1

    # ---- write parquet
    import datetime
    import pyarrow as pa
    import pyarrow.parquet as pq

    epoch = datetime.date(1970, 1, 1)

    def dec(b):
        return None if b is None else b.decode("utf-8")

    rows_u = sorted(ud.items())
    u_tab = pa.table({
        "user_id": pa.array([dec(k[0]) for k, _ in rows_u], pa.string()),
        "day": pa.array([epoch + datetime.timedelta(days=k[1]) for k, _ in rows_u], pa.date32()),
        "sessions": pa.array([v[0] for _, v in rows_u], pa.int64()),
        "events": pa.array([v[1] for _, v in rows_u], pa.int64()),
        "revenue_cents": pa.array([v[2] for _, v in rows_u], pa.int64()),
        "top_category": pa.array([dec(top_of(v[3])) for _, v in rows_u], pa.string()),
    })
    rows_c = sorted(cd.items())
    c_tab = pa.table({
        "campaign_id": pa.array([dec(k[0]) for k, _ in rows_c], pa.string()),
        "channel": pa.array([dec(b"organic" if k[0] == b"organic" else channels[k[0]]) for k, _ in rows_c],
                            pa.string()),
        "day": pa.array([epoch + datetime.timedelta(days=k[1]) for k, _ in rows_c], pa.date32()),
        "sessions": pa.array([v[0] for _, v in rows_c], pa.int64()),
        "purchasing_sessions": pa.array([v[1] for _, v in rows_c], pa.int64()),
        "revenue_cents": pa.array([v[2] for _, v in rows_c], pa.int64()),
        "assisted_sessions": pa.array([v[3] for _, v in rows_c], pa.int64()),
    })
    for name, tab in (("user_daily", u_tab), ("campaign_daily", c_tab)):
        d = os.path.join(out, name)
        os.makedirs(d, exist_ok=True)
        pq.write_table(tab, os.path.join(d, "part-0.parquet"))

    # ---- self-checks from rules.md
    assert sum(v[0] for v in ud.values()) == sum(v[0] for v in cd.values())
    assert sum(v[2] for v in ud.values()) == sum(v[2] for v in cd.values())
    assert sum(v[3] for v in cd.values()) >= sum(v[0] for k, v in cd.items() if k[0] != b"organic")

    # ---- stage dumps
    if dump:
        os.makedirs(dump, exist_ok=True)

        def write(name, header, rows):
            rows = sorted(rows)
            with open(os.path.join(dump, name), "wb") as f:
                f.write(header + b"\n")
                for r in rows:
                    f.write(b",".join(b"" if x is None else (x if isinstance(x, bytes) else str(x).encode())
                                      for x in r) + b"\n")

        ids = set(parent)
        ids.update(raw_uids)  # raw user_ids of clean events and clicks
        persons_rows = [(i, person(i)) for i in ids]
        write("persons.csv", b"user_id,person", persons_rows)
        write("events_clean.csv", b"event_id,person,ts_ms,event_type,product_id,price_cents",
              [(e[2], e[0], e[1], e[3], e[4], e[5]) for e in events])
        write("clicks_clean.csv", b"click_id,person,campaign_id,ts_ms", clicks_clean)
        write("sessions.csv",
              b"person,start_ms,n_events,revenue,top_category,credited_campaign,assist_campaigns",
              [(s[0], s[1], s[2], s[3], s[4], s[5], b";".join(s[6])) for s in sessions])
    print(f"oracle: {len(events)} events, {len(clicks_clean)} clicks, {len(sessions)} sessions, "
          f"{len(ud)} user_daily rows, {len(cd)} campaign_daily rows", file=sys.stderr)


def top_of(cat_rev):
    if not cat_rev:
        return None
    best = max(cat_rev.values())
    return min(c for c, v in cat_rev.items() if v == best)


if __name__ == "__main__":
    main()
