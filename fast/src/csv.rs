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
