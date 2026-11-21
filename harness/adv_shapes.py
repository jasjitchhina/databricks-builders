#!/usr/bin/env python3
"""adv_shapes.py SRC DST_PREFIX — file-layout / ID-shape stress variants of a generated dataset (stdlib only).
Row content is unchanged except in -longids (consistent renaming), so the data guarantees still hold.
  <prefix>-onefile   each table in ONE file, no trailing newline at EOF
  <prefix>-manyfiles 3000 event / 800 click / 300 link files: Zipf sizes, header-only, 0-byte files,
                     ~1/3 without trailing newline, a README.txt and a subdir (must be ignored)
  <prefix>-longids   every user/link ID -> 'x'*28+'~'+id, event_id/click_id -> 24-byte shared prefix + id
                     (u128/15-byte packing edges and long-common-prefix hashing at 1M scale)
  <prefix>-sparse    clicks/ holds only a 0-byte file and a header-only file; links/ has no files at all
"""
import os, random, shutil, sys

SRC, PFX = sys.argv[1], sys.argv[2]
R = random.Random(7)
TABLES = {'events': 'event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents',
          'clicks': 'click_id,user_id,campaign_id,ts', 'links': 'user_id_a,user_id_b'}


def body_lines(table):
    out = []
    d = os.path.join(SRC, table)
    for fn in sorted(os.listdir(d)):
        data = open(os.path.join(d, fn), 'rb').read()
        ls = data.split(b'\n')
        if ls and ls[-1] == b'':
            ls.pop()
        out += ls[1:]
    return out


def base(dst):
    if os.path.exists(dst):
        shutil.rmtree(dst)
    os.makedirs(dst)
    for f in ('products.csv', 'campaigns.csv', 'manifest.json'):
        shutil.copy(os.path.join(SRC, f), dst)


def write_file(path, header, lines, trailing=True):
    with open(path, 'wb') as fh:
        fh.write(header.encode() + b'\n' + b'\n'.join(lines))
        if trailing and lines:
            fh.write(b'\n')


BODY = {t: body_lines(t) for t in TABLES}

# onefile
dst = PFX + '-onefile'; base(dst)
for t, h in TABLES.items():
    os.makedirs(os.path.join(dst, t))
    write_file(os.path.join(dst, t, 'all.csv'), h, BODY[t], trailing=False)

# manyfiles
dst = PFX + '-manyfiles'; base(dst)
for t, h in TABLES.items():
    n = {'events': 3000, 'clicks': 800, 'links': 300}[t]
    d = os.path.join(dst, t); os.makedirs(d)
    lines = list(BODY[t]); R.shuffle(lines)
    w = [1.0 / (i + 1) for i in range(n)]; R.shuffle(w); tot = sum(w)
    prev, acc = 0, 0.0
    for i, x in enumerate(w):
        acc += x / tot
        c = len(lines) if i == n - 1 else int(acc * len(lines))
        write_file(os.path.join(d, 'p%05d.csv' % i), h, lines[prev:c], trailing=R.random() > 0.33); prev = c
    for i in range(5):
        write_file(os.path.join(d, 'hdr%d.csv' % i), h, [])
        open(os.path.join(d, 'zero%d.csv' % i), 'wb').close()
    open(os.path.join(d, 'README.txt'), 'w').write('not,a,csv\n1,2,3\n')
    os.makedirs(os.path.join(d, 'sub')); write_file(os.path.join(d, 'sub', 'x.csv'), h, lines[:50])

# longids
def split(ln):
    return ln.split(b',')


def mapid(v):
    if not v:
        return v
    q = v[:1] == b'"' and v[-1:] == b'"' and len(v) >= 2
    inner = v[1:-1] if q else v
    if not inner or b'"' in inner:
        return v
    out = b'x' * 28 + b'~' + inner
    return b'"' + out + b'"' if q else out


def mapkey(v):
    if not v:
        return v
    q = v[:1] == b'"' and v[-1:] == b'"' and len(v) >= 2
    inner = v[1:-1] if q else v
    if not inner or b'"' in inner:
        return v
    out = b'key-0123456789abcdefghij' + inner
    return b'"' + out + b'"' if q else out


dst = PFX + '-longids'; base(dst)
cols = {'events': {0: mapkey, 1: mapid}, 'clicks': {0: mapkey, 1: mapid}, 'links': {0: mapid, 1: mapid}}
for t, h in TABLES.items():
    os.makedirs(os.path.join(dst, t))
    out = []
    for ln in BODY[t]:
        f = split(ln)
        if len(f) == len(h.split(',')):
            for i, fn in cols[t].items():
                f[i] = fn(f[i])
            ln = b','.join(f)
        out.append(ln)
    k = 7
    for i in range(k):
        write_file(os.path.join(dst, t, 'p%d.csv' % i), h, out[i::k])

# sparse
dst = PFX + '-sparse'; base(dst)
os.makedirs(os.path.join(dst, 'events')); os.makedirs(os.path.join(dst, 'clicks')); os.makedirs(os.path.join(dst, 'links'))
write_file(os.path.join(dst, 'events', 'p0.csv'), TABLES['events'], BODY['events'])
open(os.path.join(dst, 'clicks', 'zero.csv'), 'wb').close()
write_file(os.path.join(dst, 'clicks', 'hdr.csv'), TABLES['clicks'], [])
print('wrote', PFX + '-{onefile,manyfiles,longids,sparse}', file=sys.stderr)
