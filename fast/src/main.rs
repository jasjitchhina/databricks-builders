use std::path::PathBuf;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static M: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
    if std::env::var_os("FAST_NO_ZAP").is_none() {
        zap_input_mappings(&data, nthreads);
    }
    // Skip destructors: freeing millions of small allocations at exit is pure waste.
    std::process::exit(0);
}

/// Cold page cache insurance: ask the kernel to start reading every input file now
/// (POSIX_FADV_WILLNEED, async readahead) from a background thread, in the order the
/// pipeline consumes them (links, clicks, events). Nearly free when the cache is warm.
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
    for _ in 0..nt {
        let (files, next) = (files.clone(), next.clone());
        std::thread::spawn(move || {
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
        });
    }
}

/// After all outputs are written: drop the page-table entries of the (leaked) CSV
/// mappings with MADV_DONTNEED from several threads in parallel (it runs under the
/// mmap read lock, unlike munmap), so the single-threaded exit teardown has nothing
/// left to walk. Only read-only file mappings under the data dir are touched; a stray
/// later access would just re-fault the same bytes from the page cache.
fn zap_input_mappings(data: &std::path::Path, nthreads: usize) {
    let Ok(root) = std::fs::canonicalize(data) else { return };
    let root = root.to_string_lossy().into_owned();
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else { return };
    let mut ranges: Vec<(usize, usize)> = vec![];
    for line in maps.lines() {
        // addr perms offset dev inode path
        let mut it = line.split_whitespace();
        let (Some(addr), Some(perms)) = (it.next(), it.next()) else { continue };
        let path = it.nth(3).unwrap_or("");
        if !perms.starts_with("r--") || !path.starts_with(root.as_str()) {
            continue;
        }
        let Some((a, b)) = addr.split_once('-') else { continue };
        let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else { continue };
        // split into 64MB pieces for balance
        let mut x = a;
        while x < b {
            let l = (64usize << 20).min(b - x);
            ranges.push((x, l));
            x += l;
        }
    }
    if ranges.is_empty() {
        return;
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..nthreads.clamp(1, 16) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&(a, l)) = ranges.get(i) else { break };
                unsafe { libc::madvise(a as *mut libc::c_void, l, libc::MADV_DONTNEED) };
            });
        }
    });
}
