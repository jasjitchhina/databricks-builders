//! Tiny in-process sampling profiler (dev tool): SIGPROF every `us` of
//! process CPU time records the interrupted RIP; `stop` writes them
//! (relative to the executable's load base) for addr2line.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const CAP: usize = 1 << 20;
static SAMPLES: [AtomicU64; CAP] = [const { AtomicU64::new(0) }; CAP];
static N: AtomicUsize = AtomicUsize::new(0);

extern "C" fn handler(_sig: libc::c_int, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    unsafe {
        let uc = ctx as *const libc::ucontext_t;
        let rip = (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64;
        let i = N.fetch_add(1, Ordering::Relaxed);
        if i < CAP {
            SAMPLES[i].store(rip, Ordering::Relaxed);
        }
    }
}

pub fn start(us: i64) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
        let it = libc::itimerval { it_interval: libc::timeval { tv_sec: 0, tv_usec: us }, it_value: libc::timeval { tv_sec: 0, tv_usec: us } };
        libc::setitimer(libc::ITIMER_PROF, &it, std::ptr::null_mut());
    }
}

pub fn stop(path: &str) {
    unsafe {
        let it: libc::itimerval = std::mem::zeroed();
        libc::setitimer(libc::ITIMER_PROF, &it, std::ptr::null_mut());
    }
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let exe = std::fs::read_link("/proc/self/exe").unwrap();
    let exe_s = exe.to_string_lossy().to_string();
    // load base = start of the first mapping of the executable (offset 0)
    let mut base = 0u64;
    for l in maps.lines() {
        if l.ends_with(&exe_s) {
            let f: Vec<&str> = l.split_whitespace().collect();
            let start = u64::from_str_radix(f[0].split('-').next().unwrap(), 16).unwrap();
            let off = u64::from_str_radix(f[2], 16).unwrap();
            if off == 0 {
                base = start;
                break;
            }
        }
    }
    let n = N.load(Ordering::Relaxed).min(CAP);
    let mut out = String::new();
    for i in 0..n {
        let r = SAMPLES[i].load(Ordering::Relaxed);
        if r >= base {
            out.push_str(&format!("{:x}\n", r - base));
        }
    }
    std::fs::write(path, out).unwrap();
    eprintln!("prof: {n} samples -> {path} (exe {exe_s})");
}
