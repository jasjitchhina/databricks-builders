#!/usr/bin/env python3
"""adv_synth.py DST SEED [PEOPLE] — synthetic dataset dense in structural edge cases (stdlib only).

Window contains Feb 29 2028 (or a random midnight for odd seeds). Respects every guarantee of rules.md
(no comma/quote/newline in values, equal-ingest copies identical, click copies identical, ingest_ts valid
F1, clean products/campaigns). Oracle output is ground truth.

Edge cases: 20k-ID chain linked in random order, 5k-ID star, a shared device joining 300 people,
unused linked IDs that become the name, unicode/space/case IDs (UTF-8 vs UTF-16 order traps),
gaps of exactly 1,800,000 / 1,800,001 ms, runs of equal ts across a gap, sessions crossing midnight,
events at window edges, clicks at start-7d / start-7d-1ms / start / start+1ms, same-ms click ties with
tricky click_ids, top_category ties, unknown products, revenue sums > 2^32, late at exactly 24h / +1s,
dedup groups whose earliest copy is invalid or another user, F3 leading zeros, F2 offsets changing date.
"""
import json
import os
import random
import shutil
import sys
from datetime import datetime, timezone, timedelta

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
DST, SEED = sys.argv[1], int(sys.argv[2])
PEOPLE = int(sys.argv[3]) if len(sys.argv) > 3 else 8000
R = random.Random(SEED)
DAY = 86400000
GAP = 1800000
WEEK = 7 * DAY

if SEED % 2 == 0:
    WS_DT = datetime(2028, 2, 23, tzinfo=timezone.utc)
else:
    WS_DT = datetime(2025, 1, 1, tzinfo=timezone.utc) + timedelta(days=R.randrange(4 * 365))
WS = int(WS_DT.timestamp() * 1000)
WE = WS + 14 * DAY


def f1(ms):
    return datetime.fromtimestamp(ms // 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def enc(ms):
    if ms % 1000 or R.random() < 0.3:
        return '0' * R.choice([0, 0, 0, 1, 5, 25]) + str(ms)
    if R.random() < 0.55:
        return f1(ms)
    off = R.choice([R.randint(-23 * 60 - 59, 23 * 60 + 59), 0, -600, 720, 23 * 60 + 59, -(23 * 60 + 59)])
    loc = ms + off * 60000
    sign = '-' if off < 0 or (off == 0 and R.random() < 0.5) else '+'
    a = abs(off)
    return datetime.fromtimestamp(loc // 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%S') + '%s%02d:%02d' % (sign, a // 60, a % 60)


BADTS = ['', '2028-02-30T10:00:00Z', '2027-02-29T10:00:00Z', '2028-13-01T00:00:00Z', '2028-02-25T24:00:00Z',
         '2028-02-25T10:00:60Z', '2028-02-25T10:00:00z', '2028-02-25 10:00:00Z', '2028-02-25T10:00:00',
         '2028-02-25T10:00:00.000Z', '2028-02-25T10:00:00+24:00', '2028-02-25T10:00:00+05:60',
         '2028-02-25T10:00:00+0200', '1.79e12', '-5', '99999999999999999999999', '18234١', 'abc']


def q(v, p=0.3):
    return '"%s"' % v if R.random() < p else v


# ---------------- ids
uid_counter = [0]
PREFIXES = ['u', 'u', 'u', 'u', 'U', 'u0', ' u', 'ü', 'ｕ', '\U0001F600', 'ué', 'a', 'z']
SUFFIXES = ['', '', '', '', ' ', '_', 'Z', '�', '\U00010000']


def new_id():
    uid_counter[0] += 1
    return R.choice(PREFIXES) + str(uid_counter[0]) + R.choice(SUFFIXES)


links = []  # (a, b)


def link_tree(ids):
    for i in range(1, len(ids)):
        links.append((ids[i], ids[R.randrange(i)]))


def link_chain(ids):
    order = list(ids)
    R.shuffle(order)
    for i in range(1, len(order)):
        a, b = order[i - 1], order[i]
        links.append((a, b) if R.random() < 0.5 else (b, a))


persons = []  # list of lists of ids used in rows
for _ in range(PEOPLE):
    k = R.choice([1, 1, 1, 2, 2, 3, 4, 6])
    ids = [new_id() for _ in range(k)]
    link_tree(ids)
    if R.random() < 0.08:  # extra linked id never used in rows (may be the name)
        extra = R.choice(['!', ' ', '0', 'A']) + new_id()
        links.append((extra, ids[0]))
    if R.random() < 0.02:
        links.append((ids[0], ids[0]))
    persons.append(ids)

chain = [new_id() for _ in range(20000)]
link_chain(chain)
star = [new_id() for _ in range(5000)]
for s in star[1:]:
    links.append((star[0], s) if R.random() < 0.5 else (s, star[0]))
persons.append(chain)
persons.append(star)
device = 'dev-' + new_id()
for p in R.sample(range(PEOPLE), min(PEOPLE, 300)):
    links.append((device, R.choice(persons[p])))
# whales: many events
whales = R.sample(range(len(persons) - 2), 5) + [len(persons) - 2, len(persons) - 1]

# ---------------- products / campaigns
CATS = ['apparel', 'books', 'garden', 'kitchen', 'toys', 'Zeta', 'électronique', 'a', 'aa', 'b ']
products = ['p%d' % i for i in range(300)]
pcat = {p: R.choice(CATS) for p in products}
camps = ['c_%d' % i for i in range(60)]
cchan = {c: R.choice(['email', 'search', 'social', 'display', 'affiliate']) for c in camps}
MISSING_PRODUCTS = ['p999999', 'P1', 'p1 ', ' p1', 'unknown', '']

events = []  # (event_id, user, ts_str, ingest_str, type, product, price)
clicks = []  # (click_id, user, campaign, ts_str)
eid_counter = [0]
cid_counter = [0]


def new_eid():
    eid_counter[0] += 1
    return 'e%d' % eid_counter[0]


def new_cid():
    cid_counter[0] += 1
    v = cid_counter[0]
    return R.choice(['k%d' % v, 'k%d' % v, 'K%d' % v, 'k%d ' % v, '\U0001F600%d' % v, 'k0%d' % v])


def add_event(ids, ms, whale=False):
    t = R.random()
    if t < 0.75:
        et, price = 'view' if R.random() < 0.7 else 'cart', R.choice(['', '', '5', '-1', 'x'])
    elif t < 0.999:
        et = 'purchase'
        price = str(R.choice([100, 200, 300, 9999999, 10000000, R.randint(1, 50000)]))
        if whale and R.random() < 0.5:
            price = '9999999'
        x = R.random()
        if x < 0.03:
            price = R.choice(['0', '10000001', '19.99', '-5', '+5', '', ' 5', '５', '0250', '00000000000000000100'])
    else:
        et, price = R.choice(['Purchase', 'VIEW', 'cart ', '']), '100'
    prod = R.choice(products) if R.random() > 0.01 else R.choice(MISSING_PRODUCTS)
    uid = R.choice(ids) if R.random() < 0.997 else ''
    ts = enc(ms) if R.random() > 0.004 else R.choice(BADTS)
    lag = R.choice([0, 1, 5, 60, 600]) * 1000 + R.randint(0, 999)
    if R.random() < 0.01:
        lag = R.choice([DAY, DAY + 1000, DAY - 1000, DAY + 1, -60000, 5 * DAY])
    ing = ((ms + lag) // 1000) * 1000
    if ms % 1000 == 0 and lag in (DAY, DAY + 1000):
        ing = ms + lag
    e = [new_eid(), uid, ts, f1(ing), et, prod, price]
    events.append(e)
    r = R.random()
    if r < 0.03:  # later copies
        for k in R.sample(range(1, 601), R.randint(1, 3)):
            c = list(e)
            c[3] = f1(ing + k * 1000)
            if R.random() < 0.2:
                c[1] = R.choice(ids)
            if R.random() < 0.1:
                c[2] = enc(ms + R.choice([0, 1, GAP]))
            events.append(c)
    elif r < 0.035:  # earliest copy invalid, later valid
        c = list(e)
        c[3] = f1(ing - R.randint(1, 600) * 1000)
        c[2] = R.choice(BADTS)
        events.append(c)
    elif r < 0.04:  # exact copies (same ingest -> identical)
        events.append(list(e))


GAPS = [0, 0, 1, 1000, 30000, 60000, 300000, GAP - 1, GAP, GAP, GAP + 1]


def gen_person(ids, nchains, whale=False):
    starts = []
    for _ in range(nchains):
        x = R.random()
        if x < 0.05:
            ms = WS + R.choice([0, 1, 999])
        elif x < 0.1:
            ms = WE - R.choice([1, 1000, GAP])
        elif x < 0.25:  # near midnight
            ms = WS + R.randrange(14) * DAY - R.randint(1, 3 * GAP)
        else:
            ms = WS + R.randrange(14 * DAY)
        if R.random() < 0.7:
            ms = (ms // 1000) * 1000
        starts.append(ms)
        n = R.randint(1, 40 if whale else 12)
        for _ in range(n):
            if WS - DAY <= ms < WE + DAY:
                reps = R.choice([1, 1, 1, 2, 3])
                for _ in range(reps):
                    add_event(ids, ms, whale)
            ms += R.choice(GAPS)
            if R.random() < 0.5:
                ms = (ms // 1000) * 1000 if R.random() < 0.5 else ms
    # clicks around starts
    for s in starts:
        if R.random() < 0.35:
            continue
        for _ in range(R.randint(1, 4)):
            off = R.choice([0, 1, -1, WEEK, WEEK + 1, WEEK - 1, R.randint(0, WEEK), R.randint(0, WEEK), 3600000, GAP])
            cms = s - off
            if R.random() < 0.15:  # same ms tie
                for _ in range(2):
                    add_click(ids, cms)
            add_click(ids, cms)
    if R.random() < 0.05:
        add_click(ids, WS - WEEK)
        add_click(ids, WS - WEEK - 1)
        add_click(ids, WE)


def add_click(ids, ms):
    camp = R.choice(camps[:5] if R.random() < 0.3 else camps)
    if R.random() < 0.006:
        camp = R.choice(['C_1', 'c_1 ', 'c_9999', '', 'organic'])
    uid = R.choice(ids) if R.random() < 0.997 else ''
    ts = enc(ms) if R.random() > 0.004 else R.choice(BADTS)
    c = [new_cid(), uid, camp, ts]
    clicks.append(c)
    if R.random() < 0.03:
        clicks.append(list(c))


for i, ids in enumerate(persons):
    if i in whales:
        gen_person(ids, R.randint(150, 400), whale=True)
    else:
        gen_person(ids, R.choice([1, 1, 2, 3, 5, 10]))


def write(folder, header, lines, nfiles, nheader):
    os.makedirs(folder)
    R.shuffle(lines)
    w = [1.0 / (i + 1) for i in range(nfiles)]
    R.shuffle(w)
    tot = sum(w)
    chunks, acc, prev = [], 0.0, 0
    for x in w:
        acc += x / tot
        c = int(round(acc * len(lines)))
        chunks.append(lines[prev:c]); prev = c
    chunks[-1] += lines[prev:]
    chunks += [[] for _ in range(nheader)]
    R.shuffle(chunks)
    for i, ch in enumerate(chunks):
        with open(os.path.join(folder, 'part-%05d.csv' % i), 'w', encoding='utf-8', newline='') as fh:
            fh.write(header + '\n')
            fh.writelines(l + '\n' for l in ch)


def lines_of(rows, ncol):
    out = []
    for r in rows:
        x = R.random()
        if x < 0.002:
            out.append('')
        elif x < 0.004:
            out.append(','.join(r[:-1]))
        elif x < 0.005:
            out.append(','.join(r) + ',')
        elif x < 0.006:
            out.append(','.join(['"' + r[0]] + r[1:]))
        elif x < 0.007:
            out.append(','.join([r[0][:1] + '"' + r[0][1:]] + r[1:]))
        out.append(','.join(q(v) for v in r))
    return out


if os.path.exists(DST):
    shutil.rmtree(DST)
os.makedirs(DST)
with open(os.path.join(DST, 'manifest.json'), 'w') as fh:
    json.dump({'window_start': f1(WS), 'window_end': f1(WE)}, fh)
with open(os.path.join(DST, 'products.csv'), 'w', encoding='utf-8') as fh:
    fh.write('product_id,category\n' + ''.join('%s,%s\n' % (p, pcat[p]) for p in products))
with open(os.path.join(DST, 'campaigns.csv'), 'w', encoding='utf-8') as fh:
    fh.write('campaign_id,channel\n' + ''.join('%s,%s\n' % (c, cchan[c]) for c in camps))

lk = []
for a, b in links:
    if R.random() < 0.08:
        lk.append((b, a))
    if R.random() < 0.002:
        lk.append((a, ''))
    lk.append((a, b))
lk.append(('user_id_a', 'user_id_b'))
write(os.path.join(DST, 'events'), 'event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents', lines_of(events, 7), R.randint(40, 200), R.randint(1, 3))
write(os.path.join(DST, 'clicks'), 'click_id,user_id,campaign_id,ts', lines_of(clicks, 4), R.randint(10, 50), R.randint(1, 3))
write(os.path.join(DST, 'links'), 'user_id_a,user_id_b', lines_of([list(x) for x in lk], 2), R.randint(5, 25), R.randint(1, 3))
print('%s: %d events, %d clicks, %d links, window %s' % (DST, len(events), len(clicks), len(lk), f1(WS)), file=sys.stderr)
