//! Timestamp forms F1/F2/F3 -> epoch ms. None = bad timestamp.

pub const DAY_MS: i64 = 86_400_000;

#[inline(always)]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let x = b[i].wrapping_sub(b'0');
    let y = b[i + 1].wrapping_sub(b'0');
    if x > 9 || y > 9 {
        None
    } else {
        Some((x as i64) * 10 + y as i64)
    }
}

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

#[inline(always)]
fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

const YMIN: i64 = 1900;
const YN: usize = 400;

/// DAYS[(y - YMIN) * 12 + m - 1] = days from epoch to y-m-01; MLEN = month length.
struct Tables {
    days: [i32; YN * 12],
    mlen: [u8; YN * 12],
}

static TABLES: std::sync::LazyLock<Tables> = std::sync::LazyLock::new(|| {
    let mut t = Tables { days: [0; YN * 12], mlen: [0; YN * 12] };
    for yi in 0..YN {
        let y = YMIN + yi as i64;
        for m in 1..=12i64 {
            t.days[yi * 12 + m as usize - 1] = days_from_civil(y, m, 1) as i32;
            t.mlen[yi * 12 + m as usize - 1] = match m {
                2 => if is_leap(y) { 29 } else { 28 },
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
        }
    }
    t
});

const D8: u64 = 0x3030_3030_3030_3030;

/// True iff every byte of `x` selected by `m` (0xFF lanes) is an ASCII digit.
#[inline(always)]
fn digits_ok(x: u64, m: u64) -> bool {
    let hi = 0xF0F0_F0F0_F0F0_F0F0 & m;
    let base = D8 & m;
    (x & hi) == base && ((x & m).wrapping_add(0x0606_0606_0606_0606 & m) & hi) == base
}

/// Parses `YYYY-MM-DDTHH:MM:SS` (first 19 bytes of b, b.len() >= 20) to epoch ms, validating the calendar.
#[inline(always)]
fn civil19(b: &[u8]) -> Option<i64> {
    #[cfg(target_feature = "avx2")]
    {
        return unsafe { civil19_simd(b.as_ptr()) };
    }
    #[allow(unreachable_code)]
    civil19_swar(b)
}

/// SSE version of `civil19_swar` (reads bytes 0..20 of p).
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn civil19_simd(p: *const u8) -> Option<i64> {
    use std::arch::x86_64::*;
    let lo = _mm_loadu_si128(p as *const __m128i); // bytes 0..16
    let hi = _mm_loadu_si128(p.add(4) as *const __m128i); // bytes 4..20
    // separators: '-' at 4, 7; 'T' at 10; ':' at 13 (lo) and 16 (hi offset 12)
    let sep_lo = _mm_shuffle_epi8(lo, _mm_setr_epi8(4, 7, 10, 13, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1));
    let sep_hi = _mm_shuffle_epi8(hi, _mm_setr_epi8(-1, -1, -1, -1, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1));
    let sep = _mm_or_si128(sep_lo, sep_hi);
    let want = _mm_setr_epi8(b'-' as i8, b'-' as i8, b'T' as i8, b':' as i8, b':' as i8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    if _mm_movemask_epi8(_mm_cmpeq_epi8(sep, want)) != 0xFFFF {
        return None;
    }
    // digits Y0..Y3 M0 M1 D0 D1 H0 H1 (lo: 0,1,2,3,5,6,8,9,11,12) m0 m1 S0 S1 (hi: 10,11,13,14)
    let dl = _mm_shuffle_epi8(lo, _mm_setr_epi8(0, 1, 2, 3, 5, 6, 8, 9, 11, 12, -1, -1, -1, -1, -1, -1));
    let dh = _mm_shuffle_epi8(hi, _mm_setr_epi8(-1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 10, 11, 13, 14, -1, -1));
    let dig = _mm_or_si128(dl, dh);
    // lanes 14, 15 are 0; make them '0' so they pass the digit check
    let dig = _mm_or_si128(dig, _mm_setr_epi8(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'0' as i8, b'0' as i8));
    let d = _mm_sub_epi8(dig, _mm_set1_epi8(b'0' as i8));
    if _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d)) != 0xFFFF {
        return None;
    }
    // pairs -> u16: [Y0Y1, Y2Y3, MM, DD, hh, mm, ss, 0]
    let v = _mm_maddubs_epi16(d, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1));
    let y = _mm_extract_epi16(v, 0) as i64 * 100 + _mm_extract_epi16(v, 1) as i64;
    let mo = _mm_extract_epi16(v, 2) as i64;
    let dd = _mm_extract_epi16(v, 3) as i64;
    let h = _mm_extract_epi16(v, 4) as i64;
    let mi = _mm_extract_epi16(v, 5) as i64;
    let s = _mm_extract_epi16(v, 6) as i64;
    finish_civil(y, mo, dd, h, mi, s)
}

#[inline(always)]
fn finish_civil(y: i64, mo: i64, dd: i64, h: i64, mi: i64, s: i64) -> Option<i64> {
    if !(1..=12).contains(&mo) || dd < 1 || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = if (YMIN..YMIN + YN as i64).contains(&y) {
        let t = &*TABLES;
        let i = (y - YMIN) as usize * 12 + mo as usize - 1;
        if dd > t.mlen[i] as i64 {
            return None;
        }
        t.days[i] as i64 + dd - 1
    } else {
        let mdays = match mo {
            2 => if is_leap(y) { 29 } else { 28 },
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        if dd > mdays {
            return None;
        }
        days_from_civil(y, mo, dd)
    };
    Some(((days * 24 + h) * 60 + mi) * 60_000 + s * 1000)
}

#[inline(always)]
#[allow(dead_code)]
fn civil19_swar(b: &[u8]) -> Option<i64> {
    let a = u64::from_le_bytes(b[0..8].try_into().unwrap()); // YYYY-MM-
    let c = u64::from_le_bytes(b[8..16].try_into().unwrap()); // DDTHH:MM
    let e = u32::from_le_bytes(b[16..20].try_into().unwrap()) as u64 & 0x00FF_FFFF; // :SS
    // separators
    const AM: u64 = 0xFF00_00FF_0000_0000; // bytes 4 and 7
    const AS: u64 = 0x2D00_002D_0000_0000;
    const CM: u64 = 0x0000_FF00_00FF_0000; // bytes 2 (T) and 5 (:)
    const CS: u64 = 0x0000_3A00_0054_0000;
    if (a & AM) != AS || (c & CM) != CS || (e & 0xFF) != 0x3A {
        return None;
    }
    if !digits_ok(a, !AM) || !digits_ok(c, !CM) || !digits_ok(e, 0x00FF_FF00) {
        return None;
    }
    let d = |x: u64, i: u32| ((x >> (8 * i)) & 0xF) as i64;
    let y = d(a, 0) * 1000 + d(a, 1) * 100 + d(a, 2) * 10 + d(a, 3);
    let mo = d(a, 5) * 10 + d(a, 6);
    let dd = d(c, 0) * 10 + d(c, 1);
    let h = d(c, 3) * 10 + d(c, 4);
    let mi = d(c, 6) * 10 + d(c, 7);
    let s = d(e, 1) * 10 + d(e, 2);
    if !(1..=12).contains(&mo) || dd < 1 || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = if (YMIN..YMIN + YN as i64).contains(&y) {
        let t = &*TABLES;
        let i = (y - YMIN) as usize * 12 + mo as usize - 1;
        if dd > t.mlen[i] as i64 {
            return None;
        }
        t.days[i] as i64 + dd - 1
    } else {
        let mdays = match mo {
            2 => if is_leap(y) { 29 } else { 28 },
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        if dd > mdays {
            return None;
        }
        days_from_civil(y, mo, dd)
    };
    Some(((days * 24 + h) * 60 + mi) * 60_000 + s * 1000)
}

/// F1, F2 or F3. Values too large for i64 are returned as i64::MAX (always
/// outside any window), which is equivalent to "bad".
#[inline]
pub fn parse_ts(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => return civil19(b),
        25 if (b[19] == b'+' || b[19] == b'-') && b[22] == b':' => {
            let t = civil19(b)?;
            let oh = d2(b, 20)?;
            let om = d2(b, 23)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let off = (oh * 60 + om) * 60_000;
            return Some(if b[19] == b'+' { t - off } else { t + off });
        }
        _ => {}
    }
    parse_f3(b)
}

/// `parse_ts` for a field inside mapping `m`: the F3 form uses a SIMD digit
/// parser that reads the 16 bytes ending at the field's end when they lie in `m`.
#[inline(always)]
pub fn parse_ts_in(m: &[u8], b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => return civil19(b),
        25 if (b[19] == b'+' || b[19] == b'-') && b[22] == b':' => return parse_ts(b),
        _ => {}
    }
    #[cfg(target_feature = "avx2")]
    {
        let n = b.len();
        let end = b.as_ptr() as usize + n;
        if n >= 1 && n <= 16 && end - 16 >= m.as_ptr() as usize && end <= m.as_ptr() as usize + m.len() {
            return unsafe { f3_simd(b.as_ptr().add(n), n) };
        }
    }
    let _ = m;
    parse_f3(b)
}

/// Digits-only value of the n (1..=16) bytes ending at `end` (the 16 bytes
/// before `end` must be readable).
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn f3_simd(end: *const u8, n: usize) -> Option<i64> {
    use std::arch::x86_64::*;
    let x = _mm_loadu_si128(end.sub(16) as *const __m128i);
    let idx = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let pre = _mm_cmpgt_epi8(_mm_set1_epi8((16 - n) as i8), idx);
    let x = _mm_blendv_epi8(x, _mm_set1_epi8(b'0' as i8), pre);
    let d = _mm_sub_epi8(x, _mm_set1_epi8(b'0' as i8));
    if _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d)) != 0xFFFF {
        return None;
    }
    let p = _mm_maddubs_epi16(d, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1));
    let q = _mm_madd_epi16(p, _mm_setr_epi16(100, 1, 100, 1, 100, 1, 100, 1));
    let r = _mm_packus_epi32(q, q);
    let t = _mm_madd_epi16(r, _mm_setr_epi16(10000, 1, 10000, 1, 10000, 1, 10000, 1));
    let hi = _mm_cvtsi128_si32(t) as u32 as u64;
    let lo = _mm_extract_epi32(t, 1) as u32 as u64;
    Some((hi * 100_000_000 + lo) as i64)
}

#[inline]
fn parse_f3(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    if b.len() <= 18 {
        let mut v: u64 = 0;
        let mut bad = false;
        for &c in b {
            let d = c.wrapping_sub(b'0');
            bad |= d > 9;
            v = v * 10 + d as u64;
        }
        return if bad { None } else { Some(v as i64) };
    }
    let mut v: u64 = 0;
    let mut overflow = false;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        if !overflow {
            match v.checked_mul(10).and_then(|x| x.checked_add(d as u64)) {
                Some(x) => v = x,
                None => overflow = true,
            }
        }
    }
    if overflow || v > i64::MAX as u64 {
        Some(i64::MAX)
    } else {
        Some(v as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forms() {
        let f1 = parse_ts(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(f1, 1791071890000);
        assert_eq!(parse_ts(b"1791071890000"), Some(f1));
        assert_eq!(parse_ts(b"0001791071890000"), Some(f1));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10-00:00"), Some(f1));
        for bad in [
            &b""[..], b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z", b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10", b"2026-10-03T23:58:10.000Z", b"1791071890000.0", b"1.79e12",
            b"-5", b"2026-13-03T23:58:10Z", b"2026-02-30T23:58:10Z", b"2027-02-29T23:58:10Z",
            b"2026-10-03T24:00:00Z", b"2026-10-03T23:58:60Z", b"2026-10-03T23:58:10+24:00",
            b"2026-10-03T23:58:10+05:60", b"2026-10-03T23:58:10+0200", "２５０".as_bytes(),
        ] {
            assert_eq!(parse_ts(bad), None, "{:?}", std::str::from_utf8(bad));
        }
        assert!(parse_ts(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse_ts(b"86400000"), Some(86400000));
        assert_eq!(parse_ts(b"999999999999999999999"), Some(i64::MAX));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10+23:59").unwrap(), f1 - (23 * 60 + 59) * 60000);
        // exhaustive-ish cross check vs the slow formula
        for y in [1899i64, 1900, 1970, 2000, 2024, 2025, 2026, 2027, 2028, 2100, 2299, 2300, 9999, 0] {
            for mo in 0..=13i64 {
                for d in [0i64, 1, 28, 29, 30, 31, 32] {
                    let txt = format!("{y:04}-{mo:02}-{d:02}T12:34:56Z");
                    let ml = match mo { 2 => if is_leap(y) { 29 } else { 28 }, 4 | 6 | 9 | 11 => 30, _ => 31 };
                    let want = if (1..=12).contains(&mo) && d >= 1 && d <= ml { Some(((days_from_civil(y, mo, d) * 24 + 12) * 60 + 34) * 60000 + 56000) } else { None };
                    assert_eq!(parse_ts(txt.as_bytes()), want, "{txt}");
                }
            }
        }
        for bad in ["2026-10-03T2a:58:10Z", "2026-1/-03T23:58:10Z", "2026-10-03T23:58:1:Z", "2026/10-03T23:58:10Z", "2026-10-03T23-58:10Z", "2026-10-03T23:58-10Z", "20\x0026-10-03T23:58:10Z", "2026-10-03T23:5\u{7f}:10Z"] {
            assert_eq!(parse_ts(bad.as_bytes()), None, "{bad}");
        }
    }
}

#[cfg(test)]
mod fuzz {
    use super::*;
    fn d2(b: &[u8], i: usize) -> Option<i64> {
        let (x, y) = (b[i].wrapping_sub(b'0'), b[i + 1].wrapping_sub(b'0'));
        if x > 9 || y > 9 { None } else { Some(x as i64 * 10 + y as i64) }
    }
    fn civil_ref(b: &[u8]) -> Option<i64> {
        if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' { return None; }
        let y = d2(b, 0)? * 100 + d2(b, 2)?;
        let (mo, d, h, mi, s) = (d2(b, 5)?, d2(b, 8)?, d2(b, 11)?, d2(b, 14)?, d2(b, 17)?);
        if !(1..=12).contains(&mo) || d < 1 || h > 23 || mi > 59 || s > 59 { return None; }
        let ml = match mo { 2 => if is_leap(y) { 29 } else { 28 }, 4 | 6 | 9 | 11 => 30, _ => 31 };
        if d > ml { return None; }
        Some(((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + s * 1000)
    }
    fn ref_ts(b: &[u8]) -> Option<i64> {
        if b.len() == 20 && b[19] == b'Z' { return civil_ref(b); }
        if b.len() == 25 && (b[19] == b'+' || b[19] == b'-') && b[22] == b':' {
            let t = civil_ref(b)?;
            let (oh, om) = (d2(b, 20)?, d2(b, 23)?);
            if oh > 23 || om > 59 { return None; }
            let off = (oh * 60 + om) * 60_000;
            return Some(if b[19] == b'+' { t - off } else { t + off });
        }
        if b.is_empty() || !b.iter().all(|c| c.is_ascii_digit()) { return None; }
        let v: u128 = b.iter().fold(0u128, |a, &c| (a.saturating_mul(10)).saturating_add((c - b'0') as u128));
        Some(if v > i64::MAX as u128 { i64::MAX } else { v as i64 })
    }
    #[test]
    fn differential() {
        let mut seed = 99u64;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
        let templates: [&[u8]; 8] = [b"2028-02-29T23:59:59Z", b"2027-12-31T00:00:00+05:30", b"1999-09-09T09:09:09-00:00", b"0001791071890000", b"1791071890000", b"9999999999999999", b"7", b"00000000000000000001791071890000"];
        let alpha = b"0123456789-+:TZtz /\x7f\xff";
        for _ in 0..2_000_000 {
            let mut v = templates[rnd() as usize % 8].to_vec();
            for _ in 0..(rnd() % 3) {
                let i = rnd() as usize % v.len();
                v[i] = if rnd() % 2 == 0 { alpha[rnd() as usize % alpha.len()] } else { b'0' + (rnd() % 10) as u8 };
            }
            assert_eq!(parse_ts(&v), ref_ts(&v), "{:?}", String::from_utf8_lossy(&v));
            let mut padded = vec![b'7'; 20];
            padded.extend_from_slice(&v);
            padded.extend_from_slice(b",x");
            assert_eq!(parse_ts_in(&padded, &padded[20..20 + v.len()]), ref_ts(&v), "in {:?}", String::from_utf8_lossy(&v));
        }
    }
}
