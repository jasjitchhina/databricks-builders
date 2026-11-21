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

/// Parses `YYYY-MM-DDTHH:MM:SS` (first 19 bytes) to epoch ms, validating the calendar.
#[inline(always)]
fn civil19(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let h = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let s = d2(b, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let mdays = match mo {
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if d > mdays {
        return None;
    }
    Some(((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60_000 + s * 1000)
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

#[inline]
fn parse_f3(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
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
    }
}
