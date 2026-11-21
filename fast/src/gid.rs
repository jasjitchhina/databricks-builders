//! Generator-style IDs -> dense numbers (for the C1 redesign; pure functions, no wiring).
//!
//! The generator writes every ID as `prefix + lowercase base36(scramble(n, key))`, where
//! `scramble` is a fixed bijection on `0..2^40` and `key` is a per-dataset, per-kind 40-bit
//! value (event_id `e`, click_id `k`, user_id `u`). Event numbers are dense `0..E`, click
//! numbers dense `0..C`, user numbers `person * 512 + alias` (kiosk IDs at `n >= 2^39`).
//!
//! EXACTNESS does not depend on the key or on the data being generator data:
//! * `b36` returns `Some(v)` iff the body is the canonical lowercase base36 spelling of `v`
//!   (`encode_b36(v) == body`), so on canonical IDs body -> value is injective;
//! * `unscramble(., key)` is a bijection on `0..2^40` for every key;
//! hence for a fixed prefix and ANY key, `id_n` is injective on the IDs it accepts, and
//! `encode(prefix, id_n(id), key) == id` gives the bytes back. Everything it rejects
//! (`None`: case variants, leading zeros, wrong length, other characters, other prefix,
//! value >= 2^40) must take the generic path. The key only decides whether the numbers come
//! out dense; a wrong key gives numbers spread over `0..2^40` (still exact, just not dense),
//! so callers must also range-check `n` against their table size.

pub const BITS: u32 = 40;
pub const MASK: u64 = (1 << BITS) - 1;
const M1: u64 = 0x9E37_79B9_7F4B;
const M2: u64 = 0xD6E8_FEB8_6659;
const IM1: u64 = inv40(M1);
const IM2: u64 = inv40(M2);

/// Inverse of an odd number modulo 2^40 (Newton iteration).
const fn inv40(a: u64) -> u64 {
    let mut x = a; // correct to 3 bits for odd a
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(x)));
        i += 1;
    }
    x & MASK
}

/// The generator's scramble (generator/src/text.rs), bit for bit.
#[inline(always)]
pub fn scramble(n: u64, key: u64) -> u64 {
    let mut m = n & MASK;
    m = (m.wrapping_mul(M1) ^ key) & MASK;
    m ^= m >> 17;
    m.wrapping_mul(M2) & MASK
}

/// `y = (n * M1 ^ key) & MASK`: the key-independent half of the inverse.
#[inline(always)]
pub fn unmix(v: u64) -> u64 {
    let m = v.wrapping_mul(IM2) & MASK;
    m ^ (m >> 17) ^ (m >> 34)
}

/// Exact inverse of `scramble` for the same key (and any key: a bijection on 0..2^40).
#[inline(always)]
pub fn unscramble(v: u64, key: u64) -> u64 {
    ((unmix(v) ^ key) & MASK).wrapping_mul(IM1) & MASK
}

/// base36 digit value of a byte: '0'-'9' -> 0..9, 'a'-'z' -> 10..35, else 0xFF.
const fn b36_table() -> [u8; 256] {
    let mut t = [0xFFu8; 256];
    let mut c = 0usize;
    while c < 256 {
        let b = c as u8;
        if b.is_ascii_digit() {
            t[c] = b - b'0';
        } else if b.is_ascii_lowercase() {
            t[c] = b - b'a' + 10;
        }
        c += 1;
    }
    t
}
static B36: [u8; 256] = b36_table();

/// Canonical lowercase base36 body -> value. `Some(v)` iff `encode_b36(v) == body` and v < 2^40:
/// 1..=8 chars of [0-9a-z], no leading '0' unless the body is exactly "0".
#[inline]
pub fn b36(body: &[u8]) -> Option<u64> {
    let n = body.len();
    if n == 0 || n > 8 || (n > 1 && body[0] == b'0') {
        return None;
    }
    let mut v = 0u64;
    let mut bad = 0u8;
    for &c in body {
        let d = B36[c as usize];
        bad |= d;
        v = v * 36 + (d as u64 & 63);
    }
    // valid digits are < 36 (bit 7 clear); 0xFF has bit 7 set
    if bad & 0x80 != 0 || v > MASK {
        None
    } else {
        Some(v)
    }
}

/// Canonical lowercase base36 spelling of v (the generator's `base36`).
pub fn encode_b36(out: &mut Vec<u8>, v: u64) {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [0u8; 13];
    let mut at = buf.len();
    let mut rest = v;
    loop {
        at -= 1;
        buf[at] = DIGITS[(rest % 36) as usize];
        rest /= 36;
        if rest == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[at..]);
}

/// ID bytes -> n, or None (not canonical for `prefix`: generic path).
#[inline]
pub fn id_n(id: &[u8], prefix: u8, key: u64) -> Option<u64> {
    if id.first() != Some(&prefix) {
        return None;
    }
    b36(&id[1..]).map(|v| unscramble(v, key))
}

/// Exact inverse of `id_n`: appends `prefix + base36(scramble(n, key))`.
pub fn encode(out: &mut Vec<u8>, prefix: u8, n: u64, key: u64) {
    out.push(prefix);
    encode_b36(out, scramble(n, key));
}

// ---- compact keys (keys.rs u64: 6-bit codes, big-endian from bit 63, zero padded) ----
// codes: '0'-'9' 1..10, 'A'-'Z' 11..36, '_' 37, 'a'-'z' 38..63

/// keys.rs compact code of an ASCII byte (0 if not representable).
pub const fn compact_code(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0' + 1,
        b'A'..=b'Z' => b - b'A' + 11,
        b'_' => 37,
        b'a'..=b'z' => b - b'a' + 38,
        _ => 0,
    }
}

/// base36 digit of a compact code, 0xFF if not [0-9a-z] (code 0 = padding is handled apart).
const fn code_b36_table() -> [u8; 64] {
    let mut t = [0xFFu8; 64];
    let mut c = 1usize;
    while c < 64 {
        if c <= 10 {
            t[c] = (c - 1) as u8;
        } else if c >= 38 {
            t[c] = (c - 38 + 10) as u8;
        }
        c += 1;
    }
    t
}
static CODE_B36: [u8; 64] = code_b36_table();

/// Same as `b36(&id[1..])` (after the prefix check) but from a compact key; never re-reads bytes.
/// Branch-light SWAR version (no tables, no per-digit loop); equal to `compact_b36_scalar` on every u64.
#[inline(always)]
pub fn compact_b36(k: u64, prefix: u8) -> Option<u64> {
    const B: u64 = 0x0101_0101_0101_0101;
    const H: u64 = 0x8080_8080_8080_8080;
    let pc = compact_code(prefix) as u64;
    // 9 body codes: c1 at bits 52..57 ... c8 at 10..15, c9 at 4..9 and bits 0..3 (must be 0: <= 8 digits)
    let x = (k >> 10) & ((1u64 << 48) - 1); // c1..c8, c8 lowest
    // spread 8 x 6-bit codes into 8 bytes: byte j = c(8-j)
    let mut t = (x & 0xFF_FFFF) | ((x & 0xFFFF_FF00_0000) << 8);
    t = (t & 0x0000_0FFF_0000_0FFF) | ((t & 0x00FF_F000_00FF_F000) << 4);
    t = (t & 0x003F_003F_003F_003F) | ((t & 0x0FC0_0FC0_0FC0_0FC0) << 2);
    let tz_bytes = t.trailing_zeros() / 8; // codes are contiguous then zero padding
    let ok_shape = (k >> 58) == pc && pc != 0 && k & 0x3FF == 0 && t != 0;
    let y = t.checked_shr(8 * tz_bytes).unwrap_or(0); // right-aligned: byte 0 = last digit
    let len = 8 - tz_bytes; // number of body codes (valid when t != 0)
    let low = if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 };
    // per byte (all < 64, so +127 never carries across bytes)
    let nz = (y + 0x7F * B) & H; // code != 0
    let ge11 = (y + (128 - 11) * B) & H;
    let ge38 = (y + (128 - 38) * B) & H;
    let invalid = (ge11 & !ge38) | (!nz & H & low); // 'A'-'Z', '_', or an interior zero code
    let d = y - (nz >> 7) - (ge38 >> 7) * 27; // '0'-'9' code 1..10 -> 0..9, 'a'-'z' 38..63 -> 10..35
    let p = (d & 0x00FF_00FF_00FF_00FF) + ((d >> 8) & 0x00FF_00FF_00FF_00FF) * 36;
    let q = (p & 0x0000_FFFF_0000_FFFF) + ((p >> 16) & 0x0000_FFFF_0000_FFFF) * 1296;
    let v = (q & 0xFFFF_FFFF) + (q >> 32) * 1_679_616;
    let lead0 = len > 1 && (k >> 52) & 63 == 1;
    if ok_shape & (invalid == 0) & !lead0 & (v <= MASK) {
        Some(v)
    } else {
        None
    }
}

/// Reference (per-digit loop) for `compact_b36`.
pub fn compact_b36_scalar(k: u64, prefix: u8) -> Option<u64> {
    if (k >> 58) as u8 != compact_code(prefix) || compact_code(prefix) == 0 {
        return None;
    }
    let body = k << 6; // codes 1.. from bit 63; padding is zero, and a zero code ends the ID
    if body == 0 {
        return None; // prefix only
    }
    // body length in codes: codes are contiguous then zero padding
    let len = ((64 - body.trailing_zeros() as usize) + 5) / 6;
    if len > 8 {
        return None;
    }
    let mut v = 0u64;
    let mut bad = 0u8;
    for i in 0..len {
        let c = ((body >> (58 - 6 * i)) & 63) as usize;
        let d = if c == 0 { 0xFF } else { CODE_B36[c] }; // interior zero cannot occur; be safe
        bad |= d;
        v = v * 36 + (d as u64 & 63);
    }
    if bad & 0x80 != 0 || v > MASK || (len > 1 && (body >> 58) == 1) {
        None
    } else {
        Some(v)
    }
}

/// Compact key -> n, or None (generic path).
#[inline]
pub fn compact_n(k: u64, prefix: u8, key: u64) -> Option<u64> {
    compact_b36(k, prefix).map(|v| unscramble(v, key))
}

// ---- key recovery ----

/// Minimum sample for `recover_key` (verification needs >= 90% of these to map below n_bound).
pub const MIN_SAMPLE: usize = 64;

/// Recover the dataset key from `vals` = `b36` values of sampled canonical IDs of ONE kind.
/// Assumes some anchor among the first `anchors` vals has `n = t * stride` with `t < enum_bound`
/// (events/clicks: stride 1, enum_bound = n_bound >= max n, e.g. next_pow2(rows);
/// users: stride 512 (alias 0, ~77% of rows), enum_bound >= persons, n_bound = enum_bound*512).
/// Accepts a key only if >= 90% of up to 256 sampled vals unscramble below `n_bound`
/// (a wrong key maps each val uniformly over 2^40: false acceptance < 2^-300 for n_bound <= 2^36).
/// Cost: <= anchors * enum_bound (mul+xor+cmp); returns at the first verified key.
/// None: not generator-like data (or bounds too small) -> caller keeps the generic pipeline.
pub fn recover_key(vals: &[u64], stride: u64, enum_bound: u64, n_bound: u64, anchors: usize) -> Option<u64> {
    recover_key_range(vals, stride, 0, enum_bound, n_bound, anchors)
}

/// `recover_key` restricted to `t in t_lo..t_hi` (split across threads; any hit is verified).
pub fn recover_key_range(vals: &[u64], stride: u64, t_lo: u64, t_hi: u64, n_bound: u64, anchors: usize) -> Option<u64> {
    if vals.len() < MIN_SAMPLE || n_bound == 0 || stride == 0 {
        return None;
    }
    let ys: Vec<u64> = vals.iter().take(256).map(|&v| unmix(v & MASK)).collect();
    let step = stride.wrapping_mul(M1) & MASK;
    for a in 0..anchors.min(ys.len()) {
        let ya = ys[a];
        // quick filter on the next two sampled ids (either may be a kiosk id: then rely on
        // another anchor); d = y_b ^ y_a, n_b = ((d ^ x) * IM1) & MASK with x = n_a * M1
        let b1 = ys[(a + 1) % ys.len()] ^ ya;
        let b2 = ys[(a + 2) % ys.len()] ^ ya;
        let mut x = t_lo.wrapping_mul(stride).wrapping_mul(M1) & MASK;
        for _ in t_lo..t_hi {
            if (((b1 ^ x).wrapping_mul(IM1)) & MASK) < n_bound
                && (((b2 ^ x).wrapping_mul(IM1)) & MASK) < n_bound
            {
                let k = ya ^ x;
                if verify(&ys, k, n_bound) {
                    return Some(k);
                }
            }
            x = (x + step) & MASK;
        }
    }
    None
}

fn verify(ys: &[u64], k: u64, n_bound: u64) -> bool {
    let ok = ys.iter().filter(|&&y| (((y ^ k).wrapping_mul(IM1)) & MASK) < n_bound).count();
    ok * 10 >= ys.len() * 9
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::IdKey;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    /// Reference: Some(v) iff some v < 2^40 spells exactly `body`.
    fn ref_b36(body: &[u8]) -> Option<u64> {
        if body.is_empty() || body.len() > 8 {
            return None;
        }
        let mut v: u128 = 0;
        for &c in body {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'z' => c - b'a' + 10,
                _ => return None,
            };
            v = v * 36 + d as u128;
        }
        if v > MASK as u128 {
            return None;
        }
        let mut s = Vec::new();
        encode_b36(&mut s, v as u64);
        (s == body).then_some(v as u64)
    }

    fn compact(b: &[u8]) -> Option<u64> {
        let mut buf = b.to_vec();
        buf.resize(b.len() + 32, b'#');
        <u64 as IdKey>::pack(&buf, &buf[..b.len()])
    }

    #[test]
    fn inverses() {
        assert_eq!(M1.wrapping_mul(IM1) & MASK, 1);
        assert_eq!(M2.wrapping_mul(IM2) & MASK, 1);
    }

    #[test]
    fn scramble_roundtrip() {
        let mut r = Rng(1);
        for i in 0..2_000_000u64 {
            let key = if i % 7 == 0 { 0 } else { r.next() };
            let n = match i % 5 {
                0 => i,
                1 => MASK - (i % 1000),
                _ => r.next() & MASK,
            };
            let v = scramble(n, key);
            assert!(v <= MASK);
            assert_eq!(unscramble(v, key), n, "n={n} key={key:#x}");
            assert_eq!(scramble(unscramble(n, key), key), n);
        }
    }

    #[test]
    fn scramble_is_bijection_on_small_window() {
        // injectivity spot check: 2^20 consecutive n, one key, no collisions
        let key = 0x1234_5678_9A;
        let mut v: Vec<u64> = (0..1u64 << 20).map(|n| scramble(n, key)).collect();
        v.sort_unstable();
        v.dedup();
        assert_eq!(v.len(), 1 << 20);
    }

    #[test]
    fn b36_exhaustive_short() {
        // every string of length 0..=3 over an alphabet with near-misses
        let alpha: Vec<u8> = b"0189azAZ_-/:`{ \"".iter().copied().chain([0u8, 0x80, 0xFF]).collect();
        let mut s: Vec<u8> = Vec::new();
        fn rec(alpha: &[u8], s: &mut Vec<u8>, depth: usize) {
            assert_eq!(b36(s), ref_b36(s), "{:?}", s);
            if depth == 0 {
                return;
            }
            for &c in alpha {
                s.push(c);
                rec(alpha, s, depth - 1);
                s.pop();
            }
        }
        rec(&alpha, &mut s, 3);
        // all 1-2 char strings over the full byte range
        for a in 0..=255u8 {
            assert_eq!(b36(&[a]), ref_b36(&[a]));
            for b in 0..=255u8 {
                assert_eq!(b36(&[a, b]), ref_b36(&[a, b]));
            }
        }
    }

    #[test]
    fn b36_roundtrip_and_edges() {
        let mut r = Rng(2);
        let mut s = Vec::new();
        let mut edges: Vec<u64> = vec![0, 1, 35, 36, 1295, 1296, MASK, MASK - 1, 36u64.pow(7) - 1, 36u64.pow(7)];
        for p in 1..8 {
            edges.push(36u64.pow(p));
            edges.push(36u64.pow(p) - 1);
        }
        for i in 0..500_000 {
            let v = if i < edges.len() { edges[i] } else { r.next() & MASK };
            s.clear();
            encode_b36(&mut s, v);
            assert_eq!(b36(&s), Some(v));
            assert_eq!(ref_b36(&s), Some(v));
            // near misses: uppercase one char, leading zero, extra char, truncated, swapped char
            let j = (r.next() as usize) % s.len();
            let mut t = s.clone();
            if t[j].is_ascii_lowercase() {
                t[j] = t[j].to_ascii_uppercase();
                assert_eq!(b36(&t), None);
            }
            let mut t = vec![b'0'];
            t.extend_from_slice(&s);
            assert_eq!(b36(&t), None);
            let mut t = s.clone();
            t.push(b'0' + (r.next() % 10) as u8);
            assert_eq!(b36(&t), ref_b36(&t));
            let mut t = s.clone();
            t[j] = [b'g', b'z', b'9', b'0', b'_', b'.'][(r.next() % 6) as usize];
            assert_eq!(b36(&t), ref_b36(&t));
        }
        // > 2^40 and 9 chars
        assert_eq!(b36(b"zzzzzzzz"), None);
        assert_eq!(b36(b"100000000"), None);
        let mut big = Vec::new();
        encode_b36(&mut big, MASK + 1);
        assert_eq!(b36(&big), None);
    }

    #[test]
    fn random_strings_match_reference() {
        let alpha = b"0123456789abcdefghijklmnopqrstuvwxyzAZ_0";
        let mut r = Rng(3);
        for _ in 0..2_000_000 {
            let n = (r.next() % 11) as usize;
            let s: Vec<u8> = (0..n).map(|_| alpha[(r.next() as usize) % alpha.len()]).collect();
            assert_eq!(b36(&s), ref_b36(&s), "{:?}", String::from_utf8_lossy(&s));
        }
    }

    #[test]
    fn id_n_and_encode_are_inverse() {
        let mut r = Rng(4);
        let mut out = Vec::new();
        for i in 0..300_000u64 {
            let key = r.next() & MASK;
            let n = if i % 2 == 0 { i } else { r.next() & MASK };
            for &p in b"eku" {
                out.clear();
                encode(&mut out, p, n, key);
                assert_eq!(id_n(&out, p, key), Some(n));
                assert_eq!(id_n(&out, if p == b'e' { b'k' } else { b'e' }, key), None);
                // re-encode of the decoded value gives the same bytes (bijection bytes <-> n)
                let mut back = Vec::new();
                encode(&mut back, p, id_n(&out, p, key).unwrap(), key);
                assert_eq!(back, out);
                // case variant of the prefix / any char -> None
                let mut up = out.clone();
                up[0] = up[0].to_ascii_uppercase();
                assert_eq!(id_n(&up, p, key), None);
            }
        }
        assert_eq!(id_n(b"", b'e', 0), None);
        assert_eq!(id_n(b"e", b'e', 0), None);
        assert_eq!(id_n(b"e0", b'e', 0), Some(unscramble(0, 0)));
        assert_eq!(id_n(b"e00", b'e', 0), None);
    }

    #[test]
    fn compact_matches_bytes() {
        let alpha = b"0123456789abcdefghijklmnopqrstuvwxyzEKU_";
        let mut r = Rng(5);
        let mut checked = 0;
        for i in 0..3_000_000u64 {
            let s: Vec<u8> = if i % 3 == 0 {
                let mut s = vec![b"eku"[(i % 9 / 3) as usize]];
                encode_b36(&mut s, r.next() & MASK >> (r.next() % 40));
                s
            } else {
                let n = (r.next() % 11) as usize;
                let mut s: Vec<u8> = (0..n).map(|_| alpha[(r.next() as usize) % alpha.len()]).collect();
                if n > 0 && i % 2 == 0 {
                    s[0] = b'e';
                }
                s
            };
            let Some(k) = compact(&s) else { continue };
            checked += 1;
            for &p in b"ekuE_" {
                let want = if s.first() == Some(&p) { b36(&s[1..]) } else { None };
                assert_eq!(compact_b36(k, p), want, "{:?} prefix {}", String::from_utf8_lossy(&s), p as char);
            }
        }
        assert!(checked > 1_000_000);
        // exhaustive: prefix + 0..=3 body chars over [0-9a-zA-Z_]
        let full: Vec<u8> = (b'0'..=b'9').chain(b'a'..=b'z').chain(b'A'..=b'Z').chain([b'_']).collect();
        let mut s = vec![b'e'];
        fn rec(full: &[u8], s: &mut Vec<u8>, d: usize) {
            let k = compact(s).unwrap();
            assert_eq!(compact_b36(k, b'e'), b36(&s[1..]), "{:?}", String::from_utf8_lossy(s));
            if d == 0 {
                return;
            }
            for &c in full {
                s.push(c);
                rec(full, s, d - 1);
                s.pop();
            }
        }
        rec(&full, &mut s, 3);
    }

    #[test]
    fn compact_swar_equals_scalar() {
        let mut r = Rng(11);
        let mut some = 0;
        for i in 0..20_000_000u64 {
            let k = match i % 4 {
                0 => r.next(),
                // valid-looking: prefix 'e' + random codes, random length, padded
                _ => {
                    let len = (r.next() % 10) as u32;
                    let mut k = (compact_code(b"eku"[(i % 3) as usize]) as u64) << 58;
                    for j in 0..len {
                        let c = match r.next() % 8 {
                            0 => r.next() % 64,
                            1..=3 => 1 + r.next() % 10,
                            _ => 38 + r.next() % 26,
                        };
                        k |= c << (52 - 6 * j);
                    }
                    k
                }
            };
            for &p in b"eku_0" {
                let a = compact_b36(k, p);
                assert_eq!(a, compact_b36_scalar(k, p), "k={k:#018x} p={}", p as char);
                some += a.is_some() as u32;
            }
        }
        assert!(some > 1_000_000);
    }

    /// Synthetic generator-like samples: values of ids for n drawn from 0..e (with repeats).
    fn sample(key: u64, e: u64, stride: u64, kiosk_pct: u64, r: &mut Rng, m: usize) -> Vec<u64> {
        (0..m)
            .map(|_| {
                let n = if r.next() % 100 < kiosk_pct {
                    (1u64 << 39) + r.next() % 5000
                } else if stride > 1 {
                    let alias = if r.next() % 100 < 77 { 0 } else { 1 + r.next() % 8 };
                    (r.next() % e) * stride + alias
                } else {
                    r.next() % e
                };
                scramble(n, key)
            })
            .collect()
    }

    #[test]
    fn recover_events_and_clicks() {
        let mut r = Rng(6);
        for trial in 0..20 {
            let key = r.next() & MASK;
            let e = [1000u64, 65_536, 250_000, 1_000_000][trial % 4];
            let vals = sample(key, e, 1, 0, &mut r, 256);
            let bound = e.next_power_of_two();
            let k = recover_key(&vals, 1, bound, bound, 2).expect("key");
            // any key that verifies must place the sample in range; with this sample size it is the key
            assert_eq!(k, key, "trial {trial}");
            assert!(vals.iter().all(|&v| unscramble(v, k) < e));
        }
    }

    #[test]
    fn recover_users_with_alias_and_kiosk() {
        let mut r = Rng(7);
        for trial in 0..10 {
            let key = r.next() & MASK;
            let persons = [500u64, 30_000, 200_000][trial % 3];
            let vals = sample(key, persons, 512, 2, &mut r, 256);
            let eb = persons.next_power_of_two();
            let k = recover_key(&vals, 512, eb, eb * 512, 8).expect("user key");
            assert_eq!(k, key);
        }
    }

    #[test]
    fn recover_split_ranges_agree() {
        let mut r = Rng(8);
        let key = r.next() & MASK;
        let vals = sample(key, 100_000, 1, 0, &mut r, 128);
        let b = 1u64 << 17;
        let parts: Vec<Option<u64>> = (0..4).map(|i| recover_key_range(&vals, 1, i * b / 4, (i + 1) * b / 4, b, 1)).collect();
        let hits: Vec<u64> = parts.into_iter().flatten().collect();
        assert_eq!(hits, vec![key]);
    }

    #[test]
    fn recover_rejects_non_generator_data() {
        let mut r = Rng(9);
        let vals: Vec<u64> = (0..256).map(|_| r.next() & MASK).collect();
        assert_eq!(recover_key(&vals, 1, 1 << 16, 1 << 16, 4), None);
        assert_eq!(recover_key(&vals, 512, 1 << 12, 1 << 21, 4), None);
        // too small a sample, or a bound below the real n range
        let key = 0xABCDEF0123;
        let small = sample(key, 1000, 1, 0, &mut r, 32);
        assert_eq!(recover_key(&small, 1, 1024, 1024, 2), None);
        let v = sample(key, 100_000, 1, 0, &mut r, 256);
        assert_eq!(recover_key(&v, 1, 1 << 10, 1 << 10, 2), None);
    }

    /// Real data check: `cargo test --release gid -- --ignored` (needs ~/data/dev, the seed-1 dev set).
    #[test]
    #[ignore]
    fn dev_dataset_dense() {
        let home = std::env::var("HOME").unwrap();
        let d = format!("{home}/data/dev");
        for (table, prefix, col, ncol) in [("events", b'e', 0usize, 7usize), ("clicks", b'k', 0, 4)] {
            let mut ids: Vec<Vec<u8>> = Vec::new();
            let mut files: Vec<_> = std::fs::read_dir(format!("{d}/{table}")).unwrap().map(|e| e.unwrap().path()).collect();
            files.sort();
            for f in files {
                let data = std::fs::read(f).unwrap();
                for ln in data.split(|&c| c == b'\n').skip(1) {
                    let fs: Vec<&[u8]> = ln.split(|&c| c == b',').collect();
                    if fs.len() == ncol {
                        let f = fs[col];
                        let f = if f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' { &f[1..f.len() - 1] } else { f };
                        ids.push(f.to_vec());
                    }
                }
            }
            let vals: Vec<u64> = ids.iter().filter_map(|s| if s.first() == Some(&prefix) { b36(&s[1..]) } else { None }).collect();
            let canon = vals.len();
            let step = vals.len() / 300;
            let samp: Vec<u64> = vals.iter().step_by(step.max(1)).copied().collect();
            let bound = (ids.len() as u64).next_power_of_two();
            let key = recover_key(&samp, 1, bound, bound, 2).expect("key");
            let mut ns: Vec<u64> = vals.iter().map(|&v| unscramble(v, key)).collect();
            ns.sort_unstable();
            ns.dedup();
            let max = *ns.last().unwrap();
            eprintln!("{table}: rows {} canonical {canon} distinct n {} max n {max} key {key:#x}", ids.len(), ns.len());
            // canonical spellings cover almost every n (an event whose every copy is a case
            // variant has no canonical spelling); nothing may fall outside 0..rows
            assert!(canon * 100 >= ids.len() * 99, "{table}: canonical share");
            assert!(ns.len() as u64 * 1000 >= (max + 1) * 999, "{table}: n not dense 0..max");
            assert!(max < ids.len() as u64);
        }
        // users: n = person * 512 + alias, kiosk ids at n >= 2^39
        let mut uvals = Vec::new();
        let mut urows = 0usize;
        for (table, cols, ncol) in [("events", &[1usize][..], 7usize), ("clicks", &[1][..], 4), ("links", &[0, 1][..], 2)] {
            for f in std::fs::read_dir(format!("{d}/{table}")).unwrap() {
                let data = std::fs::read(f.unwrap().path()).unwrap();
                for ln in data.split(|&c| c == b'\n').skip(1) {
                    let fs: Vec<&[u8]> = ln.split(|&c| c == b',').collect();
                    if fs.len() == ncol {
                        for &c in cols {
                            urows += 1;
                            if fs[c].first() == Some(&b'u') {
                                if let Some(v) = b36(&fs[c][1..]) {
                                    uvals.push(v);
                                }
                            }
                        }
                    }
                }
            }
        }
        let samp: Vec<u64> = uvals.iter().step_by(uvals.len() / 300).copied().collect();
        let eb = (urows as u64).next_power_of_two();
        let key = recover_key(&samp, 512, eb, eb * 512, 8).expect("user key");
        let ns: Vec<u64> = uvals.iter().map(|&v| unscramble(v, key)).collect();
        let kiosk = ns.iter().filter(|&&n| n >= 1 << 39).count();
        let mut persons: Vec<u64> = ns.iter().filter(|&&n| n < 1 << 39).map(|&n| n >> 9).collect();
        persons.sort_unstable();
        persons.dedup();
        let maxp = *persons.last().unwrap();
        let alias0 = ns.iter().filter(|&&n| n < 1 << 39 && n & 511 == 0).count();
        eprintln!("users: fields {urows} canonical {} kiosk {kiosk} alias0 {alias0} distinct persons {} max person {maxp} key {key:#x}", uvals.len(), persons.len());
        assert!(kiosk * 100 < uvals.len() && alias0 * 2 > uvals.len());
        assert!(persons.len() as u64 * 100 >= (maxp + 1) * 99, "persons not dense");
    }
}
