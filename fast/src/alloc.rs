//! Global allocator: allocations >= 1 MiB are 2 MiB-aligned anonymous mappings
//! advised MADV_HUGEPAGE (far fewer page faults on the multi-GB working set);
//! everything else goes to the system allocator. Use from a binary:
//! `#[global_allocator] static A: fast::alloc::HugeAlloc = fast::alloc::HugeAlloc;`

use std::alloc::{GlobalAlloc, Layout, System};

pub struct HugeAlloc;

const BIG: usize = 1 << 20;
const HP: usize = 2 << 20;

#[inline]
fn round(n: usize) -> usize {
    (n + HP - 1) & !(HP - 1)
}

unsafe fn map_big(size: usize) -> *mut u8 {
    let len = round(size);
    let raw = libc::mmap(
        std::ptr::null_mut(),
        len + HP,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
        -1,
        0,
    );
    if raw == libc::MAP_FAILED {
        return std::ptr::null_mut();
    }
    let r = raw as usize;
    let a = (r + HP - 1) & !(HP - 1);
    if a > r {
        libc::munmap(raw, a - r);
    }
    let tail = (r + len + HP) - (a + len);
    if tail > 0 {
        libc::munmap((a + len) as *mut libc::c_void, tail);
    }
    libc::madvise(a as *mut libc::c_void, len, libc::MADV_HUGEPAGE);
    a as *mut u8
}

/// Freed big regions (addr, len), recycled instead of munmapped: munmap in a
/// multithreaded process means TLB-shootdown IPIs, and a recycled region is
/// already faulted in.
static CACHE: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

unsafe fn big_alloc(size: usize, zero: bool) -> *mut u8 {
    let len = round(size);
    {
        let mut c = CACHE.lock().unwrap();
        // best fit
        let mut best: Option<usize> = None;
        for (i, &(_, l)) in c.iter().enumerate() {
            if l >= len && best.map_or(true, |b| l < c[b].1) {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let (a, l) = c.swap_remove(i);
            drop(c);
            if zero {
                std::ptr::write_bytes(a as *mut u8, 0, len.min(l));
            }
            // keep the true length: stash it in the region header table
            LENS.lock().unwrap().push((a, l));
            return a as *mut u8;
        }
    }
    let p = map_big(len);
    if !p.is_null() {
        LENS.lock().unwrap().push((p as usize, len));
    }
    p
}

/// True mapping length of live big regions (a recycled region may be larger than asked).
static LENS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

unsafe fn big_free(p: *mut u8) {
    let mut ls = LENS.lock().unwrap();
    let i = ls.iter().position(|&(a, _)| a == p as usize).expect("big_free: unknown region");
    let e = ls.swap_remove(i);
    drop(ls);
    CACHE.lock().unwrap().push(e);
}

unsafe fn big_len(p: *mut u8) -> usize {
    let ls = LENS.lock().unwrap();
    ls.iter().find(|&&(a, _)| a == p as usize).map(|&(_, l)| l).unwrap()
}

unsafe impl GlobalAlloc for HugeAlloc {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG && l.align() <= HP {
            big_alloc(l.size(), false)
        } else {
            System.alloc(l)
        }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG && l.align() <= HP {
            big_alloc(l.size(), true)
        } else {
            System.alloc_zeroed(l)
        }
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() >= BIG && l.align() <= HP {
            big_free(p)
        } else {
            System.dealloc(p, l)
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let old_big = l.size() >= BIG && l.align() <= HP;
        let new_big = new >= BIG && l.align() <= HP;
        if !old_big && !new_big {
            return System.realloc(p, l, new);
        }
        if old_big && new_big && new <= big_len(p) {
            return p;
        }
        let q = self.alloc(Layout::from_size_align_unchecked(new, l.align()));
        if !q.is_null() {
            std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
            self.dealloc(p, l);
        }
        q
    }
}

/// A zero-initialised buffer of `n` Ts in its own 2 MiB-aligned anonymous
/// mapping advised MADV_HUGEPAGE: for big randomly-accessed tables, where 4K
/// pages mean a TLB miss (a 2-D page walk under virtualisation) per access.
/// T must be valid when all-zero.
pub struct HugeBuf<T: Copy> {
    ptr: *mut T,
    n: usize,
    len_bytes: usize,
}

unsafe impl<T: Copy + Send> Send for HugeBuf<T> {}
unsafe impl<T: Copy + Sync> Sync for HugeBuf<T> {}

impl<T: Copy> HugeBuf<T> {
    pub fn zeroed(n: usize) -> Self {
        let bytes = (n * std::mem::size_of::<T>()).max(1);
        let len = round(bytes);
        let p = unsafe { map_big(len) };
        assert!(!p.is_null(), "mmap failed");
        HugeBuf { ptr: p as *mut T, n, len_bytes: len }
    }
    #[inline(always)]
    pub fn as_ptr(&self) -> *const T {
        self.ptr
    }
    #[inline(always)]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.n
    }
}

impl<T: Copy> std::ops::Deref for HugeBuf<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.n) }
    }
}
impl<T: Copy> std::ops::DerefMut for HugeBuf<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.n) }
    }
}
impl<T: Copy> Drop for HugeBuf<T> {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len_bytes) };
    }
}

/// `nreg` append-only regions of `cap` Ts each in one hugepage buffer, with a
/// per-region spill Vec when a region fills. Replaces Vec<Vec<T>> for
/// partition outputs (one mapping instead of hundreds of small 4K-faulted ones).
/// Region starts are skewed by r*64 bytes mod 4096 so the region tails do not
/// all alias the same L1/L2 sets.
pub struct Regions<T: Copy> {
    buf: HugeBuf<u8>,
    base: Vec<*mut T>,
    cap: usize,
    len: Vec<u32>,
    spill: Vec<Vec<T>>,
}

unsafe impl<T: Copy + Send> Send for Regions<T> {}
unsafe impl<T: Copy + Sync> Sync for Regions<T> {}

impl<T: Copy> Regions<T> {
    pub fn new(nreg: usize, cap: usize) -> Self {
        let cap = cap.max(1);
        let sz = std::mem::size_of::<T>();
        assert!(std::mem::align_of::<T>() <= 64);
        let stride = (cap * sz + 4095) & !4095;
        let buf = HugeBuf::<u8>::zeroed(nreg * stride + 64 * 64 + 64);
        let base = (0..nreg).map(|r| unsafe { buf.as_mut_ptr().add(r * stride + (r % 64) * 64) as *mut T }).collect();
        Regions { buf, base, cap, len: vec![0; nreg], spill: (0..nreg).map(|_| Vec::new()).collect() }
    }
    #[inline(always)]
    pub fn push(&mut self, r: usize, x: T) {
        let l = unsafe { *self.len.get_unchecked(r) } as usize;
        if l < self.cap {
            unsafe {
                self.base.get_unchecked(r).add(l).write(x);
                *self.len.get_unchecked_mut(r) += 1;
            }
        } else {
            self.spill[r].push(x);
        }
    }
    #[inline]
    pub fn len_of(&self, r: usize) -> usize {
        self.len[r] as usize + self.spill[r].len()
    }
    /// The region's items: (main part, spill part).
    #[inline]
    pub fn get(&self, r: usize) -> (&[T], &[T]) {
        let l = self.len[r] as usize;
        (unsafe { std::slice::from_raw_parts(self.base[r], l) }, &self.spill[r])
    }
    #[inline]
    pub fn iter(&self, r: usize) -> impl Iterator<Item = &T> {
        let (a, b) = self.get(r);
        a.iter().chain(b.iter())
    }
}

impl<T: Copy> Regions<T> {
    /// Item `i` of region `r` (main part first, then spill).
    #[inline(always)]
    pub fn at(&self, r: usize, i: usize) -> &T {
        let l = unsafe { *self.len.get_unchecked(r) } as usize;
        if i < l {
            unsafe { &*self.base.get_unchecked(r).add(i) }
        } else {
            unsafe { self.spill.get_unchecked(r).get_unchecked(i - l) }
        }
    }
}

impl<T: Copy> Regions<T> {
    /// Calls f(index, item) over region r without a per-item chain branch.
    #[inline(always)]
    pub fn each(&self, r: usize, mut f: impl FnMut(usize, &T)) {
        let (a, b) = self.get(r);
        for (i, x) in a.iter().enumerate() {
            f(i, x);
        }
        let n = a.len();
        for (i, x) in b.iter().enumerate() {
            f(n + i, x);
        }
    }
}
