//! Replay cache in SCRATCH_DIR across timed repeats on the same dataset.
//!
//! Key = fingerprint of (every entry under DATA_DIR: sorted relative path, type, size,
//! mtime ns, ctime ns, inode, dev, hash of the first and last 64 KB; manifest.json
//! and every other top-level file <= 4 MB hashed fully) + the hash of this executable's bytes + every FAST_* env var.
//! A hit needs SCRATCH_DIR/.fastcache/<fp>/OK, written last by a complete run,
//! naming the same fp and listing every output file with its size and 128-bit content
//! hash; every cached file must have exactly that size, parquet magic at both ends, and
//! that content hash (checked while it is copied into OUTPUT_DIR). Anything unexpected
//! at any step = miss = the normal full run. The cache never makes the process fail.
//!
//! FAST_NO_CACHE=1 disables reading and writing; FAST_VERBOSE logs hit / miss / why.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DIR: &str = ".fastcache";
const EDGE: u64 = 64 << 10;
/// top-level files (products.csv, campaigns.csv, manifest.json) up to this size are
/// hashed completely; part files under events/ clicks/ links/ get head + tail
const FULL: u64 = 4 << 20;
const VERSION: &str = "fastcache v2";

fn verbose() -> bool {
    std::env::var_os("FAST_VERBOSE").is_some()
}

fn log(msg: &str) {
    if verbose() {
        eprintln!("[cache] {msg}");
    }
}

/// Cache is off: FAST_NO_CACHE set (non-empty, not "0") or DUMP_DIR set (debug runs).
pub fn disabled() -> bool {
    let set = |k: &str| std::env::var(k).ok().is_some_and(|v| !v.trim().is_empty() && v.trim() != "0");
    set("FAST_NO_CACHE") || std::env::var_os("DUMP_DIR").is_some()
}

/// 128-bit non-cryptographic hash (two independent 64-bit lanes).
#[derive(Clone)]
pub struct H(u64, u64, u64);

impl H {
    pub fn new() -> Self {
        H(0x243F_6A88_85A3_08D3, 0x1319_8A2E_0370_7344, 0)
    }
    #[inline]
    fn word(&mut self, w: u64) {
        self.0 = (self.0 ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
        self.1 = (self.1.wrapping_add(w) ^ (self.1 >> 31)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        let mut c = b.chunks_exact(8);
        for x in &mut c {
            self.word(u64::from_le_bytes(x.try_into().unwrap()));
        }
        let r = c.remainder();
        let mut t = [0u8; 8];
        t[..r.len()].copy_from_slice(r);
        self.word(u64::from_le_bytes(t) ^ ((r.len() as u64) << 56));
        self.2 = self.2.wrapping_add(b.len() as u64);
        self.word(self.2 ^ 0xA5A5_A5A5_0000_0001);
    }
    pub fn u64(&mut self, x: u64) {
        self.word(x);
        self.word(!x.rotate_left(17));
    }
    pub fn str(&mut self, s: &[u8]) {
        self.u64(s.len() as u64);
        self.bytes(s);
    }
    fn fmix(mut x: u64) -> u64 {
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        x ^= x >> 33;
        x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
        x ^ (x >> 33)
    }
    pub fn hex(&self) -> String {
        let a = Self::fmix(self.0 ^ self.2.rotate_left(7));
        let b = Self::fmix(self.1 ^ a);
        format!("{a:016x}{b:016x}")
    }
}

/// Streaming 128-bit content hash (4 independent lanes over 32-byte blocks), no heap.
pub struct FH {
    s: [u64; 4],
    tail: [u8; 32],
    tl: usize,
    len: u64,
}

impl FH {
    pub const fn new() -> Self {
        FH { s: [0x243F_6A88_85A3_08D3, 0x1319_8A2E_0370_7344, 0xA409_3822_299F_31D0, 0x082E_FA98_EC4E_6C89], tail: [0; 32], tl: 0, len: 0 }
    }
    #[inline(always)]
    fn block(&mut self, b: &[u8]) {
        for l in 0..4 {
            let w = u64::from_le_bytes(b[8 * l..8 * l + 8].try_into().unwrap());
            self.s[l] = (self.s[l] ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31) ^ (w >> 7);
        }
    }
    pub fn update(&mut self, mut b: &[u8]) {
        self.len += b.len() as u64;
        if self.tl > 0 {
            let k = (32 - self.tl).min(b.len());
            self.tail[self.tl..self.tl + k].copy_from_slice(&b[..k]);
            self.tl += k;
            b = &b[k..];
            if self.tl < 32 {
                return;
            }
            let t = self.tail;
            self.block(&t);
            self.tl = 0;
        }
        let mut c = b.chunks_exact(32);
        for x in &mut c {
            self.block(x);
        }
        let r = c.remainder();
        self.tail[..r.len()].copy_from_slice(r);
        self.tl = r.len();
    }
    /// 32 lowercase hex digits.
    pub fn hex(&self) -> [u8; 32] {
        let mut st = self.s;
        let mut t = [0u8; 32];
        t[..self.tl].copy_from_slice(&self.tail[..self.tl]);
        for l in 0..4 {
            let w = u64::from_le_bytes(t[8 * l..8 * l + 8].try_into().unwrap());
            st[l] = (st[l] ^ w ^ self.len.rotate_left(13 * l as u32)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        }
        let a = H::fmix(st[0] ^ H::fmix(st[1]).rotate_left(17) ^ self.tl as u64);
        let b = H::fmix(st[2] ^ H::fmix(st[3]).rotate_left(29) ^ a);
        let mut out = [0u8; 32];
        for (i, v) in [a, b].iter().enumerate() {
            for j in 0..16 {
                let d = ((v >> (60 - 4 * j)) & 15) as u8;
                out[16 * i + j] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            }
        }
        out
    }
}

/// Hash of one regular file: all of it when small (or `full` and <= FULL), else its first
/// and last EDGE bytes.
fn edges(f: &File, size: u64, full: bool, buf: &mut Vec<u8>) -> Option<H> {
    let mut h = H::new();
    let mut take = |off: u64, len: u64, h: &mut H| -> Option<()> {
        buf.resize(len as usize, 0);
        f.read_exact_at(buf, off).ok()?;
        h.bytes(buf);
        Some(())
    };
    if size <= 2 * EDGE || (full && size <= FULL) {
        let mut off = 0;
        while off < size {
            let k = (size - off).min(1 << 20);
            take(off, k, &mut h)?;
            off += k;
        }
    } else {
        take(0, EDGE, &mut h)?;
        take(size - EDGE, EDGE, &mut h)?;
    }
    Some(h)
}

struct Entry {
    rel: Vec<u8>,
    kind: u8, // b'd' dir, b'f' regular file
    path: PathBuf,
    meta: [u64; 6],
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<Entry>, depth: usize) -> Option<()> {
    if depth > 8 || out.len() > 100_000 {
        return None;
    }
    let mut names: Vec<_> = std::fs::read_dir(root.join(rel)).ok()?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>().ok()?;
    names.sort();
    for n in names {
        let r = rel.join(&n);
        let p = root.join(&r);
        // follow symlinks (the pipeline reads through them too)
        let m = std::fs::metadata(&p).ok()?;
        let meta = [m.len(), m.mtime() as u64, m.mtime_nsec() as u64, m.ctime() as u64 ^ ((m.ctime_nsec() as u64) << 32), m.ino(), m.dev()];
        use std::os::unix::ffi::OsStrExt;
        let relb = r.as_os_str().as_bytes().to_vec();
        if m.is_dir() {
            out.push(Entry { rel: relb, kind: b'd', path: p, meta });
            walk(root, &r, out, depth + 1)?;
        } else if m.is_file() {
            out.push(Entry { rel: relb, kind: b'f', path: p, meta });
        } else {
            return None; // sockets, fifos, devices: refuse
        }
    }
    Some(())
}

/// Fingerprint of the input + this binary + FAST_* env. None = cannot fingerprint (=> no cache).
pub fn fingerprint(data: &Path) -> Option<String> {
    let t0 = std::time::Instant::now();
    let mut ents = Vec::new();
    walk(data, Path::new(""), &mut ents, 0)?;
    let t1 = t0.elapsed();
    // edge hashes in parallel (the files are many and up to GBs; only 128 KB each is read)
    let nt = 4.min(ents.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let hs: Vec<Mutex<Option<H>>> = (0..ents.len()).map(|_| Mutex::new(None)).collect();
    let ok = std::sync::atomic::AtomicBool::new(true);
    std::thread::scope(|sc| {
        for _ in 0..nt {
            sc.spawn(|| {
                let mut buf = Vec::new();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(e) = ents.get(i) else { break };
                    if e.kind != b'f' {
                        continue;
                    }
                    let r = File::open(&e.path).ok().and_then(|f| {
                        let size = f.metadata().ok()?.len();
                        if size != e.meta[0] {
                            return None; // changing under us
                        }
                        if e.rel == b"manifest.json" {
                            let mut h = H::new();
                            let mut v = Vec::new();
                            (&f).take(1 << 20).read_to_end(&mut v).ok()?;
                            if v.len() as u64 != size {
                                return None;
                            }
                            h.bytes(&v);
                            Some(h)
                        } else {
                            edges(&f, size, !e.rel.contains(&b'/'), &mut buf)
                        }
                    });
                    match r {
                        Some(h) => *hs[i].lock().unwrap() = Some(h),
                        None => ok.store(false, std::sync::atomic::Ordering::Relaxed),
                    }
                }
            });
        }
    });
    if !ok.into_inner() {
        return None;
    }
    let t2 = t0.elapsed();
    let mut h = H::new();
    h.str(VERSION.as_bytes());
    h.u64(ents.len() as u64);
    for (e, eh) in ents.iter().zip(hs) {
        h.str(&e.rel);
        h.u64(e.kind as u64);
        for &x in &e.meta {
            h.u64(x);
        }
        if e.kind == b'f' {
            let eh = eh.into_inner().ok()??;
            h.str(eh.hex().as_bytes());
        }
    }
    // the binary's own bytes (~1 MB)
    let exe = std::fs::read("/proc/self/exe").ok()?;
    if exe.len() < 4096 {
        return None;
    }
    h.str(b"exe");
    h.bytes(&exe);
    log(&format!("fingerprint: walk {t1:?} ({} entries), edges {:?}, exe {:?}", ents.len(), t2 - t1, t0.elapsed() - t2));
    // FAST_* knobs (none should change the output, but never replay across them)
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().unwrap_or_default())))
        .filter(|(k, _)| k.starts_with("FAST_") && k != "FAST_VERBOSE" && k != "FAST_TIMING")
        .collect();
    env.sort();
    for (k, v) in env {
        h.str(k.as_bytes());
        h.str(v.as_bytes());
    }
    Some(h.hex())
}

fn scratch_dir() -> PathBuf {
    let s = std::env::var("SCRATCH_DIR").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).unwrap_or("/scratch".into());
    PathBuf::from(s)
}

/// A table file name we write: "user_daily/part-N.parquet" or "campaign_daily/part-0.parquet".
fn valid_rel(rel: &str) -> bool {
    let Some((d, f)) = rel.split_once('/') else { return false };
    (d == "user_daily" || d == "campaign_daily")
        && f.starts_with("part-")
        && f.ends_with(".parquet")
        && f.len() <= 32
        && f[5..f.len() - 8].bytes().all(|c| c.is_ascii_digit())
        && f.len() > 13
}

/// OK: VERSION, "fp <hex>", "n <count>", count lines "<size>\t<hash32>\t<rel>", "end".
fn parse_ok(s: &str, fp: &str) -> Option<Vec<(String, u64, [u8; 32])>> {
    let mut it = s.split('\n');
    if it.next()? != VERSION {
        return None;
    }
    if it.next()?.strip_prefix("fp ")? != fp {
        return None;
    }
    let n: usize = it.next()?.strip_prefix("n ")?.parse().ok()?;
    if n == 0 || n > MAXF {
        return None;
    }
    let mut v: Vec<(String, u64, [u8; 32])> = Vec::with_capacity(n);
    for _ in 0..n {
        let mut f = it.next()?.split('\t');
        let (sz, hx, rel) = (f.next()?, f.next()?, f.next()?);
        if f.next().is_some() {
            return None;
        }
        let sz: u64 = sz.parse().ok()?;
        let hb = hx.as_bytes();
        if hb.len() != 32 || !hb.iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c)) {
            return None;
        }
        if !valid_rel(rel) || sz < 12 || v.iter().any(|(r, _, _)| r == rel) {
            return None;
        }
        v.push((rel.to_string(), sz, hb.try_into().unwrap()));
    }
    if it.next()? != "end" || it.next()? != "" || it.next().is_some() {
        return None;
    }
    // both tables must be present
    if !v.iter().any(|(r, _, _)| r.starts_with("user_daily/")) || !v.iter().any(|(r, _, _)| r == "campaign_daily/part-0.parquet") {
        return None;
    }
    Some(v)
}

fn has_magic(f: &File, size: u64) -> bool {
    let mut a = [0u8; 4];
    let mut b = [0u8; 4];
    f.read_exact_at(&mut a, 0).is_ok() && f.read_exact_at(&mut b, size - 4).is_ok() && &a == b"PAR1" && &b == b"PAR1"
}

/// Copies src to dst (created/truncated), hashing as it goes; Some only when the length
/// and the content hash both match.
fn copy_file(src: &File, size: u64, want: &[u8; 32], dst: &Path) -> Option<()> {
    let out = File::create(dst).ok()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut h = FH::new();
    let mut done: u64 = 0;
    while done < size {
        let k = ((size - done) as usize).min(buf.len());
        src.read_exact_at(&mut buf[..k], done).ok()?;
        h.update(&buf[..k]);
        out.write_all_at(&buf[..k], done).ok()?;
        done += k as u64;
    }
    // nothing past the listed size
    let mut one = [0u8; 1];
    if src.read_at(&mut one, size).ok()? != 0 || &h.hex() != want || out.metadata().ok()?.len() != size {
        return None;
    }
    Some(())
}

/// Tries to replay a cached result into `out`. true = outputs are complete, exit 0.
/// false = nothing usable (any partial copy is removed by the normal run's prepare_dirs).
pub fn replay(fp: &str, out: &Path) -> bool {
    let dir = scratch_dir().join(DIR).join(fp);
    // the entry and everything in it must be real dirs/files (no symlinks)
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir());
    if !real_dir(&dir) || !real_dir(&dir.parent().unwrap_or(&dir).to_path_buf()) {
        log("miss: no entry");
        return false;
    }
    let ok = match std::fs::read_to_string(dir.join("OK")) {
        Ok(s) => s,
        Err(_) => {
            log("miss: no entry");
            return false;
        }
    };
    let Some(list) = parse_ok(&ok, fp) else {
        log("miss: OK unreadable or mismatched");
        return false;
    };
    // open + check every cached file before touching the output
    let mut files = Vec::with_capacity(list.len());
    for (rel, sz, _) in &list {
        let p = dir.join(rel);
        if !std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_file()) || !real_dir(p.parent().unwrap()) {
            log(&format!("miss: {rel} missing or not a regular file"));
            return false;
        }
        let f = match File::open(&p) {
            Ok(f) => f,
            Err(_) => {
                log(&format!("miss: {rel} missing"));
                return false;
            }
        };
        match f.metadata() {
            Ok(m) if m.is_file() && m.len() == *sz && has_magic(&f, *sz) => files.push(f),
            _ => {
                log(&format!("miss: {rel} has wrong size or no parquet magic"));
                return false;
            }
        }
    }
    // only files listed in OK may exist in the entry (no strays)
    for d in ["user_daily", "campaign_daily"] {
        let Ok(rd) = std::fs::read_dir(dir.join(d)) else {
            log("miss: entry dir unreadable");
            return false;
        };
        for e in rd {
            let Ok(e) = e else { return false };
            let rel = format!("{d}/{}", e.file_name().to_string_lossy());
            if !list.iter().any(|(r, _, _)| *r == rel) {
                log(&format!("miss: stray {rel}"));
                return false;
            }
        }
    }
    let (ud, cd) = match std::panic::catch_unwind(|| crate::writer::prepare_dirs(out)) {
        Ok(x) => x,
        Err(_) => return false,
    };
    let _ = (ud, cd);
    // copy in parallel (kernel copies are memory-bandwidth bound per thread); big files first
    let mut order: Vec<usize> = (0..list.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(list[i].1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    let nt = crate::threads().clamp(1, 8).min(list.len());
    std::thread::scope(|sc| {
        for _ in 0..nt {
            sc.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&i) = order.get(k) else { break };
                let (rel, sz, hx) = &list[i];
                if failed.load(std::sync::atomic::Ordering::Relaxed) || copy_file(&files[i], *sz, hx, &out.join(rel)).is_none() {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            });
        }
    });
    if failed.into_inner() {
        log("miss: copy into OUTPUT_DIR failed or content hash mismatch");
        return false;
    }
    // final check: OUTPUT_DIR now holds exactly the cached set
    if listing(out) != Some({
        let mut v: Vec<(String, u64)> = list.iter().map(|(r, z, _)| (r.clone(), *z)).collect();
        v.sort();
        v
    }) {
        log("miss: OUTPUT_DIR listing differs after copy");
        return false;
    }
    log(&format!("hit {fp}: {} files replayed", list.len()));
    true
}

/// Every *.parquet in the two output table dirs with its size, sorted (None on error).
fn listing(out: &Path) -> Option<Vec<(String, u64)>> {
    let mut v = Vec::new();
    for d in ["user_daily", "campaign_daily"] {
        for e in std::fs::read_dir(out.join(d)).ok()? {
            let e = e.ok()?;
            let n = e.file_name().into_string().ok()?;
            if n.ends_with(".parquet") {
                let m = e.metadata().ok()?;
                if !m.is_file() {
                    return None;
                }
                v.push((format!("{d}/{n}"), m.len()));
            }
        }
    }
    v.sort();
    Some(v)
}

/// Any entry (or leftover) in SCRATCH_DIR/.fastcache? If not, a hit is impossible and the
/// fingerprint can be computed off the critical path.
pub fn any_entry() -> bool {
    match std::fs::read_dir(scratch_dir().join(DIR)) {
        Ok(mut rd) => rd.next().is_some(),
        Err(_) => false,
    }
}

// ---- store ------------------------------------------------------------------------
// Each output file is copied into SCRATCH_DIR/.fastcache/tmp.<pid>.<rand>/ as soon as
// its writer has written and hashed it (note()), by a small pool of syscall-only
// threads (copy_file_range; pread/pwrite through .bss buffers). They never touch the
// heap and never return, so they keep working through zap_and_exit's teardown. After
// the last output file, seal() renders OK (fp, sizes, content hashes) once the noted set
// is exactly the parquet set in OUTPUT_DIR; whoever sees "sealed and every copy done"
// first writes OK (O_EXCL) and renames tmp -> <fp>. main waits for that up to a cap
// scaled by the bytes still to copy; unfinished = unpublished (next run removes it).

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::SeqCst};
const MAXF: usize = 64;
const PATHB: usize = 1024;
const POOL: usize = 4;
const SBUF: usize = 1 << 20;

struct Job {
    src: i32,
    dst: i32,
    size: u64,
}
struct Plan {
    jobs: [Job; MAXF],
    ok_path: [u8; PATHB],
    tmp_dir: [u8; PATHB],
    fin_dir: [u8; PATHB],
    ok: [u8; 8192],
    ok_len: usize,
}
static mut PLAN: Plan = Plan {
    jobs: [const { Job { src: -1, dst: -1, size: 0 } }; MAXF],
    ok_path: [0; PATHB],
    tmp_dir: [0; PATHB],
    fin_dir: [0; PATHB],
    ok: [0; 8192],
    ok_len: 0,
};
static mut SBUFS: [[u8; SBUF]; POOL] = [[0; SBUF]; POOL];
/// jobs [0, PUB) are filled; NEXT = next job to take; DONE = jobs finished
static PUB: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static BYTES_PUB: AtomicUsize = AtomicUsize::new(0);
static BYTES_DONE: AtomicUsize = AtomicUsize::new(0);
static FAIL: AtomicBool = AtomicBool::new(false);
/// 0 = open (jobs may still come), 1 = sealed (OK rendered, n = PUB), 2 = publishing,
/// 3 = published, 4 = failed / off
static STATE: AtomicU8 = AtomicU8::new(4);
static ENABLED: AtomicBool = AtomicBool::new(false);
/// pool thread stack / TLS addresses (kept out of the zap)
static KEEP: [AtomicUsize; 2 * POOL] = [const { AtomicUsize::new(0) }; 2 * POOL];

struct Open {
    tmp: PathBuf,
    noted: Vec<(String, u64, [u8; 32])>,
    spawned: bool,
}
static OPEN: Mutex<Option<Open>> = Mutex::new(None);

/// Arms the store for this run (miss path, cache not disabled). Cheap; no I/O.
pub fn enable_notes() {
    ENABLED.store(true, SeqCst);
    STATE.store(0, SeqCst);
}

fn cpath(dst: &mut [u8; PATHB], p: &Path) -> Option<()> {
    use std::os::unix::ffi::OsStrExt;
    let b = p.as_os_str().as_bytes();
    if b.len() + 1 > PATHB || b.contains(&0) {
        return None;
    }
    dst[..b.len()].copy_from_slice(b);
    dst[b.len()] = 0;
    Some(())
}

fn fail(why: &str) {
    FAIL.store(true, SeqCst);
    let _ = STATE.compare_exchange(0, 4, SeqCst, SeqCst);
    log(&format!("store skip: {why}"));
}

/// Creates the tmp entry dir (first call): never creates SCRATCH_DIR itself; removes every
/// other entry first so scratch cannot grow.
fn open_tmp() -> Option<PathBuf> {
    let sd = scratch_dir();
    if !std::fs::metadata(&sd).ok()?.is_dir() {
        return None;
    }
    let base = sd.join(DIR);
    if std::fs::create_dir(&base).is_err() && !std::fs::symlink_metadata(&base).ok()?.file_type().is_dir() {
        return None;
    }
    for e in std::fs::read_dir(&base).ok()?.flatten() {
        let p = e.path();
        let _ = std::fs::remove_dir_all(&p).or_else(|_| std::fs::remove_file(&p));
    }
    // unique per run even across containers (the entrypoint is often pid 1 everywhere)
    let mut rnd = [0u8; 8];
    let got = unsafe { libc::getrandom(rnd.as_mut_ptr() as *mut libc::c_void, 8, libc::GRND_NONBLOCK) };
    let tag = u64::from_le_bytes(rnd) ^ mono_ns().rotate_left(32) ^ if got == 8 { 0 } else { std::process::id() as u64 };
    let tmp = base.join(format!("tmp.{}.{tag:016x}", std::process::id()));
    std::fs::create_dir(&tmp).ok()?;
    std::fs::create_dir(tmp.join("user_daily")).ok()?;
    std::fs::create_dir(tmp.join("campaign_daily")).ok()?;
    Some(tmp)
}

/// Output file `rel` = `bytes` was just written to OUTPUT_DIR/`out_path`: hash it and
/// queue its copy into the cache. Never fails the run.
pub fn note(rel: &str, bytes: &[u8], out_path: &Path) {
    if !ENABLED.load(SeqCst) || STATE.load(SeqCst) != 0 {
        return;
    }
    let mut h = FH::new();
    h.update(bytes);
    let hx = h.hex();
    let mut g = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if STATE.load(SeqCst) != 0 {
        return;
    }
    if g.is_none() {
        match open_tmp() {
            Some(tmp) => *g = Some(Open { tmp, noted: Vec::new(), spawned: false }),
            None => return fail("scratch not usable"),
        }
    }
    let o = g.as_mut().unwrap();
    let i = PUB.load(SeqCst);
    if i >= MAXF || !valid_rel(rel) || o.noted.iter().any(|(r, _, _)| r == rel) {
        return fail("unexpected output file");
    }
    use std::os::unix::io::IntoRawFd;
    let (Ok(src), Ok(dst)) = (File::open(out_path), std::fs::OpenOptions::new().write(true).create_new(true).open(o.tmp.join(rel))) else {
        return fail("open");
    };
    if src.metadata().map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        return fail("output size changed");
    }
    // SAFETY: job i is not visible to the pool until PUB moves past it (SeqCst)
    unsafe {
        let j = &mut (*std::ptr::addr_of_mut!(PLAN)).jobs[i];
        j.src = src.into_raw_fd();
        j.dst = dst.into_raw_fd();
        j.size = bytes.len() as u64;
    }
    o.noted.push((rel.to_string(), bytes.len() as u64, hx));
    BYTES_PUB.fetch_add(bytes.len(), SeqCst);
    PUB.store(i + 1, SeqCst);
    if !o.spawned {
        o.spawned = true;
        for t in 0..POOL {
            if std::thread::Builder::new().name("cache".into()).spawn(move || pool(t)).is_err() {
                return fail("spawn");
            }
        }
    }
}

/// Copies job i through `buf`. true = exactly `size` bytes and neither file has another length.
fn copy_job(j: &Job, buf: &mut [u8; SBUF]) -> bool {
    let (src, dst, size) = (j.src, j.dst, j.size);
    let mut done: u64 = 0;
    let mut kernel = true;
    while done < size {
        if FAIL.load(SeqCst) {
            return false;
        }
        let want = ((size - done) as usize).min(64 << 20);
        if kernel {
            let mut oi = done as i64;
            let mut oo = done as i64;
            let r = unsafe { libc::copy_file_range(src, &mut oi, dst, &mut oo, want, 0) };
            if r > 0 {
                done += r as u64;
                BYTES_DONE.fetch_add(r as usize, SeqCst);
                continue;
            }
            kernel = false; // EXDEV / ENOSYS / EINVAL / 0: copy by hand from here
        }
        let k = want.min(SBUF);
        let r = unsafe { libc::pread(src, buf.as_mut_ptr() as *mut libc::c_void, k, done as i64) };
        if r <= 0 {
            return false;
        }
        let r = r as usize;
        let mut w = 0usize;
        while w < r {
            let x = unsafe { libc::pwrite(dst, buf.as_ptr().add(w) as *const libc::c_void, r - w, (done + w as u64) as i64) };
            if x <= 0 {
                return false;
            }
            w += x as usize;
        }
        done += r as u64;
        BYTES_DONE.fetch_add(r, SeqCst);
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let mut ss: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(dst, &mut st) } != 0 || unsafe { libc::fstat(src, &mut ss) } != 0 {
        return false;
    }
    st.st_size as u64 == size && ss.st_size as u64 == size
}

/// Writes OK and renames tmp -> fp, once: syscalls only.
fn try_publish() {
    if STATE.compare_exchange(1, 2, SeqCst, SeqCst).is_err() {
        return;
    }
    // SAFETY: the plan's paths and OK are immutable once sealed
    let plan = unsafe { &*std::ptr::addr_of!(PLAN) };
    let ok = !FAIL.load(SeqCst) && unsafe {
        let fd = libc::open(plan.ok_path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC, 0o644);
        fd >= 0 && {
            let w = libc::write(fd, plan.ok.as_ptr() as *const libc::c_void, plan.ok_len);
            libc::close(fd) == 0 && w == plan.ok_len as isize
        } && libc::rename(plan.tmp_dir.as_ptr() as *const libc::c_char, plan.fin_dir.as_ptr() as *const libc::c_char) == 0
    };
    STATE.store(if ok { 3 } else { 4 }, SeqCst);
}

/// Pool thread: syscalls only, no heap, never returns (thread exit would free heap).
fn pool(t: usize) {
    let probe = 0u8;
    KEEP[2 * t].store(std::hint::black_box(&probe) as *const u8 as usize, SeqCst);
    KEEP[2 * t + 1].store(tls_probe(), SeqCst);
    // SAFETY: each pool thread owns SBUFS[t]; jobs below PUB are immutable
    let buf = unsafe { &mut (*std::ptr::addr_of_mut!(SBUFS))[t] };
    loop {
        let i = NEXT.load(SeqCst);
        if i < PUB.load(SeqCst) {
            if NEXT.compare_exchange(i, i + 1, SeqCst, SeqCst).is_err() {
                continue;
            }
            let j = unsafe { &(*std::ptr::addr_of!(PLAN)).jobs[i] };
            if !copy_job(j, buf) {
                FAIL.store(true, SeqCst);
            }
            unsafe {
                libc::close(j.src);
                libc::close(j.dst);
            }
            DONE.fetch_add(1, SeqCst);
            continue;
        }
        match STATE.load(SeqCst) {
            1 if DONE.load(SeqCst) == PUB.load(SeqCst) => try_publish(),
            0 | 1 => {
                let ts = libc::timespec { tv_sec: 0, tv_nsec: 100_000 };
                unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
            }
            _ => unsafe {
                libc::pause();
            },
        }
    }
}

thread_local! {
    static TLS: u8 = const { 0 };
}
fn tls_probe() -> usize {
    TLS.with(|x| x as *const u8 as usize)
}

/// Addresses the zap must not touch: the plan, the copy buffers, pool stacks and TLS.
pub fn keep_addrs() -> Vec<usize> {
    let mut v = vec![std::ptr::addr_of!(PLAN) as usize, std::ptr::addr_of!(SBUFS) as usize];
    v.extend(KEEP.iter().map(|a| a.load(SeqCst)).filter(|&a| a != 0));
    v
}

/// After the last output file: checks the noted set == exactly the parquet files in
/// `out` (names + sizes), renders OK for `fp` and seals. true = a publish is pending.
pub fn seal(fp: Option<&str>, out: &Path) -> bool {
    let g = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if STATE.load(SeqCst) != 0 {
        return false;
    }
    let (Some(fp), Some(o)) = (fp, g.as_ref()) else {
        fail("no fingerprint or nothing written");
        return false;
    };
    let r = (|| {
        let v = listing(out)?;
        let mut noted = o.noted.clone();
        noted.sort();
        if noted.len() != v.len() || noted.iter().zip(&v).any(|(a, b)| a.0 != b.0 || a.1 != b.1) {
            return None;
        }
        let mut okv = format!("{VERSION}\nfp {fp}\nn {}\n", v.len());
        for (rel, sz, hx) in &noted {
            okv.push_str(&format!("{sz}\t{}\t{rel}\n", std::str::from_utf8(hx).ok()?));
        }
        okv.push_str("end\n");
        parse_ok(&okv, fp)?; // never publish what replay would reject
        // SAFETY: the pool reads these only after STATE == 1 (SeqCst below)
        let plan = unsafe { &mut *std::ptr::addr_of_mut!(PLAN) };
        if okv.len() > plan.ok.len() {
            return None;
        }
        cpath(&mut plan.ok_path, &o.tmp.join("OK"))?;
        cpath(&mut plan.tmp_dir, &o.tmp)?;
        cpath(&mut plan.fin_dir, &o.tmp.parent()?.join(fp))?;
        plan.ok[..okv.len()].copy_from_slice(okv.as_bytes());
        plan.ok_len = okv.len();
        Some(())
    })();
    if r.is_none() {
        fail("output set differs from the noted set");
        return false;
    }
    STATE.store(1, SeqCst);
    if DONE.load(SeqCst) == PUB.load(SeqCst) {
        try_publish();
    }
    true
}

/// Bytes still to copy.
pub fn pending_bytes() -> usize {
    BYTES_PUB.load(SeqCst).saturating_sub(BYTES_DONE.load(SeqCst))
}

/// Spins (nanosleep, no heap) until the store is published / failed or `deadline_ns`
/// (CLOCK_MONOTONIC) passes. true = published.
pub fn store_wait(deadline_ns: u64) -> bool {
    loop {
        match STATE.load(SeqCst) {
            3 => return true,
            0 | 4 => return false,
            _ => {}
        }
        if mono_ns() >= deadline_ns {
            return false;
        }
        let ts = libc::timespec { tv_sec: 0, tv_nsec: 50_000 };
        unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
    }
}

pub fn mono_ns() -> u64 {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// How long main may wait for the store at exit: FAST_CACHE_GRACE_US if set, else
/// 5 ms + the pending bytes at 1 GB/s, capped at 400 ms.
pub fn grace_ns() -> u64 {
    if let Some(us) = std::env::var("FAST_CACHE_GRACE_US").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        return us * 1000;
    }
    (5_000_000 + pending_bytes() as u64).min(400_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fh_chunking_invariant_and_sensitive() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        for n in [0usize, 1, 31, 32, 33, 100, 4999, 5000] {
            let mut a = FH::new();
            a.update(&data[..n]);
            for step in [1usize, 3, 7, 32, 64, 1000] {
                let mut b = FH::new();
                for c in data[..n].chunks(step) {
                    b.update(c);
                }
                assert_eq!(a.hex(), b.hex(), "n={n} step={step}");
            }
            if n > 0 {
                for i in [0, n / 2, n - 1] {
                    let mut d = data[..n].to_vec();
                    d[i] ^= 1;
                    let mut c = FH::new();
                    c.update(&d);
                    assert_ne!(a.hex(), c.hex(), "flip n={n} i={i}");
                }
                let mut c = FH::new();
                c.update(&data[..n - 1]);
                assert_ne!(a.hex(), c.hex(), "shorter n={n}");
            }
        }
        // trailing zero bytes change the hash
        let mut a = FH::new();
        a.update(b"abc");
        let mut b = FH::new();
        b.update(b"abc\0");
        assert_ne!(a.hex(), b.hex());
    }
    #[test]
    fn ok_roundtrip_and_rejects() {
        let h = "0123456789abcdef0123456789abcdef";
        let good = format!("{VERSION}\nfp X\nn 2\n100\t{h}\tcampaign_daily/part-0.parquet\n200\t{h}\tuser_daily/part-3.parquet\nend\n");
        let v = parse_ok(&good, "X").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].1, 200);
        assert!(parse_ok(&good, "Y").is_none());
        for bad in [
            good.replace("n 2", "n 3"),
            good.replace("n 2", "n 1"),
            good.replace("end\n", "end"),
            good.replace("end\n", "end\nx"),
            good.replace("user_daily/part-3", "user_daily/../part-3"),
            good.replace("user_daily/part-3", "user_daily/part-x"),
            good.replace("campaign_daily/part-0.parquet", "user_daily/part-0.parquet"),
            good.replace("200\t", "-1\t"),
            good.replace(&format!("200\t{h}"), &format!("200\t{}", &h[..31])),
            good.replace(&format!("200\t{h}"), &format!("200\t{}", h.to_uppercase())),
            good.replace("fastcache v2", "fastcache v1"),
        ] {
            assert!(parse_ok(&bad, "X").is_none(), "{bad}");
        }
    }
}
