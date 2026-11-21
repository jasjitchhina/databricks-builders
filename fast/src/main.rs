use std::path::PathBuf;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static M: fast::alloc::DonorAlloc<mimalloc::MiMalloc> = fast::alloc::DonorAlloc(mimalloc::MiMalloc);

#[cfg(all(feature = "hugealloc", not(feature = "mimalloc")))]
#[global_allocator]
static A: fast::alloc::HugeAlloc = fast::alloc::HugeAlloc;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(args.get(1).cloned().or_else(|| std::env::var("DATA_DIR").ok()).unwrap_or("/data".into()));
    let out = PathBuf::from(args.get(2).cloned().or_else(|| std::env::var("OUTPUT_DIR").ok()).unwrap_or("/output".into()));
    let nthreads: usize = std::env::var("NCPU").ok().and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8));
    fast::set_threads(nthreads);
    let dump = std::env::var("DUMP_DIR").ok().map(PathBuf::from);
    let verbose = std::env::var("FAST_TIMING").is_ok();
    let t0 = Instant::now();
    if std::env::var_os("FAST_NO_PREFETCH").is_none() {
        prefetch_inputs(&data);
    }
    let c = if std::env::var("FAST_TEMP_LOADER").is_ok() { fast::loader::load(&data) } else { fast::clean::clean(&data, nthreads) };
    let t1 = Instant::now();
    let (ud, cd) = fast::writer::prepare_dirs(&out);
    let o = fast::engine::run(&c, dump.is_some(), Some(&ud));
    let t2 = Instant::now();
    fast::writer::write_campaigns(&cd, &c, &o);
    let t3 = Instant::now();
    if let (Some(d), Some(sd)) = (dump, o.session_dump.as_ref()) {
        std::fs::create_dir_all(&d).unwrap();
        let mut lines: Vec<&[u8]> = sd.iter().flat_map(|b| b.split(|&x| x == b'\n').filter(|l| !l.is_empty())).collect();
        lines.sort();
        let mut buf = b"person,start_ms,n_events,revenue,top_category,credited_campaign,assist_campaigns\n".to_vec();
        for l in lines {
            buf.extend_from_slice(l);
            buf.push(b'\n');
        }
        std::fs::write(d.join("sessions.csv"), buf).unwrap();
    }
    if verbose {
        eprintln!("main total {:?}", t0.elapsed());
        if std::env::var("FAST_SMAPS").is_ok() {
            let _ = std::fs::copy("/proc/self/smaps", std::env::var("FAST_SMAPS").unwrap());
        }
        if std::env::var("FAST_MEM").is_ok() { if let Ok(st) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            let v: Vec<&str> = st.lines().filter(|l| l.starts_with("Rss") || l.starts_with("AnonHuge")).collect();
            eprintln!("{}", v.join(" | ").split_whitespace().collect::<Vec<_>>().join(" "));
        }}
        if let Ok(st) = std::fs::read_to_string("/proc/self/status") {
            let v: Vec<&str> = st.lines().filter(|l| l.starts_with("VmRSS") || l.starts_with("RssAnon") || l.starts_with("RssFile") || l.starts_with("VmHWM") || l.starts_with("VmPTE")).collect();
            eprintln!("{}", v.join(" | ").split_whitespace().collect::<Vec<_>>().join(" "));
        }
        eprintln!(
            "load {:?} engine {:?} write {:?} | events {} clicks {} persons {}",
            t1 - t0, t2 - t1, t3 - t2, c.parts.iter().map(|p| p.ev.len()).sum::<usize>(), c.parts.iter().map(|p| p.cl.len()).sum::<usize>(), c.parts.iter().map(|p| p.person_names.len()).sum::<usize>()
        );
        if std::env::var("FAST_MEASURE_DROP").is_ok() {
            let t = Instant::now();
            drop(o);
            drop(c);
            eprintln!("drop {:?}", t.elapsed());
            return;
        }
    }
    if verbose {
        let ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        eprintln!("end_epoch_ns {ns}");
    }
    // Outputs are written and closed. Skip destructors (freeing millions of small
    // allocations is pure waste) and pre-clear page tables in parallel so the
    // single-threaded exit teardown has little left to do.
    if std::env::var_os("FAST_NO_ZAP").is_none() {
        cov_flush();
        zap_and_exit(&data, nthreads, std::env::var_os("FAST_NO_ZAP_ANON").is_none(), verbose);
    }
    std::process::exit(0);
}

/// Cold page cache insurance: ask the kernel to start reading every input file now
/// (POSIX_FADV_WILLNEED, async readahead) from a background thread, in the order the
/// pipeline consumes them (links, clicks, events). Nearly free when the cache is warm.
static PREFETCH: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> = std::sync::Mutex::new(Vec::new());

fn prefetch_inputs(data: &std::path::Path) {
    let mut files = vec![];
    for sub in ["links", "clicks", "events"] {
        let mut v: Vec<_> = std::fs::read_dir(data.join(sub))
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        v.sort();
        files.extend(v);
    }
    // several threads keep the device queue deep; each opens, advises (8MB ranges) and
    // closes one file at a time so at most `nt` descriptors are ever held (EMFILE-safe)
    let nt: usize = std::env::var("FAST_PF_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let files = std::sync::Arc::new(files);
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut hs = PREFETCH.lock().unwrap();
    for _ in 0..nt {
        let (files, next) = (files.clone(), next.clone());
        hs.push(std::thread::spawn(move || {
            use std::os::unix::io::AsRawFd;
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(p) = files.get(i) else { break };
                let Ok(f) = std::fs::File::open(p) else { continue };
                let len = f.metadata().map(|m| m.len() as i64).unwrap_or(0);
                let mut o = 0;
                while o < len {
                    let l = (8i64 << 20).min(len - o);
                    unsafe { libc::posix_fadvise(f.as_raw_fd(), o, l, libc::POSIX_FADV_WILLNEED) };
                    o += l;
                }
            }
        }));
    }
}

const MAX_RANGES: usize = 1 << 14;
/// Work list for the zap threads. Lives in .bss (a small mapping that is never
/// zapped) because heap memory may be zeroed under their feet.
static mut ZAP: [(usize, usize); MAX_RANGES] = [(0, 0); MAX_RANGES];
static ZAP_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static ZAP_NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static ZAP_GO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ZAP_DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static ZAP_READY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// stack and TLS addresses of every thread alive during the zap (never zapped)
static ZAP_KEEP: [std::sync::atomic::AtomicUsize; 64] = [const { std::sync::atomic::AtomicUsize::new(0) }; 64];
thread_local! {
    static TLS_PROBE: u8 = const { 0 };
}
fn tls_addr() -> usize {
    TLS_PROBE.with(|x| x as *const u8 as usize)
}

/// Called once, after every output file is written and closed; never returns.
/// MADV_DONTNEED (runs under the mmap read lock, so it scales across threads,
/// unlike munmap) on (1) the read-only CSV file mappings under DATA_DIR and
/// (2) if `anon`, every large private anonymous mapping (THP=never hedge: exit
/// would otherwise free GBs of 4K pages on one thread). After the zap no code may
/// touch the heap: zap threads only make syscalls, then sleep; main spins on
/// atomics and calls _exit.
fn zap_and_exit(data: &std::path::Path, nthreads: usize, anon: bool, verbose: bool) -> ! {
    use std::sync::atomic::Ordering::SeqCst;
    // no other thread may run (and allocate) during the zap
    for h in PREFETCH.lock().unwrap().drain(..) {
        let _ = h.join();
    }
    let root = std::fs::canonicalize(data).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let nt = nthreads.clamp(1, 16);
    // spawn the zap threads first (spawning allocates); each publishes its stack and
    // TLS addresses, then waits for ZAP_GO
    for t in 0..nt {
        std::thread::spawn(move || {
            let probe = 0u8;
            ZAP_KEEP[2 * t].store(std::hint::black_box(&probe) as *const u8 as usize, SeqCst);
            ZAP_KEEP[2 * t + 1].store(tls_addr(), SeqCst);
            ZAP_READY.fetch_add(1, SeqCst);
            while !ZAP_GO.load(SeqCst) {
                unsafe { libc::sched_yield() };
            }
            loop {
                let i = ZAP_NEXT.fetch_add(1, SeqCst);
                if i >= ZAP_N.load(SeqCst) {
                    break;
                }
                let (a, l) = unsafe { (*std::ptr::addr_of!(ZAP))[i] };
                unsafe { libc::madvise(a as *mut libc::c_void, l, libc::MADV_DONTNEED) };
            }
            ZAP_DONE.fetch_add(1, SeqCst);
            loop {
                unsafe { libc::pause() };
            }
        });
    }
    while ZAP_READY.load(SeqCst) < nt {
        unsafe { libc::sched_yield() };
    }
    let probe = 0u8;
    let mut keep: Vec<usize> = ZAP_KEEP[..2 * nt].iter().map(|a| a.load(SeqCst)).collect();
    keep.push(std::hint::black_box(&probe) as *const u8 as usize); // main stack
    keep.push(tls_addr()); // main TLS
    keep.push(std::ptr::addr_of!(ZAP) as usize);
    keep.push(&ZAP_DONE as *const _ as usize);
    keep.push(&ZAP_KEEP as *const _ as usize);
    let (mut n, mut file_mb, mut anon_mb) = (0usize, 0usize, 0usize);
    for line in maps.lines() {
        // addr perms offset dev inode [path]
        let mut it = line.split_whitespace();
        let (Some(addr), Some(perms)) = (it.next(), it.next()) else { continue };
        let path = it.nth(3).unwrap_or("");
        let Some((a, b)) = addr.split_once('-') else { continue };
        let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else { continue };
        let is_file = perms.starts_with("r--") && !root.is_empty() && path.starts_with(root.as_str());
        let is_anon = anon
            && perms == "rw-p"
            && path.is_empty()
            && b - a >= (8 << 20)
            && !keep.iter().any(|k| (a..b).contains(k));
        if !is_file && !is_anon {
            continue;
        }
        if is_file { file_mb += (b - a) >> 20 } else { anon_mb += (b - a) >> 20 }
        // 32MB pieces for balance
        let mut x = a;
        while x < b && n < MAX_RANGES {
            let l = (32usize << 20).min(b - x);
            unsafe { (*std::ptr::addr_of_mut!(ZAP))[n] = (x, l) };
            n += 1;
            x += l;
        }
    }
    if verbose {
        eprintln!("zap: {n} ranges, file {file_mb} MB, anon {anon_mb} MB");
    }
    drop(maps);
    drop(keep);
    ZAP_N.store(n, SeqCst);
    ZAP_GO.store(true, SeqCst);
    // from here on: no allocation, no destructors, no stdio
    while ZAP_DONE.load(SeqCst) < nt {
        unsafe { libc::sched_yield() };
    }
    unsafe { libc::_exit(0) }
}

/// Coverage builds (feature "cov", -C instrument-coverage): write the profile
/// before _exit skips the runtime's atexit writer.
#[cfg(feature = "cov")]
fn cov_flush() {
    extern "C" {
        fn __llvm_profile_write_file() -> i32;
    }
    unsafe { __llvm_profile_write_file() };
}
#[cfg(not(feature = "cov"))]
#[inline(always)]
fn cov_flush() {}
