//! enginebench DATA [N]: clean once, then run engine N times (no output files); prints per-run ms.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static M: mimalloc::MiMalloc = mimalloc::MiMalloc;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let n: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let threads: usize = std::env::var("NCPU").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    fast::set_threads(threads);
    let c = fast::clean::clean(std::path::Path::new(&a[1]), threads);
    let nv: u64 = std::env::var("VARIANTS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut v = vec![vec![]; nv as usize];
    for _ in 0..n {
        for var in 0..nv {
            let radix_mode = std::env::var_os("ABRADIX").is_some();
            fast::engine::VARIANT.store(if radix_mode { 0 } else { var }, std::sync::atomic::Ordering::Relaxed);
            if let Ok(r) = std::env::var(format!("RADIX_MIN{var}")) {
                fast::engine::RADIX_MIN.store(r.parse().unwrap(), std::sync::atomic::Ordering::Relaxed);
            }
            let t = std::time::Instant::now();
            let o = fast::engine::run(&c, false, None);
            v[var as usize].push(t.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(o);
        }
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        eprintln!("thread-summed part work per run {:.1}ms, max part {:.1}ms, max worker {:.1}ms",
            fast::engine::T_PROC.load(Relaxed) as f64 / 1e6 / (n as f64 * nv as f64),
            fast::engine::T_MAXPART.load(Relaxed) as f64 / 1e6, fast::engine::T_WORKER_MAX.load(Relaxed) as f64 / 1e6);
        let mut sizes: Vec<(usize, usize, usize)> = c.parts.iter().map(|p| (p.ev.len(), p.cl.len(), p.person_names.len())).collect();
        sizes.sort();
        eprintln!("parts ev/cl/persons: min {:?} med {:?} max {:?}", sizes[0], sizes[sizes.len()/2], sizes[sizes.len()-1]);
    }
    eprintln!("first run {:.1} ms", v[0][0]);
    for (i, v) in v.iter_mut().enumerate() {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!("variant {i} engine ms: min {:.1} med {:.1} max {:.1}", v[0], v[n / 2], v[n - 1]);
    }
}
