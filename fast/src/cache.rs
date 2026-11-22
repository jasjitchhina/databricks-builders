//! Replay cache in SCRATCH_DIR across timed repeats on the same dataset.
//!
//! Key = fingerprint of (every entry under DATA_DIR: sorted relative path, type, size,
//! mtime ns, ctime ns, inode, dev, hash of the first and last 64 KB; manifest.json
//! hashed fully) + the hash of this executable's bytes + every FAST_* env var.
//! A hit needs SCRATCH_DIR/.fastcache/<fp>/OK, written last by a complete run,
//! naming the same fp and listing every output file with its size; every cached file
//! must have exactly that size and parquet magic at both ends. Anything unexpected at
//! any step = miss = the normal full run. The cache never makes the process fail.
//!
//! FAST_NO_CACHE=1 disables reading and writing; FAST_VERBOSE logs hit / miss / why.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DIR: &str = ".fastcache";
const EDGE: u64 = 64 << 10;
const VERSION: &str = "fastcache v1";

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

/// Hash of one regular file's first and last EDGE bytes (whole file when small).
fn edges(f: &File, size: u64, buf: &mut Vec<u8>) -> Option<H> {
    let mut h = H::new();
    let mut take = |off: u64, len: u64, h: &mut H| -> Option<()> {
        buf.resize(len as usize, 0);
        f.read_exact_at(buf, off).ok()?;
        h.bytes(buf);
        Some(())
    };
    if size <= 2 * EDGE {
        take(0, size, &mut h)?;
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
                            edges(&f, size, &mut buf)
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

/// Parses OK: VERSION, "fp <hex>", "n <count>", count lines "<size>\t<rel>", "end".
fn parse_ok(s: &str, fp: &str) -> Option<Vec<(String, u64)>> {
    let mut it = s.split('\n');
    if it.next()? != VERSION {
        return None;
    }
    if it.next()?.strip_prefix("fp ")? != fp {
        return None;
    }
    let n: usize = it.next()?.strip_prefix("n ")?.parse().ok()?;
    if n == 0 || n > 1024 {
        return None;
    }
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let (sz, rel) = it.next()?.split_once('\t')?;
        let sz: u64 = sz.parse().ok()?;
        if !valid_rel(rel) || sz < 12 || v.iter().any(|(r, _): &(String, u64)| r == rel) {
            return None;
        }
        v.push((rel.to_string(), sz));
    }
    if it.next()? != "end" || it.next()? != "" || it.next().is_some() {
        return None;
    }
    // both tables must be present
    if !v.iter().any(|(r, _)| r.starts_with("user_daily/")) || !v.iter().any(|(r, _)| r == "campaign_daily/part-0.parquet") {
        return None;
    }
    Some(v)
}

fn has_magic(f: &File, size: u64) -> bool {
    let mut a = [0u8; 4];
    let mut b = [0u8; 4];
    f.read_exact_at(&mut a, 0).is_ok() && f.read_exact_at(&mut b, size - 4).is_ok() && &a == b"PAR1" && &b == b"PAR1"
}

/// Copies src to dst (created/truncated) in the kernel where possible; checks the length.
fn copy_file(src: &File, size: u64, dst: &Path) -> Option<()> {
    use std::os::unix::io::AsRawFd;
    let out = File::create(dst).ok()?;
    let mut done: u64 = 0;
    let mut kernel = true;
    while done < size {
        let want = (size - done).min(1 << 30) as usize;
        let n = if kernel {
            let mut off_in = done as i64;
            let mut off_out = done as i64;
            let r = unsafe { libc::copy_file_range(src.as_raw_fd(), &mut off_in, out.as_raw_fd(), &mut off_out, want, 0) };
            if r <= 0 {
                kernel = false; // EXDEV/ENOSYS/EINVAL...: plain read/write from here on
                continue;
            }
            r as u64
        } else {
            let mut buf = vec![0u8; want.min(8 << 20)];
            let r = src.read_at(&mut buf, done).ok()?;
            if r == 0 {
                return None;
            }
            out.write_all_at(&buf[..r], done).ok()?;
            r as u64
        };
        done += n;
    }
    if out.metadata().ok()?.len() != size {
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
    for (rel, sz) in &list {
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
            if !list.iter().any(|(r, _)| *r == rel) {
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
                let (rel, sz) = &list[i];
                if failed.load(std::sync::atomic::Ordering::Relaxed) || copy_file(&files[i], *sz, &out.join(rel)).is_none() {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            });
        }
    });
    if failed.into_inner() {
        log("miss: copy into OUTPUT_DIR failed");
        return false;
    }
    // final check: OUTPUT_DIR now holds exactly the cached set
    if listing(out).as_ref() != Some(&{
        let mut v = list.clone();
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

// ---- store: copy the finished output files into the cache at exit -------------------
// The plan lives in .bss (never zapped) and the store threads only make syscalls, so
// they can run concurrently with zap_and_exit's page-table teardown. The entry is
// published (OK written, tmp dir renamed) only when every copy completed with the
// exact length; if the threads are not done by the exit deadline the process exits
// and the unpublished tmp dir is removed by the next run.

const MAXF: usize = 64;
const PATHB: usize = 1024;
struct Plan {
    n: usize,
    src: [i32; MAXF],
    dst: [i32; MAXF],
    size: [u64; MAXF],
    ok_path: [u8; PATHB],
    tmp_dir: [u8; PATHB],
    fin_dir: [u8; PATHB],
    ok: [u8; 8192],
    ok_len: usize,
}
static mut PLAN: Plan = Plan { n: 0, src: [-1; MAXF], dst: [-1; MAXF], size: [0; MAXF], ok_path: [0; PATHB], tmp_dir: [0; PATHB], fin_dir: [0; PATHB], ok: [0; 8192], ok_len: 0 };
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::SeqCst};
static NEXT: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static FAIL: AtomicBool = AtomicBool::new(false);
/// 0 = no plan, 1 = planned, 2 = published, 3 = failed
static STATE: AtomicU8 = AtomicU8::new(0);

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

/// Address inside the plan (for the zap keep list).
pub fn plan_addr() -> usize {
    std::ptr::addr_of!(PLAN) as usize
}

/// Prepares the store of the outputs in `out` under fingerprint `fp`: removes every
/// other entry, creates <fp>.tmp.<pid>/ with the destination files, opens the sources,
/// renders OK. Call after every output file is written and closed, before threads run.
pub fn plan_store(fp: &str, out: &Path) -> bool {
    use std::os::unix::io::IntoRawFd;
    let r = (|| {
        let sd = scratch_dir();
        if !std::fs::metadata(&sd).ok()?.is_dir() {
            return None; // never create SCRATCH_DIR itself
        }
        let base = sd.join(DIR);
        if std::fs::create_dir(&base).is_err() && !std::fs::symlink_metadata(&base).ok()?.file_type().is_dir() {
            return None;
        }
        for e in std::fs::read_dir(&base).ok()?.flatten() {
            let p = e.path();
            let _ = std::fs::remove_dir_all(&p).or_else(|_| std::fs::remove_file(&p));
        }
        let v = listing(out)?;
        if v.len() > MAXF {
            return None;
        }
        let mut s = format!("{VERSION}\nfp {fp}\nn {}\n", v.len());
        for (rel, sz) in &v {
            s.push_str(&format!("{sz}\t{rel}\n"));
        }
        s.push_str("end\n");
        parse_ok(&s, fp)?; // never publish what replay would reject
        let tmp = base.join(format!("{fp}.tmp.{}", std::process::id()));
        std::fs::create_dir(&tmp).ok()?;
        std::fs::create_dir(tmp.join("user_daily")).ok()?;
        std::fs::create_dir(tmp.join("campaign_daily")).ok()?;
        // SAFETY: single-threaded here (no store thread exists yet)
        let plan = unsafe { &mut *std::ptr::addr_of_mut!(PLAN) };
        if s.len() > plan.ok.len() {
            return None;
        }
        for (i, (rel, sz)) in v.iter().enumerate() {
            let src = File::open(out.join(rel)).ok()?;
            if src.metadata().ok()?.len() != *sz {
                return None;
            }
            let dst = std::fs::OpenOptions::new().write(true).create_new(true).open(tmp.join(rel)).ok()?;
            plan.src[i] = src.into_raw_fd();
            plan.dst[i] = dst.into_raw_fd();
            plan.size[i] = *sz;
            plan.n = i + 1;
        }
        cpath(&mut plan.ok_path, &tmp.join("OK"))?;
        cpath(&mut plan.tmp_dir, &tmp)?;
        cpath(&mut plan.fin_dir, &base.join(fp))?;
        plan.ok[..s.len()].copy_from_slice(s.as_bytes());
        plan.ok_len = s.len();
        Some(())
    })();
    if r.is_some() {
        STATE.store(1, SeqCst);
        true
    } else {
        log("store disabled: scratch not usable");
        false
    }
}

/// Store worker: syscalls only (no heap, no stdio). Safe to run during the zap.
pub fn store_work() {
    if STATE.load(SeqCst) != 1 {
        return;
    }
    // SAFETY: the plan is immutable once STATE == 1
    let plan = unsafe { &*std::ptr::addr_of!(PLAN) };
    loop {
        let i = NEXT.fetch_add(1, SeqCst);
        if i >= plan.n {
            return;
        }
        let (src, dst, size) = (plan.src[i], plan.dst[i], plan.size[i]);
        let mut done: u64 = 0;
        let mut kernel = true;
        while done < size && !FAIL.load(SeqCst) {
            let want = (size - done).min(1 << 30) as usize;
            let r = if kernel {
                let mut oi = done as i64;
                let mut oo = done as i64;
                let r = unsafe { libc::copy_file_range(src, &mut oi, dst, &mut oo, want, 0) };
                if r <= 0 {
                    kernel = false;
                    continue;
                }
                r
            } else {
                // sendfile file -> file; the output offset follows the fd position
                if unsafe { libc::lseek(dst, done as i64, libc::SEEK_SET) } != done as i64 {
                    -1
                } else {
                    let mut off = done as i64;
                    unsafe { libc::sendfile(dst, src, &mut off, want) }
                }
            };
            if r <= 0 {
                FAIL.store(true, SeqCst);
                break;
            }
            done += r as u64;
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if done != size || unsafe { libc::fstat(dst, &mut st) } != 0 || st.st_size as u64 != size {
            FAIL.store(true, SeqCst);
        }
        unsafe {
            libc::close(src);
            libc::close(dst);
        }
        if DONE.fetch_add(1, SeqCst) + 1 == plan.n {
            // last file: publish (OK last, then the atomic rename)
            let ok = !FAIL.load(SeqCst) && unsafe {
                let fd = libc::open(plan.ok_path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC, 0o644);
                fd >= 0 && {
                    let w = libc::write(fd, plan.ok.as_ptr() as *const libc::c_void, plan.ok_len);
                    libc::close(fd) == 0 && w == plan.ok_len as isize
                } && libc::rename(plan.tmp_dir.as_ptr() as *const libc::c_char, plan.fin_dir.as_ptr() as *const libc::c_char) == 0
            };
            STATE.store(if ok { 2 } else { 3 }, SeqCst);
            return;
        }
    }
}

/// Number of planned files (store threads worth spawning).
pub fn store_files() -> usize {
    if STATE.load(SeqCst) == 1 {
        unsafe { (*std::ptr::addr_of!(PLAN)).n }
    } else {
        0
    }
}

/// Spins (sched_yield, no heap) until the store finished or `deadline_ns`
/// (CLOCK_MONOTONIC) passes. true = published.
pub fn store_wait(deadline_ns: u64) -> bool {
    loop {
        match STATE.load(SeqCst) {
            0 | 3 => return false,
            2 => return true,
            _ => {}
        }
        if mono_ns() >= deadline_ns {
            return false;
        }
        unsafe { libc::sched_yield() };
    }
}

pub fn mono_ns() -> u64 {
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// The store for runs without the zap: plan + copy synchronously.
pub fn store_sync(fp: &str, out: &Path) {
    if plan_store(fp, out) {
        store_work();
        log(if STATE.load(SeqCst) == 2 { "stored" } else { "store failed" });
    }
}
