//! The CSV dialect of rules.md, plus file listing and line-aligned chunking.

use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Splits one line (without its '\n') into exactly N fields and strips
/// syntactic quotes. None = malformed (wrong field count, stray quote, empty line).
#[inline(always)]
pub fn split<'a, const N: usize>(line: &'a [u8]) -> Option<[&'a [u8]; N]> {
    let mut f: [&[u8]; N] = [&[]; N];
    let mut i = 0usize;
    let mut start = 0usize;
    let mut quoted: u32 = 0; // bit per field that contains a quote
    for (pos, &c) in line.iter().enumerate() {
        if c == b',' {
            if i + 1 >= N {
                return None;
            }
            f[i] = unsafe { line.get_unchecked(start..pos) };
            i += 1;
            start = pos + 1;
        } else if c == b'"' {
            quoted |= 1 << i;
        }
    }
    if i + 1 != N {
        return None;
    }
    f[N - 1] = unsafe { line.get_unchecked(start..) };
    if quoted != 0 {
        for (j, fld) in f.iter_mut().enumerate() {
            if quoted & (1 << j) != 0 {
                *fld = unquote(fld)?;
            }
        }
    }
    Some(f)
}

#[inline]
fn unquote(f: &[u8]) -> Option<&[u8]> {
    let n = f.len();
    if n < 2 || f[0] != b'"' || f[n - 1] != b'"' {
        return None;
    }
    let inner = &f[1..n - 1];
    if memchr::memchr(b'"', inner).is_some() {
        return None;
    }
    Some(inner)
}

/// `*.csv` files directly in `dir`, sorted by name.
pub fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.ends_with(".csv") && !n.starts_with('.') && e.path().is_file()
            })
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

pub fn map(p: &Path) -> Option<Mmap> {
    let f = File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let m = unsafe { Mmap::map(&f).ok()? };
    Some(m)
}

/// A line-aligned byte range of a mapped file. `skip_header` is true for the
/// chunk that starts at offset 0.
#[derive(Clone, Copy)]
pub struct Chunk {
    pub file: usize,
    pub start: usize,
    pub end: usize,
}

/// Splits every file into chunks of about `target` bytes, each starting at a
/// line start (after the header line for the first chunk of a file).
pub fn chunks(maps: &[Option<Mmap>], target: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    for (fi, m) in maps.iter().enumerate() {
        let Some(m) = m else { continue };
        let b: &[u8] = m;
        // skip the header line
        let first = match memchr::memchr(b'\n', b) {
            Some(p) => p + 1,
            None => continue, // header only, no newline
        };
        let mut s = first;
        while s < b.len() {
            let mut e = (s + target).min(b.len());
            if e < b.len() {
                // move e to the next line start at or after e
                match memchr::memchr(b'\n', &b[e - 1..]) {
                    Some(p) => e = e - 1 + p + 1,
                    None => e = b.len(),
                }
            }
            out.push(Chunk { file: fi, start: s, end: e });
            s = e;
        }
    }
    // biggest-first isn't needed: chunks are uniform
    out
}

/// Calls `f` on each line (without '\n') in b[start..end]. A final line
/// without '\n' at end of file still counts as a line.
#[inline(always)]
pub fn for_lines(b: &[u8], mut f: impl FnMut(&[u8])) {
    let mut s = 0;
    for p in memchr::memchr_iter(b'\n', b) {
        f(unsafe { b.get_unchecked(s..p) });
        s = p + 1;
    }
    if s < b.len() {
        f(&b[s..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialect() {
        assert_eq!(split::<2>(b"a,b"), Some([&b"a"[..], b"b"]));
        assert_eq!(split::<2>(b"\"a\",\"\""), Some([&b"a"[..], b""]));
        assert_eq!(split::<2>(b"a,b\""), None);
        assert_eq!(split::<2>(b"a,b,c"), None);
        assert_eq!(split::<2>(b"a"), None);
        assert_eq!(split::<2>(b""), None);
        assert_eq!(split::<2>(b"\"a,b\""), None);
        assert_eq!(split::<2>(b"\",b"), None);
        assert_eq!(split::<2>(b"\"\"\",b"), None);
        assert_eq!(split::<2>(b" \"a\",b"), None);
        assert_eq!(split::<2>(b"a,b\r"), Some([&b"a"[..], b"b\r"]));
    }
}

/// Structural masks of 64 bytes at `p`: (separators ',' or '\n', newlines, quotes).
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    #[cfg(target_feature = "avx2")]
    {
        use std::arch::x86_64::*;
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let cm = _mm256_set1_epi8(b',' as i8);
        let nm = _mm256_set1_epi8(b'\n' as i8);
        let qm = _mm256_set1_epi8(b'"' as i8);
        let m = |x: __m256i| -> u64 {
            (_mm256_movemask_epi8(x) as u32) as u64
        };
        let nla = _mm256_cmpeq_epi8(a, nm);
        let nlb = _mm256_cmpeq_epi8(b, nm);
        let ca = _mm256_cmpeq_epi8(a, cm);
        let cb = _mm256_cmpeq_epi8(b, cm);
        let qa = _mm256_cmpeq_epi8(a, qm);
        let qb = _mm256_cmpeq_epi8(b, qm);
        let nl = m(nla) | (m(nlb) << 32);
        let sep = m(_mm256_or_si256(nla, ca)) | (m(_mm256_or_si256(nlb, cb)) << 32);
        let q = m(qa) | (m(qb) << 32);
        (sep, nl, q)
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        let (mut sep, mut nl, mut q) = (0u64, 0u64, 0u64);
        for i in 0..64 {
            let c = *p.add(i);
            nl |= ((c == b'\n') as u64) << i;
            sep |= ((c == b'\n' || c == b',') as u64) << i;
            q |= ((c == b'"') as u64) << i;
        }
        (sep, nl, q)
    }
}

/// Fast line scanner: for every well-formed line with exactly N fields in
/// `buf[start..end]` (line aligned), calls `f` with the unquoted fields.
/// Malformed lines are skipped. A final line without '\n' at `end` counts.
#[inline(always)]
pub fn scan<'a, const N: usize>(buf: &'a [u8], start: usize, end: usize, mut f: impl FnMut([&'a [u8]; N])) {
    let mut seps = [0usize; N];
    let mut nsep = 0usize;
    let mut ls = start;
    let mut qcount = 0u32;
    let mut pos = start;
    let mut tmp = [0u8; 64];
    let emit = |ls: usize, le: usize, seps: &[usize; N], nsep: usize, qcount: u32, f: &mut dyn FnMut([&'a [u8]; N])| {
        if nsep != N - 1 {
            return;
        }
        let mut fl: [&'a [u8]; N] = [&[]; N];
        let mut s = ls;
        for k in 0..N {
            let e = if k < N - 1 { seps[k] } else { le };
            fl[k] = unsafe { buf.get_unchecked(s..e) };
            s = e + 1;
        }
        if qcount != 0 {
            let mut wrapped = 0u32;
            for fld in fl.iter_mut() {
                let n = fld.len();
                if n >= 2 && fld[0] == b'"' && fld[n - 1] == b'"' {
                    *fld = &fld[1..n - 1];
                    wrapped += 2;
                }
            }
            if wrapped != qcount {
                return;
            }
        }
        f(fl);
    };
    let mut fdyn = |x: [&'a [u8]; N]| f(x);
    while pos < end {
        let rem = end - pos;
        let (sep, nl, mut q) = unsafe {
            if pos + 64 <= buf.len() {
                masks64(buf.as_ptr().add(pos))
            } else {
                let n = buf.len() - pos;
                tmp = [0u8; 64];
                tmp[..n].copy_from_slice(&buf[pos..]);
                masks64(tmp.as_ptr())
            }
        };
        let valid = if rem >= 64 { !0u64 } else { (1u64 << rem) - 1 };
        let mut s = sep & valid;
        q &= valid;
        while s != 0 {
            let i = s.trailing_zeros();
            let bit = 1u64 << i;
            let abs = pos + i as usize;
            if nl & bit != 0 {
                let low = q & (bit - 1);
                qcount += low.count_ones();
                q ^= low;
                emit(ls, abs, &seps, nsep, qcount, &mut fdyn);
                ls = abs + 1;
                nsep = 0;
                qcount = 0;
            } else {
                if nsep < N - 1 {
                    seps[nsep] = abs;
                }
                nsep += 1;
            }
            s &= s - 1;
        }
        qcount += q.count_ones();
        pos += 64;
    }
    if ls < end {
        emit(ls, end, &seps, nsep, qcount, &mut fdyn);
    }
}

/// `ids::pack` that may over-read up to 16 bytes past the field when the
/// mapping allows it (`buf` is the whole mapping the field lives in).
#[inline(always)]
pub fn pack_in(buf: &[u8], fld: &[u8]) -> u128 {
    let n = fld.len();
    let off = fld.as_ptr() as usize - buf.as_ptr() as usize;
    if n <= 15 && off + 16 <= buf.len() {
        if n == 0 {
            return 0;
        }
        let v = u128::from_be_bytes(unsafe { std::ptr::read_unaligned(fld.as_ptr() as *const [u8; 16]) });
        (v & !(u128::MAX >> (8 * n))) | n as u128
    } else {
        crate::ids::pack(fld)
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    fn collect<const N: usize>(b: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut v = Vec::new();
        scan::<N>(b, 0, b.len(), |f| v.push(f.iter().map(|x| x.to_vec()).collect()));
        v
    }
    fn slow<const N: usize>(b: &[u8]) -> Vec<Vec<Vec<u8>>> {
        let mut v = Vec::new();
        for_lines(b, |l| if let Some(f) = split::<N>(l) { v.push(f.iter().map(|x| x.to_vec()).collect()) });
        v
    }
    #[test]
    fn scan_matches_split() {
        let mut seed = 12345u64;
        let alphabet = b"ab,\"\n\r ";
        for len in 0..400 {
            for _ in 0..30 {
                let mut b = Vec::new();
                for _ in 0..len {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    b.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
                }
                assert_eq!(collect::<2>(&b), slow::<2>(&b), "{:?}", String::from_utf8_lossy(&b));
                assert_eq!(collect::<4>(&b), slow::<4>(&b));
                assert_eq!(collect::<7>(&b), slow::<7>(&b));
            }
        }
        for s in [&b"a,b"[..], b"\"a\",\"\"\n", b"x,y\n\n\"z\",w\nq"] {
            assert_eq!(collect::<2>(s), slow::<2>(s));
        }
    }
    #[test]
    fn pack_in_matches() {
        let b = b"abcdefghijklmnopqrstuvwxyz0123456789,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,,";
        for s in 0..40 {
            for n in 0..17 {
                if s + n <= b.len() {
                    assert_eq!(pack_in(b, &b[s..s + n]), crate::ids::pack(&b[s..s + n]));
                }
            }
        }
    }
}
