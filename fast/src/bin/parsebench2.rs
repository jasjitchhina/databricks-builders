//! h1 task 40: independent event-line parse kernel, standalone benchmark.
//! Usage: parsebench2 DATA_DIR
//! Compares (a) REF = h3's path rebuilt from public pieces (csv::scan_fast + ts::parse_ts_in +
//! IdKey::pack + the parse_event logic) with (b) K1.. = h1 kernels, and checks that every kernel
//! produces exactly the same record sequence and purchase side table as REF.
use fast::ids::{pack, KeyMap};
use fast::keys::IdKey;
use fast::ts::{parse_ts_in, DAY_MS};
use std::path::Path;
use std::time::Instant;

const MAX_PRICE: u64 = 10_000_000;
const NOT_OK: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Rec {
    eid: u64,
    user: u64,
    ingest: i64,
    ts: u32,
    pc: u32,
}

struct Ctx {
    ws: i64,
    we: i64,
    base: i64,
    products: KeyMap,
    unknown: u16,
}

#[derive(Default)]
struct Out {
    recs: Vec<Rec>,
    side: Vec<(u32, u16)>,
    wide: usize, // lines where an ID is not representable as u64 (h3 then reruns the file wide)
}

impl Out {
    fn clear(&mut self) {
        self.recs.clear();
        self.side.clear();
        self.wide = 0;
    }
}

#[inline(always)]
fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v * 10 + d as u64;
        if v > MAX_PRICE {
            return None;
        }
    }
    if v == 0 { None } else { Some(v as u32) }
}

/// Exactly clean::parse_event::<u64> (logic copied; h3's file is not edited).
#[inline(always)]
fn ref_event(m: &[u8], f: [&[u8]; 7], c: &Ctx, o: &mut Out) {
    let [eid, user, ts, ing, typ, prod, price] = f;
    if eid.is_empty() {
        return;
    }
    let ingest = parse_ts_in(m, ing).unwrap_or(i64::MAX);
    let Some(ek) = <u64 as IdKey>::pack(m, eid) else { o.wide += 1; return };
    let mut rec = Rec { eid: ek, user: 0, ingest, ts: NOT_OK, pc: 0 };
    'v: {
        if user.is_empty() || prod.is_empty() {
            break 'v;
        }
        let Some(t) = parse_ts_in(m, ts) else { break 'v };
        if t < c.ws || t >= c.we {
            break 'v;
        }
        if ingest == i64::MAX || ingest - t > DAY_MS {
            break 'v;
        }
        let pc = match typ {
            b"view" | b"cart" => 0,
            b"purchase" => {
                let Some(p) = parse_price(price) else { break 'v };
                let cat = c.products.get(fast::csv::pack_in(m, prod)).map_or(c.unknown, |v| v as u16);
                o.side.push((p, cat));
                o.side.len() as u32
            }
            _ => break 'v,
        };
        let Some(uk) = <u64 as IdKey>::pack(m, user) else { o.wide += 1; return };
        rec.user = uk;
        rec.pc = pc;
        rec.ts = (t - c.base) as u32;
    }
    o.recs.push(rec);
}

fn k_ref(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    fast::csv::scan_fast::<7, _>(m, 0, n, |f| ref_event(m, f, c, o));
}

// ---------------------------------------------------------------- h1 scanner
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn masks96(p: *const u8) -> (u128, u128, u128) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let d = _mm256_loadu_si256(p.add(64) as *const __m256i);
    let cm = _mm256_set1_epi8(b',' as i8);
    let nm = _mm256_set1_epi8(b'\n' as i8);
    let qm = _mm256_set1_epi8(b'"' as i8);
    let mm = |x: __m256i, k: __m256i| (_mm256_movemask_epi8(_mm256_cmpeq_epi8(x, k)) as u32) as u128;
    let c = mm(a, cm) | mm(b, cm) << 32 | mm(d, cm) << 64;
    let nl = mm(a, nm) | mm(b, nm) << 32 | mm(d, nm) << 64;
    let q = mm(a, qm) | mm(b, qm) << 32 | mm(d, qm) << 64;
    (c, nl, q)
}

/// h1 scanner: one 96-byte window per line (every generator line is < 100 bytes); lines that do
/// not end inside the window go to csv::split. Field bounds from tzcnt over the comma mask; quotes
/// checked with bit tests on the quote mask. Same accept/strip semantics as csv::scan/scan_fast.
#[cfg(target_feature = "avx2")]
#[inline(always)]
fn scan96<'a, F: FnMut([&'a [u8]; 7])>(m: &'a [u8], n: usize, mut ev: F) {
    let mut p = 0usize;
    while p < n {
        let (cm, nl, q) = unsafe { masks96(m.as_ptr().add(p)) };
        if nl == 0 || p + (nl.trailing_zeros() as usize) > n {
            let e = memchr::memchr(b'\n', &m[p..n]).map_or(n, |x| p + x);
            if let Some(f) = fast::csv::split::<7>(&m[p..e]) {
                ev(f);
            }
            p = e + 1;
            continue;
        }
        let e = nl.trailing_zeros() as usize;
        let below = (1u128 << e) - 1;
        let mut cc = cm & below;
        let next = p + e + 1;
        if cc.count_ones() != 6 {
            p = next;
            continue;
        }
        let mut b = [0usize; 8];
        for k in 1..7 {
            b[k] = cc.trailing_zeros() as usize + 1; // start of field k
            cc &= cc - 1;
        }
        b[7] = e + 1;
        let qq = q & below;
        let mut f: [&[u8]; 7] = [&[]; 7];
        if qq == 0 {
            for k in 0..7 {
                f[k] = unsafe { m.get_unchecked(p + b[k]..p + b[k + 1] - 1) };
            }
        } else {
            let mut wsum = 0u32;
            for k in 0..7 {
                let (s, en) = (b[k], b[k + 1] - 1); // [s, en)
                let w = ((en >= s + 2) & ((qq >> s) & 1 == 1) & ((qq >> (en.max(1) - 1)) & 1 == 1)) as usize;
                wsum += 2 * w as u32;
                f[k] = unsafe { m.get_unchecked(p + s + w..p + en - w) };
            }
            if wsum != qq.count_ones() {
                p = next;
                continue;
            }
        }
        ev(f);
        p = next;
    }
}

#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let mm = |x: __m256i, y: __m256i, k: u8| {
        let kk = _mm256_set1_epi8(k as i8);
        (_mm256_movemask_epi8(_mm256_cmpeq_epi8(x, kk)) as u32) as u64 | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(y, kk)) as u32) as u64) << 32
    };
    (mm(a, b, b','), mm(a, b, b'\n'), mm(a, b, b'"'))
}

/// Streaming variant: structural masks computed once per 64-byte block, held in a 2-block
/// (128-bit) window that slides with the line start. A line that does not end inside the window
/// takes the masks96 path at its own start (exactly as scan96).
#[cfg(target_feature = "avx2")]
#[inline(always)]
fn scan_str<'a, F: FnMut([&'a [u8]; 7])>(m: &'a [u8], n: usize, mut ev: F) {
    let mut p = 0usize;
    let mut blk = 0usize; // window covers [blk, blk + 128)
    let (mut c0, mut n0, mut q0) = unsafe { masks64(m.as_ptr()) };
    let (mut c1, mut n1, mut q1) = unsafe { masks64(m.as_ptr().add(64)) };
    while p < n {
        while p >= blk + 64 {
            blk += 64;
            (c0, n0, q0) = (c1, n1, q1);
            (c1, n1, q1) = unsafe { masks64(m.as_ptr().add(blk + 64)) };
        }
        let off = (p - blk) as u32;
        let cm = ((c0 as u128) | (c1 as u128) << 64) >> off;
        let nl = ((n0 as u128) | (n1 as u128) << 64) >> off;
        let q = ((q0 as u128) | (q1 as u128) << 64) >> off;
        let e = nl.trailing_zeros() as usize; // 128 if none in the window
        if e >= 128 - off as usize || p + e > n {
            // line not inside the window: per-line path
            let (cm, nl, q) = unsafe { masks96(m.as_ptr().add(p)) };
            if nl == 0 || p + (nl.trailing_zeros() as usize) > n {
                let e = memchr::memchr(b'\n', &m[p..n]).map_or(n, |x| p + x);
                if let Some(f) = fast::csv::split::<7>(&m[p..e]) {
                    ev(f);
                }
                p = e + 1;
                continue;
            }
            let e = nl.trailing_zeros() as usize;
            p = line(m, p, e, cm, q, &mut ev);
            continue;
        }
        p = line(m, p, e, cm, q, &mut ev);
    }
}

#[inline(always)]
fn line<'a, F: FnMut([&'a [u8]; 7])>(m: &'a [u8], p: usize, e: usize, cm: u128, q: u128, ev: &mut F) -> usize {
    let below = (1u128 << e) - 1;
    let mut cc = cm & below;
    let next = p + e + 1;
    if cc.count_ones() != 6 {
        return next;
    }
    let mut b = [0usize; 8];
    for k in 1..7 {
        b[k] = cc.trailing_zeros() as usize + 1;
        cc &= cc - 1;
    }
    b[7] = e + 1;
    let qq = q & below;
    let mut f: [&[u8]; 7] = [&[]; 7];
    if qq == 0 {
        for k in 0..7 {
            f[k] = unsafe { m.get_unchecked(p + b[k]..p + b[k + 1] - 1) };
        }
    } else {
        let mut wsum = 0u32;
        for k in 0..7 {
            let (s, en) = (b[k], b[k + 1] - 1);
            let w = ((en >= s + 2) & ((qq >> s) & 1 == 1) & ((qq >> (en.max(1) - 1)) & 1 == 1)) as usize;
            wsum += 2 * w as u32;
            f[k] = unsafe { m.get_unchecked(p + s + w..p + en - w) };
        }
        if wsum != qq.count_ones() {
            return next;
        }
    }
    ev(f);
    next
}

fn a_scan_str(m: &[u8], n: usize, _c: &Ctx, o: &mut Out) {
    let mut x = 0usize;
    scan_str(m, n, |f| x += f[2].len() + f[6].len());
    o.wide = x;
}
fn k2(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    scan_str(m, n, |f| ref_event(m, f, c, o));
}

fn k1(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    scan96(m, n, |f| ref_event(m, f, c, o));
}
/// ablations (timing only; results are not records)
fn a_scan96(m: &[u8], n: usize, _c: &Ctx, o: &mut Out) {
    let mut x = 0usize;
    scan96(m, n, |f| x += f[2].len() + f[6].len());
    o.wide = x;
}
fn a_scanfast(m: &[u8], n: usize, _c: &Ctx, o: &mut Out) {
    let mut x = 0usize;
    fast::csv::scan_fast::<7, _>(m, 0, n, |f| x += f[2].len() + f[6].len());
    o.wide = x;
}
fn a_scan_ts(m: &[u8], n: usize, _c: &Ctx, o: &mut Out) {
    let mut x = 0i64;
    scan96(m, n, |f| x = x.wrapping_add(parse_ts_in(m, f[2]).unwrap_or(0) ^ parse_ts_in(m, f[3]).unwrap_or(0)));
    o.wide = x as usize;
}
fn a_scan_ts_pk(m: &[u8], n: usize, _c: &Ctx, o: &mut Out) {
    let mut x = 0i64;
    scan96(m, n, |f| x = x.wrapping_add(parse_ts_in(m, f[2]).unwrap_or(0) ^ parse_ts_in(m, f[3]).unwrap_or(0) ^ <u64 as IdKey>::pack(m, f[0]).unwrap_or(0) as i64 ^ <u64 as IdKey>::pack(m, f[1]).unwrap_or(0) as i64));
    o.wide = x as usize;
}
fn a_full_nopush(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    let mut t = Out::default();
    let mut x = 0u64;
    scan96(m, n, |f| { ref_event(m, f, c, &mut t); if let Some(r) = t.recs.pop() { x = x.wrapping_add(r.ts as u64 ^ r.user ^ r.eid ^ r.ingest as u64) } });
    o.wide = x as usize;
}

// ---------------------------------------------------------------- h1 timestamps
const YMIN: i64 = 1900;
const YN: usize = 400;
struct Cal {
    /// days[(y-YMIN)*16 + m] = days from epoch to y-m-01 (m 1..=12), mlen same index
    days: Vec<i32>,
    mlen: Vec<u8>,
}
fn cal() -> &'static Cal {
    static C: std::sync::OnceLock<Cal> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let mut c = Cal { days: vec![0; YN * 16], mlen: vec![0; YN * 16] };
        for yi in 0..YN {
            let y = YMIN + yi as i64;
            for mo in 1..=12i64 {
                c.days[yi * 16 + mo as usize] = fast::ts::days_from_civil(y, mo, 1) as i32;
                let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
                c.mlen[yi * 16 + mo as usize] = match mo { 2 => if leap { 29 } else { 28 }, 4 | 6 | 9 | 11 => 30, _ => 31 };
            }
        }
        c
    })
}

/// F1 `YYYY-MM-DDTHH:MM:SSZ` at p (20 bytes readable). Vectorized validation + finish.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn f1(p: *const u8, cal: &Cal) -> Option<i64> {
    use std::arch::x86_64::*;
    let lo = _mm_loadu_si128(p as *const __m128i);
    let hi = _mm_loadu_si128(p.add(4) as *const __m128i);
    // one shuffle per load builds 16 lanes: 14 digits + 'Z'-free separators are checked separately
    // digits: Y0 Y1 Y2 Y3 M0 M1 D0 D1 H0 H1 (lo 0,1,2,3,5,6,8,9,11,12) m0 m1 S0 S1 (hi 10,11,13,14)
    let dig = _mm_or_si128(
        _mm_shuffle_epi8(lo, _mm_setr_epi8(0, 1, 2, 3, 5, 6, 8, 9, 11, 12, -1, -1, -1, -1, -1, -1)),
        _mm_shuffle_epi8(hi, _mm_setr_epi8(-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 10, 11, 13, 14, -1, -1)),
    );
    // separators '-' 4, '-' 7, 'T' 10, ':' 13 from lo; ':' 16 = hi 12, 'Z' 19 = hi 15
    let sep = _mm_or_si128(
        _mm_shuffle_epi8(lo, _mm_setr_epi8(-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1)),
        _mm_setzero_si128(),
    );
    let _ = sep;
    let s1 = u64::from_le_bytes(*(p as *const [u8; 8])); // YYYY-MM-
    let s2 = u64::from_le_bytes(*(p.add(8) as *const [u8; 8])); // DDTHH:MM
    let s3 = u32::from_le_bytes(*(p.add(16) as *const [u8; 4])); // :SSZ
    if (s1 & 0xFF0000FF_00000000) != 0x2D00002D_00000000 || (s2 & 0x0000FF00_00FF0000) != 0x00003A00_00540000 || (s3 & 0xFF0000FF) != 0x5A00003A {
        return None;
    }
    let d = _mm_sub_epi8(dig, _mm_setr_epi8(48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 0, 0));
    // digits 0..9 in lanes 0..13 (lanes 14,15 are 0): unsigned d <= 9
    let okd = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d));
    let v = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010A)); // u16: YY YY MM DD hh mm ss 0
    let bad = _mm_or_si128(
        _mm_cmpgt_epi16(v, _mm_setr_epi16(99, 99, 12, 31, 23, 59, 59, 0)),
        _mm_cmpgt_epi16(_mm_setr_epi16(0, 0, 1, 1, 0, 0, 0, 0), v),
    );
    if okd != 0xFFFF || _mm_movemask_epi8(bad) != 0 {
        return None;
    }
    let w = _mm_madd_epi16(v, _mm_setr_epi16(100, 1, 0, 0, 3600, 60, 1, 0)); // i32: y, 0, hh*3600+mm*60, ss
    let y = _mm_cvtsi128_si32(w) as i64;
    let md = _mm_extract_epi32(v, 1) as u32; // u16 lanes 2,3 = MM (low), DD (high)
    let (mo, dd) = ((md & 0xFFFF) as usize, (md >> 16) as i64);
    let secs = _mm_extract_epi32(w, 2) as i64 + _mm_extract_epi32(w, 3) as i64;
    let yi = (y - YMIN) as usize;
    if yi >= YN {
        return fast::ts::parse_ts(std::slice::from_raw_parts(p, 20));
    }
    let i = yi * 16 + mo;
    if dd > *cal.mlen.get_unchecked(i) as i64 {
        return None;
    }
    Some((*cal.days.get_unchecked(i) as i64 + dd - 1) * DAY_MS + secs * 1000)
}

#[inline(always)]
fn ts_h1(m: &[u8], b: &[u8], cal: &Cal) -> Option<i64> {
    if b.len() == 20 && b[19] == b'Z' {
        return unsafe { f1(b.as_ptr(), cal) };
    }
    parse_ts_in(m, b)
}

fn a_scan_ts_h1(m: &[u8], n: usize, _c: &Ctx, o: &mut Out) {
    let cal = cal();
    let mut x = 0i64;
    scan96(m, n, |f| x = x.wrapping_add(ts_h1(m, f[2], cal).unwrap_or(0) ^ ts_h1(m, f[3], cal).unwrap_or(0)));
    o.wide = x as usize;
}

/// Parse one line at p with the per-line 96-byte window; returns next line start.
#[cfg(target_feature = "avx2")]
#[inline(always)]
fn one<'a, F: FnMut([&'a [u8]; 7])>(m: &'a [u8], p: usize, n: usize, ev: &mut F) -> usize {
    let (cm, nl, q) = unsafe { masks96(m.as_ptr().add(p)) };
    if nl == 0 || p + (nl.trailing_zeros() as usize) > n {
        let e = memchr::memchr(b'\n', &m[p..n]).map_or(n, |x| p + x);
        if let Some(f) = fast::csv::split::<7>(&m[p..e]) {
            ev(f);
        }
        return e + 1;
    }
    line(m, p, nl.trailing_zeros() as usize, cm, q, ev)
}

/// K3: two independent cursors (first half / second half of the buffer, split at a line start),
/// one line from each per iteration: two dependency chains for the out-of-order core.
fn k3_with<G: FnMut(&[u8], [&[u8]; 7])>(m: &[u8], n: usize, mut g: G) {
    let mid = match memchr::memchr(b'\n', &m[n / 2..n]) { Some(x) => n / 2 + x + 1, None => n };
    let (mut a, mut b) = (0usize, mid);
    while a < mid && b < n {
        a = one(m, a, mid, &mut |f| g(m, f));
        b = one(m, b, n, &mut |f| g(m, f));
    }
    while a < mid { a = one(m, a, mid, &mut |f| g(m, f)); }
    while b < n { b = one(m, b, n, &mut |f| g(m, f)); }
}
fn k3(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    k3_with(m, n, |m, f| ref_event(m, f, c, o));
}
fn k3ts(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    let cal = cal();
    k3_with(m, n, |m, f| h1_event(m, f, c, o, cal));
}
fn k1ts(m: &[u8], n: usize, c: &Ctx, o: &mut Out) {
    let cal = cal();
    fast::csv::scan_fast::<7, _>(m, 0, n, |f| h1_event(m, f, c, o, cal));
}

/// ref_event with h1 timestamps
#[inline(always)]
fn h1_event(m: &[u8], f: [&[u8]; 7], c: &Ctx, o: &mut Out, cal: &Cal) {
    let [eid, user, ts, ing, typ, prod, price] = f;
    if eid.is_empty() {
        return;
    }
    let ingest = ts_h1(m, ing, cal).unwrap_or(i64::MAX);
    let Some(ek) = <u64 as IdKey>::pack(m, eid) else { o.wide += 1; return };
    let mut rec = Rec { eid: ek, user: 0, ingest, ts: NOT_OK, pc: 0 };
    'v: {
        if user.is_empty() || prod.is_empty() {
            break 'v;
        }
        let Some(t) = ts_h1(m, ts, cal) else { break 'v };
        if t < c.ws || t >= c.we {
            break 'v;
        }
        if ingest == i64::MAX || ingest - t > DAY_MS {
            break 'v;
        }
        let pc = match typ {
            b"view" | b"cart" => 0,
            b"purchase" => {
                let Some(p) = parse_price(price) else { break 'v };
                let cat = c.products.get(fast::csv::pack_in(m, prod)).map_or(c.unknown, |v| v as u16);
                o.side.push((p, cat));
                o.side.len() as u32
            }
            _ => break 'v,
        };
        let Some(uk) = <u64 as IdKey>::pack(m, user) else { o.wide += 1; return };
        rec.user = uk;
        rec.pc = pc;
        rec.ts = (t - c.base) as u32;
    }
    o.recs.push(rec);
}

/// order-insensitive canonical form (pc resolved to its side entry)
fn canon(o: &Out) -> Vec<(u64, u64, i64, u32, u32, u16)> {
    let mut v: Vec<_> = o.recs.iter().map(|r| { let (p, c) = if r.pc == 0 { (0, 0) } else { o.side[r.pc as usize - 1] }; (r.eid, r.user, r.ingest, r.ts, p, c) }).collect();
    v.sort_unstable();
    v
}

fn load(dir: &Path) -> (Vec<u8>, usize, usize, Ctx) {
    let man = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    let grab = |k: &str| -> i64 {
        let i = man.find(k).unwrap() + k.len();
        let r = &man[i..];
        let q1 = r.find('"').unwrap() + 1;
        let q2 = q1 + r[q1..].find('"').unwrap();
        fast::ts::parse_ts(r[q1..q2].as_bytes()).unwrap()
    };
    let (ws, we) = (grab("\"window_start\""), grab("\"window_end\""));
    let mut products = KeyMap::with_capacity(60000);
    let pr = std::fs::read(dir.join("products.csv")).unwrap();
    let mut cats: Vec<Vec<u8>> = Vec::new();
    for line in pr.split(|&b| b == b'\n').skip(1) {
        if let Some([p, cat]) = fast::csv::split::<2>(line) {
            let ci = match cats.iter().position(|x| x == cat) { Some(i) => i, None => { cats.push(cat.to_vec()); cats.len() - 1 } };
            products.insert(pack(p), ci as u128);
        }
    }
    let mut data = Vec::new();
    for f in fast::csv::list_csv(&dir.join("events")) {
        let b = std::fs::read(f).unwrap();
        if let Some(h) = memchr::memchr(b'\n', &b) {
            data.extend_from_slice(&b[h + 1..]);
            if !data.is_empty() && *data.last().unwrap() != b'\n' {
                data.push(b'\n');
            }
        }
    }
    let n = data.len();
    data.extend_from_slice(&[0u8; 256]);
    let lines = memchr::memchr_iter(b'\n', &data[..n]).count();
    (data, n, lines, Ctx { ws, we, base: ws - 7 * DAY_MS, products, unknown: cats.len() as u16 })
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: parsebench2 DATA_DIR");
    let (data, n, lines, ctx) = load(Path::new(&dir));
    let m = &data[..];
    let reps: usize = std::env::var("REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(7);
    let mut want = Out::default();
    k_ref(m, n, &ctx, &mut want);
    eprintln!("lines {lines}, recs {}, purchases {}, wide {}", want.recs.len(), want.side.len(), want.wide);
    let kernels: Vec<(&str, fn(&[u8], usize, &Ctx, &mut Out))> = vec![("REF(h3)", k_ref), ("K1", k1), ("K2str", k2), ("K1ts", k1ts), ("K3ilp", k3), ("K3ilp+ts", k3ts), ("a:scanfast", a_scanfast), ("a:scan96", a_scan96), ("a:scanstr", a_scan_str), ("a:+ts", a_scan_ts), ("a:+ts_h1", a_scan_ts_h1), ("a:+ts+pk", a_scan_ts_pk), ("a:nopush", a_full_nopush)];
    let only = std::env::var("ONLY").ok();
    for (name, k) in kernels {
        if only.as_deref().is_some_and(|o| o != name) { continue; }
        let mut o = Out { recs: Vec::with_capacity(want.recs.len()), side: Vec::with_capacity(want.side.len()), wide: 0 };
        let mut best = f64::MAX;
        for _ in 0..reps {
            o.clear();
            let t = Instant::now();
            k(m, n, &ctx, &mut o);
            best = best.min(t.elapsed().as_secs_f64());
        }
        if name.starts_with("a:") {
            eprintln!("{:<10} {:>6.1} ns/line  (ablation)", name, best * 1e9 / lines as f64);
            continue;
        }
        let same = if name.starts_with("K3") { canon(&o) == canon(&want) && o.wide == want.wide } else { o.recs == want.recs && o.side == want.side && o.wide == want.wide };
        if !same {
            let i = o.recs.iter().zip(want.recs.iter()).position(|(a, b)| a != b);
            eprintln!("  {name}: MISMATCH (recs {} vs {}, first diff at {:?})", o.recs.len(), want.recs.len(), i);
            if let Some(i) = i { eprintln!("    got  {:?}\n    want {:?}", o.recs[i], want.recs[i]); }
        }
        eprintln!("{:<8} {:>6.1} ns/line  {}", name, best * 1e9 / lines as f64, if same { "EXACT" } else { "WRONG" });
    }
}
