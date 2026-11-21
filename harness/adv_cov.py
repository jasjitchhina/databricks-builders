#!/usr/bin/env python3
"""adv_cov.py SRC DST_PREFIX MODE... — coverage-driven variants of a generated dataset (stdlib only),
each built to drive a fallback path of fast/ that generator-shaped data never reaches (task 43).
  manycats   products.csv re-labelled into 700 distinct categories (> 256 side-table path in clean,
             > 512 => engine generic (non-packed) path); many category names share long prefixes
  manycamps  campaigns.csv extended to 6000 campaigns and ~half the clicks re-pointed at the new ones
             (> 4095 campaigns => engine generic path; campaign idx > u12)
  longwindow manifest window stretched to 30 days (ts span > 2^31 ms rel. to window_start-7d =>
             engine generic path), boundary rows move inside the window
  bigperson  one user gets 2.3M extra valid clicks (one part with >= 2^21 clicks => per-part generic
             path) plus 3000 events spread over the window (huge radix-sorted person)
All rows stay valid CSV per rules.md; correctness is judged by the oracle, so no guarantees are assumed.
"""
import json, os, random, shutil, sys
from datetime import datetime, timezone

SRC, PFX, MODES = sys.argv[1], sys.argv[2], sys.argv[3:]
R = random.Random(43)


def copy_base(dst):
    if os.path.exists(dst):
        shutil.rmtree(dst)
    shutil.copytree(SRC, dst)


def read_lines(path):
    data = open(path, 'rb').read().split(b'\n')
    if data and data[-1] == b'':
        data.pop()
    return data


def write_lines(path, lines):
    with open(path, 'wb') as f:
        f.write(b'\n'.join(lines) + b'\n')


def ms(s):
    return int(datetime.strptime(s, '%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=timezone.utc).timestamp() * 1000)


def iso(t):
    return datetime.fromtimestamp(t / 1000, tz=timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


for mode in MODES:
    dst = f'{PFX}-{mode}'
    copy_base(dst)
    if mode == 'manycats':
        lines = read_lines(os.path.join(dst, 'products.csv'))
        out = [lines[0]]
        for i, l in enumerate(lines[1:]):
            pid = l.split(b',')[0]
            c = R.randrange(700)
            name = (b'category_with_a_long_shared_prefix_%03d' % c) if c % 3 else (b'c%d' % c)
            out.append(pid + b',' + name)
        write_lines(os.path.join(dst, 'products.csv'), out)
    elif mode == 'manycamps':
        cl = read_lines(os.path.join(dst, 'campaigns.csv'))
        chans = [b'search', b'social', b'email', b'display', b'video']
        new = [b'x_%d' % i for i in range(6000 - (len(cl) - 1))]
        write_lines(os.path.join(dst, 'campaigns.csv'), cl + [n + b',' + R.choice(chans) for n in new])
        d = os.path.join(dst, 'clicks')
        for fn in sorted(os.listdir(d)):
            p = os.path.join(d, fn)
            ls = read_lines(p)
            out = ls[:1]
            for l in ls[1:]:
                f = l.split(b',')
                # copies of a click must stay identical (rules.md): decide per unquoted click_id
                if len(f) == 4:
                    h = int.from_bytes(__import__('hashlib').blake2b(f[0].strip(b'"'), digest_size=8).digest(), 'little')
                    if h & 1:
                        f[2] = new[(h >> 1) % len(new)]
                        l = b','.join(f)
                out.append(l)
            write_lines(p, out)
    elif mode == 'longwindow':
        mp = os.path.join(dst, 'manifest.json')
        m = json.load(open(mp))
        m['window_end'] = iso(ms(m['window_start']) + 30 * 86400000)
        json.dump(m, open(mp, 'w'))
    elif mode == 'bigperson':
        m = json.load(open(os.path.join(dst, 'manifest.json')))
        ws, we = ms(m['window_start']), ms(m['window_end'])
        camps = [l.split(b',')[0] for l in read_lines(os.path.join(dst, 'campaigns.csv'))[1:]]
        prods = [l.split(b',')[0] for l in read_lines(os.path.join(dst, 'products.csv'))[1:]]
        u = b'bigperson_user'
        with open(os.path.join(dst, 'clicks', 'zz-bigperson.csv'), 'wb') as f:
            f.write(b'click_id,user_id,campaign_id,ts\n')
            for i in range(2_300_000):
                t = R.randrange(ws - 7 * 86400000, we)
                f.write(b'bpk%d,%s,%s,%d\n' % (i, u, R.choice(camps), t))
        with open(os.path.join(dst, 'events', 'zz-bigperson.csv'), 'wb') as f:
            f.write(b'event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n')
            t = ws + 3600000
            for i in range(3000):
                t += R.choice([1000, 60000, 1800000, 1800001, 7200000]) // (1 if R.random() < 0.2 else 10)
                if t >= we:
                    break
                ty = R.choice([b'view', b'view', b'cart', b'purchase'])
                f.write(b'bpe%d,%s,%s,%s,%s,%s,%s\n' % (i, u, iso(t).encode() if i % 2 else b'%d' % t,
                                                       iso(t + 5000).encode(), ty, R.choice(prods),
                                                       b'%d' % R.randrange(1, 50000) if ty == b'purchase' else b''))
    else:
        sys.exit(f'unknown mode {mode}')
    print('made', dst)
