pub mod alloc;
pub mod cache;
pub mod clean;
pub mod csv;
pub mod engine;
pub mod gid;
pub mod ids;
pub mod keys;
pub mod loader;
pub mod model;
pub mod prof;
pub mod ts;
pub mod pq;
pub mod writer;

pub const DAY_MS: i64 = 86_400_000;
pub const WEEK_MS: u32 = 604_800_000;
pub const GAP_MS: u32 = 1_800_000;

static THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub fn set_threads(n: usize) {
    THREADS.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
}
pub fn threads() -> usize {
    match THREADS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8),
        n => n,
    }
}
