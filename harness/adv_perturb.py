#!/usr/bin/env python3
"""adv_perturb.py SRC DST SEED — rewrite a generated dataset into a nastier one (stdlib only).

Oracle output on DST is ground truth; semantics are NOT preserved, but every guarantee
rules.md makes about the data is kept:
  * no value holds a comma, quote or newline (except in deliberately malformed lines)
  * event copies with equal ingest_ts are identical; ingest_ts is always valid F1
  * click copies have identical values (value perturbations are keyed on click_id)
  * products.csv / campaigns.csv untouched
Perturbations: re-split into many files (incl. header-only), heavy random quoting (incl. ""),
ts re-encoded into F1/F2(any valid offset)/F3(up to 30 leading zeros), injected bad ts of every
kind, \\r line endings, malformed lines, spaced/upper-cased/unicode IDs, price edge values,
event_type variants, extra dup copies whose earliest copy is invalid, F3 overflow.
"""
import calendar
import hashlib
import os
import random
import re
import shutil
import sys
from datetime import datetime, timezone

SRC, DST, SEED = sys.argv[1], sys.argv[2], int(sys.argv[3])
R = random.Random(SEED)

F1 = re.compile(r'^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(Z|[+-]\d{2}:\d{2})$', re.A)


def parse_ts(s):
    if s.isdigit() and s.isascii():
        return int(s)
    m = F1.match(s)
    if not m:
        return None
    y, mo, d, h, mi, se = map(int, m.groups()[:6])
    z = m.group(7)
    try:
        datetime(y, mo, d, h, mi, se)
    except ValueError:
        return None
    ms = calendar.timegm((y, mo, d, h, mi, se, 0, 0, 0)) * 1000
    if z != 'Z':
        sign = 1 if z[0] == '+' else -1
        ms -= sign * (int(z[1:3]) * 3600 + int(z[4:6]) * 60) * 1000
    return ms


def fmt_f1(ms):
    return datetime.fromtimestamp(ms // 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def encode_ts(ms, r):
    """Valid encoding of ms in a random form."""
    if ms % 1000 or r.random() < 0.35:
        z = r.choice([0, 0, 0, 1, 3, 7, 12, 30]) if r.random() < 0.3 else 0
        return '0' * z + str(ms)
    if r.random() < 0.5:
        return fmt_f1(ms)
    off = r.randint(-23 * 60 - 59, 23 * 60 + 59)
    if r.random() < 0.05:
        off = 0
    loc = ms + off * 60000
    sign = '-' if off < 0 or (off == 0 and r.random() < 0.5) else '+'
    a = abs(off)
    return datetime.fromtimestamp(loc // 1000, timezone.utc).strftime('%Y-%m-%dT%H:%M:%S') + '%s%02d:%02d' % (sign, a // 60, a % 60)


def bad_ts(ms, r, ws, we):
    """A bad (or boundary) timestamp near ms."""
    f = fmt_f1(ms)
    k = r.randrange(24)
    if k == 0: return ''
    if k == 1: return f[:-1] + 'z'
    if k == 2: return f.replace('T', 't')
    if k == 3: return f.replace('T', ' ')
    if k == 4: return f[:-1]
    if k == 5: return f[:-1] + '.000Z'
    if k == 6: return f[:5] + '13' + f[7:]
    if k == 7: return f[:5] + '02-30' + f[10:]
    if k == 8: return f[:5] + '02-29' + f[10:] if int(f[:4]) % 4 else f[:5] + '02-30' + f[10:]
    if k == 9: return f[:11] + '24' + f[13:]
    if k == 10: return f[:17] + '60Z'
    if k == 11: return f[:-1] + '+24:00'
    if k == 12: return f[:-1] + '+05:60'
    if k == 13: return f[:-1] + '+0200'
    if k == 14: return str(ms) + '.0'
    if k == 15: return '%.2e' % ms
    if k == 16: return '-' + str(ms)
    if k == 17: return '9' * r.choice([19, 20, 25])
    if k == 18: return str(ms).replace('1', '١')  # Arabic-Indic one
    if k == 19: return '２' + f[1:]  # full-width digit in year
    if k == 20: return str(ws - 1)  # just before window
    if k == 21: return encode_ts(we, r)  # exactly window end
    if k == 22: return str(r.choice([0, 86400000, 1]))
    return f[:-1] + '+00:00 '


def maybe_quote(v, r, p):
    return '"%s"' % v if r.random() < p else v


def vrng(key):
    return random.Random(int.from_bytes(hashlib.blake2b(('%d|%s' % (SEED, key)).encode(), digest_size=8).digest(), 'little'))


def malformed(fields, r):
    k = r.randrange(6)
    if k == 0: return ''
    if k == 1: return ','.join(fields + ['x'])
    if k == 2: return ','.join(fields[:-1])
    if k == 3:
        f = list(fields); i = r.randrange(len(f)); f[i] = '"' + f[i]; return ','.join(f)
    if k == 4:
        f = list(fields); i = r.randrange(len(f)); f[i] = f[i][:1] + '"' + f[i][1:]; return ','.join(f)
    f = list(fields); i = r.randrange(len(f)); f[i] = '"' + f[i] + '""'; return ','.join(f)


def split_csv(line):
    # input is generator output: well formed or malformed; we only perturb well-formed lines
    parts = line.split(',')
    out = []
    for p in parts:
        if len(p) >= 2 and p[0] == '"' and p[-1] == '"' and '"' not in p[1:-1]:
            out.append(p[1:-1])
        elif '"' in p:
            return None
        else:
            out.append(p)
    return out


def read_rows(folder, ncol):
    rows, bad = [], []
    for fn in sorted(os.listdir(folder)):
        with open(os.path.join(folder, fn), encoding='utf-8', newline='') as fh:
            data = fh.read()
        lines = data.split('\n')
        if lines and lines[-1] == '':
            lines.pop()
        for ln in lines[1:]:
            f = split_csv(ln)
            if f is None or len(f) != ncol:
                bad.append(ln)
            else:
                rows.append(f)
    return rows, bad


def mangle_id(v, r):
    k = r.randrange(5)
    if k == 0: return ' ' + v
    if k == 1: return v + ' '
    if k == 2: return v.upper()
    if k == 3: return v + 'é'
    return '\U0001F600' + v


def write_split(folder, header, lines, r, nfiles, nheader_only):
    os.makedirs(folder)
    r.shuffle(lines)
    weights = [1.0 / (i + 1) for i in range(nfiles)]
    r.shuffle(weights)
    tot = sum(weights)
    cuts, acc = [], 0.0
    for w in weights[:-1]:
        acc += w / tot
        cuts.append(int(acc * len(lines)))
    chunks, prev = [], 0
    for c in cuts + [len(lines)]:
        chunks.append(lines[prev:c]); prev = c
    chunks += [[] for _ in range(nheader_only)]
    r.shuffle(chunks)
    for i, ch in enumerate(chunks):
        with open(os.path.join(folder, 'part-%05d.csv' % i), 'w', encoding='utf-8', newline='') as fh:
            fh.write(header + '\n')
            for ln in ch:
                fh.write(ln + '\n')


def main():
    if os.path.exists(DST):
        shutil.rmtree(DST)
    os.makedirs(DST)
    for f in ('products.csv', 'campaigns.csv', 'manifest.json'):
        shutil.copy(os.path.join(SRC, f), DST)
    import json
    man = json.load(open(os.path.join(SRC, 'manifest.json')))
    ws = parse_ts(man['window_start']); we = parse_ts(man['window_end'])

    # ---- events
    rows, bad = read_rows(os.path.join(SRC, 'events'), 7)
    out = [b for b in bad]
    # extra duplicate groups where the earliest copy is invalid and a later one valid
    extra = []
    for f in R.sample(rows, min(len(rows), 4000)):
        ing = parse_ts(f[3])
        if ing is None:
            continue
        g = list(f); g[3] = fmt_f1(ing - R.choice([1000, 60000])); g[2] = bad_ts(parse_ts(f[2]) or ws, R, ws, we) if R.random() < 0.5 else g[2]
        if R.random() < 0.5: g[1] = ''
        if R.random() < 0.3: g[4] = 'Purchase'
        extra.append(g)
    # exact-copy duplicates (same ingest_ts -> must be identical values)
    for f in R.sample(rows, min(len(rows), 3000)):
        extra.append(list(f))
    rows += extra
    seen = {}
    for f in rows:
        r = R
        eid, uid, ts, ing, et, pid, price = f
        key_r = vrng('e|' + eid + '|' + ing)  # value perturbations keyed on (event_id, ingest_ts)
        ms = parse_ts(ts)
        x = key_r.random()
        if ms is not None and (ts.isdigit() or F1.match(ts)):
            if x < 0.012:
                ts = bad_ts(ms, key_r, ws, we)
            elif x < 0.5:
                ts = encode_ts(ms, key_r)
        if key_r.random() < 0.004 and uid:
            uid = mangle_id(uid, key_r)
        y = key_r.random()
        if et == 'purchase' and y < 0.06:
            price = key_r.choice(['0250', '+250', ' 250', '250 ', '10000000', '10000001', '0000010000000', '00', '0', '1', '２５０', '9999999', '-1', '1e3', ''])
        elif y < 0.002:
            et = key_r.choice(['Purchase', 'VIEW', 'cart ', ' view', 'purchases', ''])
        if key_r.random() < 0.002:
            pid = key_r.choice(['', pid.upper(), pid + ' ', 'P' + pid[1:]])
        if et == 'purchase' and key_r.random() < 0.01:
            # late boundary cases
            m2 = parse_ts(ts)
            if m2 is not None and ws <= m2 < we and m2 % 1000 == 0:
                ing = fmt_f1(m2 + key_r.choice([86400000, 86401000, 86399000, -5000]))
        if key_r.random() < 0.003:
            price += '\r'
        fields = [eid, uid, ts, ing, et, pid, price]
        k = (eid, ing)
        if seen.setdefault(k, fields) != fields:
            continue  # would break "equal ingest_ts copies are identical"
        if R.random() < 0.004:
            line = malformed(fields, R)
        else:
            line = ','.join(maybe_quote(v, R, 0.35) if not v.endswith('\r') else v for v in fields)
        out.append(line)
    write_split(os.path.join(DST, 'events'), 'event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents', out, R, R.randint(150, 400), R.randint(2, 6))

    # ---- clicks
    rows, bad = read_rows(os.path.join(SRC, 'clicks'), 4)
    out = list(bad)
    camps = [l.split(',')[0] for l in open(os.path.join(SRC, 'campaigns.csv')).read().split('\n')[1:] if l]
    for f in R.sample(rows, min(len(rows), 2000)):
        out_f = list(f)
        rows.append(out_f)  # identical duplicate copies
    for f in rows:
        cid, uid, camp, ts = f
        kr = vrng('c|' + cid)
        ms = parse_ts(ts)
        if ms is not None:
            x = kr.random()
            if x < 0.01:
                ms2 = bad_ts(ms, kr, ws, we)
                ts = ms2
            elif x < 0.015:
                ts = str(ws - 7 * 86400000 - kr.choice([0, 1]))
        if kr.random() < 0.003:
            camp = kr.choice([camp.upper(), camp + ' ', 'c_999999', ''])
        if kr.random() < 0.003 and uid:
            uid = mangle_id(uid, kr)
        # per-copy: form of ts may differ only if value same; keep identical string to be safe
        if kr.random() < 0.002:
            ts += '\r'  # ts gains \r -> bad ts
        fields = [cid, uid, camp, ts]
        if R.random() < 0.004:
            line = malformed(fields, R)
        else:
            line = ','.join(maybe_quote(v, R, 0.35) if not v.endswith('\r') else v for v in fields)
        out.append(line)
    write_split(os.path.join(DST, 'clicks'), 'click_id,user_id,campaign_id,ts', out, R, R.randint(50, 150), R.randint(1, 4))

    # ---- links
    rows, bad = read_rows(os.path.join(SRC, 'links'), 2)
    out = list(bad)
    for a, b in rows:
        if R.random() < 0.05:
            a, b = b, a
        line = ','.join(maybe_quote(v, R, 0.35) for v in (a, b))
        x = R.random()
        if x < 0.004:
            line = malformed([a, b], R)
        elif x < 0.006:
            line = a + ','
        elif x < 0.01:
            out.append(','.join(maybe_quote(v, R, 0.5) for v in (b, a)))
        elif x < 0.0105:
            line += '\r'
        out.append(line)
    write_split(os.path.join(DST, 'links'), 'user_id_a,user_id_b', out, R, R.randint(20, 60), R.randint(1, 3))


main()
