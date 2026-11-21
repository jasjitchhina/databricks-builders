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
    _buf: Option<HugeBuf<u8>>,
    _vbuf: Vec<u8>,
    base: Vec<*mut T>,
    cap: usize,
    len: Vec<u32>,
    spill: Vec<Vec<T>>,
}

unsafe impl<T: Copy + Send> Send for Regions<T> {}
unsafe impl<T: Copy + Sync> Sync for Regions<T> {}

impl<T: Copy> Regions<T> {
    pub fn new(nreg: usize, cap: usize) -> Self {
        Self::new_with(nreg, cap, true)
    }
    /// `huge`: back with a THP mapping; else with a plain (malloc) buffer.
    pub fn new_with(nreg: usize, cap: usize, huge: bool) -> Self {
        let cap = cap.max(1);
        let sz = std::mem::size_of::<T>();
        assert!(std::mem::align_of::<T>() <= 64);
        let stride = (cap * sz + 64 * 64 + 4095) & !4095; // room for the max skew
        let bytes = nreg * stride + 64 * 64 + 4096;
        let (hb, mut vb) = if huge { (Some(HugeBuf::<u8>::zeroed(bytes)), Vec::new()) } else { (None, Vec::<u8>::with_capacity(bytes)) };
        let p0 = match &hb { Some(h) => h.as_mut_ptr(), None => vb.as_mut_ptr() };
        let p0 = ((p0 as usize + 4095) & !4095) as *mut u8;
        let base = (0..nreg).map(|r| unsafe { p0.add(r * stride + (r % 64) * 64) as *mut T }).collect();
        Regions { _buf: hb, _vbuf: vb, base, cap, len: vec![0; nreg], spill: (0..nreg).map(|_| Vec::new()).collect() }
    }
    pub fn clear(&mut self) {
        self.len.iter_mut().for_each(|l| *l = 0);
        self.spill.iter_mut().for_each(|v| v.clear());
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

/// Partitioned append buffer with software write-combining: each region keeps
/// K items staged in a small cache-resident buffer and flushes them as whole
/// 64-byte lines with non-temporal stores (no read-for-ownership of the
/// destination). K * size_of::<T>() must be a multiple of 64. Call `finish`
/// before reading.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct Stage<T: Copy, const K: usize>(pub [T; K]);

pub struct WcRegions<T: Copy, const K: usize> {
    stage: Vec<Stage<T, K>>,
    nst: Vec<u8>,
    _vbuf: Vec<u8>,
    _hbuf: Option<HugeBuf<u8>>,
    base: Vec<*mut T>,
    cap: usize,
    len: Vec<u32>,
    spill: Vec<Vec<T>>,
}

unsafe impl<T: Copy + Send, const K: usize> Send for WcRegions<T, K> {}
unsafe impl<T: Copy + Sync, const K: usize> Sync for WcRegions<T, K> {}

impl<T: Copy, const K: usize> WcRegions<T, K> {
    pub fn new(nreg: usize, cap: usize) -> Self {
        Self::new_with(nreg, cap, false)
    }
    pub fn new_with(nreg: usize, cap: usize, huge: bool) -> Self {
        let sz = std::mem::size_of::<T>();
        assert!((K * sz) % 64 == 0 && std::mem::align_of::<T>() <= 64);
        let cap = (cap.max(K) + K - 1) / K * K;
        let stride = (cap * sz + 64 * 64 + 4095) & !4095; // room for the max skew
        let bytes = nreg * stride + 64 * 64 + 4096;
        let (hb, mut vb) = if huge { (Some(HugeBuf::<u8>::zeroed(bytes)), Vec::new()) } else { (None, Vec::<u8>::with_capacity(bytes)) };
        let p0 = match &hb { Some(h) => h.as_mut_ptr(), None => vb.as_mut_ptr() };
        let p0 = ((p0 as usize + 4095) & !4095) as *mut u8;
        let base = (0..nreg).map(|r| unsafe { p0.add(r * stride + (r % 64) * 64) as *mut T }).collect();
        WcRegions {
            stage: vec![Stage(unsafe { std::mem::zeroed() }); nreg],
            nst: vec![0; nreg],
            _vbuf: vb,
            _hbuf: hb,
            base,
            cap,
            len: vec![0; nreg],
            spill: (0..nreg).map(|_| Vec::new()).collect(),
        }
    }
    #[inline(always)]
    pub fn push(&mut self, r: usize, x: T) {
        unsafe {
            let k = *self.nst.get_unchecked(r) as usize;
            let st = self.stage.get_unchecked_mut(r);
            *st.0.get_unchecked_mut(k) = x;
            if k + 1 < K {
                *self.nst.get_unchecked_mut(r) = (k + 1) as u8;
            } else {
                *self.nst.get_unchecked_mut(r) = 0;
                self.flush_full(r);
            }
        }
    }
    #[inline(always)]
    unsafe fn flush_full(&mut self, r: usize) {
        let l = *self.len.get_unchecked(r) as usize;
        if l + K <= self.cap {
            let dst = self.base.get_unchecked(r).add(l) as *mut u8;
            let src = self.stage.get_unchecked(r) as *const Stage<T, K> as *const u8;
            nt_copy(dst, src, K * std::mem::size_of::<T>());
            *self.len.get_unchecked_mut(r) += K as u32;
        } else {
            let st = self.stage[r];
            self.spill[r].extend_from_slice(&st.0);
        }
    }
    /// Flushes partial stages; afterwards `get` sees every item.
    pub fn finish(&mut self) {
        for r in 0..self.nst.len() {
            let k = self.nst[r] as usize;
            let st = self.stage[r];
            for i in 0..k {
                let l = self.len[r] as usize;
                if l < self.cap {
                    unsafe { self.base[r].add(l).write(st.0[i]) };
                    self.len[r] += 1;
                } else {
                    self.spill[r].push(st.0[i]);
                }
            }
            self.nst[r] = 0;
        }
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
    }
    /// The whole backing buffer (base, bytes), e.g. to recycle it once the
    /// regions are dead.
    pub fn raw(&self) -> (*mut u8, usize) {
        match &self._hbuf {
            Some(h) => (h.as_mut_ptr(), h.len()),
            None => (self._vbuf.as_ptr() as *mut u8, self._vbuf.capacity()),
        }
    }
    pub fn clear(&mut self) {
        self.len.iter_mut().for_each(|l| *l = 0);
        self.nst.iter_mut().for_each(|l| *l = 0);
        self.spill.iter_mut().for_each(|v| v.clear());
    }
    #[inline]
    pub fn len_of(&self, r: usize) -> usize {
        self.len[r] as usize + self.spill[r].len()
    }
    #[inline]
    pub fn get(&self, r: usize) -> (&[T], &[T]) {
        (unsafe { std::slice::from_raw_parts(self.base[r], self.len[r] as usize) }, &self.spill[r])
    }
    #[inline(always)]
    pub fn at(&self, r: usize, i: usize) -> &T {
        let l = unsafe { *self.len.get_unchecked(r) } as usize;
        if i < l {
            unsafe { &*self.base.get_unchecked(r).add(i) }
        } else {
            unsafe { self.spill.get_unchecked(r).get_unchecked(i - l) }
        }
    }
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

#[inline(always)]
unsafe fn nt_copy(dst: *mut u8, src: *const u8, n: usize) {
    #[cfg(target_feature = "avx2")]
    {
        use std::arch::x86_64::*;
        let mut i = 0;
        while i < n {
            let v = _mm256_load_si256(src.add(i) as *const __m256i);
            _mm256_stream_si256(dst.add(i) as *mut __m256i, v);
            i += 32;
        }
    }
    #[cfg(not(target_feature = "avx2"))]
    std::ptr::copy_nonoverlapping(src, dst, n);
}

/// Orders this thread's non-temporal stores before anything that follows
/// (must run on the thread that issued them, before the data is shared).
#[inline(always)]
pub fn sfence() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_sfence()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wc_regions_full_and_spill() {
        for &(nreg, cap) in &[(256usize, 85usize), (70, 64), (3, 1), (65, 4096 / 48 * 4)] {
            let mut w = WcRegions::<[u64; 6], 4>::new(nreg, cap);
            let mut r2 = Regions::<[u64; 6]>::new_with(nreg, cap, false);
            let per = cap + 13;
            for i in 0..per {
                for r in 0..nreg {
                    let x = [r as u64, i as u64, 7, 7, 7, 7];
                    w.push(r, x);
                    r2.push(r, x);
                }
            }
            w.finish();
            for r in 0..nreg {
                assert_eq!(w.len_of(r), per);
                assert_eq!(r2.len_of(r), per);
                let mut n = 0;
                w.each(r, |i, x| {
                    assert_eq!(*x, [r as u64, i as u64, 7, 7, 7, 7]);
                    assert_eq!(w.at(r, i), x);
                    n += 1;
                });
                assert_eq!(n, per);
                for (i, x) in r2.iter(r).enumerate() {
                    assert_eq!(*x, [r as u64, i as u64, 7, 7, 7, 7]);
                }
            }
        }
    }
}

/// Bump allocator over recycled (already faulted-in) memory ranges.
pub struct Bump {
    ranges: Vec<(usize, usize)>,
    i: usize,
    p: usize,
}

unsafe impl Send for Bump {}

impl Bump {
    /// # Safety: each range must be writable and unused for the rest of the process (slices are 'static).
    pub unsafe fn new(base: *mut u8, len: usize) -> Self {
        Self::multi(vec![(base, len)])
    }
    /// # Safety: as `new`, for every range.
    pub unsafe fn multi(r: Vec<(*mut u8, usize)>) -> Self {
        let ranges: Vec<(usize, usize)> = r.into_iter().filter(|r| r.1 > 0).map(|(b, l)| (b as usize, b as usize + l)).collect();
        let p = ranges.first().map_or(0, |r| r.0);
        Bump { ranges, i: 0, p }
    }
    /// Uninitialized room for n Ts, or None if no range has room left.
    pub fn take<T>(&mut self, n: usize) -> Option<*mut T> {
        let need = n * std::mem::size_of::<T>();
        while self.i < self.ranges.len() {
            let a = (self.p + 63) & !63;
            if a + need <= self.ranges[self.i].1 {
                self.p = a + need;
                return Some(a as *mut T);
            }
            self.i += 1;
            if self.i < self.ranges.len() {
                self.p = self.ranges[self.i].0;
            }
        }
        None
    }
    /// Unused memory left: (base, bytes) per range.
    pub fn rest_all(&self) -> Vec<(*mut u8, usize)> {
        let mut v = Vec::new();
        for (j, &(s, e)) in self.ranges.iter().enumerate().skip(self.i) {
            let a = if j == self.i { (self.p + 63) & !63 } else { s };
            if e > a {
                v.push((a as *mut u8, e - a));
            }
        }
        v
    }
    /// The unused rest of the current range.
    pub fn rest(&self) -> (*mut u8, usize) {
        let a = (self.p + 63) & !63;
        match self.ranges.get(self.i) {
            Some(&(_, e)) => (a as *mut u8, e.saturating_sub(a)),
            None => (std::ptr::null_mut(), 0),
        }
    }
    /// Gives back the tail of the last `take` (used < taken).
    pub fn shrink_last<T>(&mut self, at: *mut T, used: usize) {
        self.p = at as usize + used * std::mem::size_of::<T>();
    }
}

/// Global-allocator wrapper that serves large allocations (>= 64 KiB) from
/// "donated" memory: buffers the clean stage no longer needs and that are
/// already faulted in, so later stages (engine scratch) avoid first-touch page
/// faults/zeroing. Donated memory is never returned: frees of it are ignored,
/// and a realloc of it moves to a new block. Everything else goes to `A`.
pub struct DonorAlloc<A: GlobalAlloc>(pub A);

const DMIN: usize = 64 << 10;

struct Donor {
    ranges: Vec<(usize, usize)>, // donated [start, end)
    cur: usize,                  // index of the range being bumped
    p: usize,                    // bump pointer in ranges[cur]
}

static DONOR: std::sync::Mutex<Donor> = std::sync::Mutex::new(Donor { ranges: Vec::new(), cur: 0, p: 0 });
static DONATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Donates [p, p+len) to the allocator. The memory must stay mapped and
/// unused by anyone else for the rest of the process.
pub unsafe fn donate(p: *mut u8, len: usize) {
    let s = (p as usize + 4095) & !4095;
    let e = (p as usize + len) & !4095;
    if e <= s + DMIN {
        return;
    }
    let mut d = DONOR.lock().unwrap();
    if d.ranges.is_empty() {
        d.p = s;
    }
    d.ranges.push((s, e));
    DONATED.store(true, std::sync::atomic::Ordering::Release);
}

fn donor_take(l: Layout) -> *mut u8 {
    let mut d = DONOR.lock().unwrap();
    let align = l.align().max(64);
    while d.cur < d.ranges.len() {
        let (_, e) = d.ranges[d.cur];
        let a = (d.p + align - 1) & !(align - 1);
        if a + l.size() <= e {
            d.p = a + l.size();
            return a as *mut u8;
        }
        d.cur += 1;
        if d.cur < d.ranges.len() {
            d.p = d.ranges[d.cur].0;
        }
    }
    std::ptr::null_mut()
}

fn in_donor(p: *mut u8) -> bool {
    let a = p as usize;
    DONOR.lock().unwrap().ranges.iter().any(|&(s, e)| a >= s && a < e)
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for DonorAlloc<A> {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() >= DMIN && DONATED.load(std::sync::atomic::Ordering::Relaxed) {
            let p = donor_take(l);
            if !p.is_null() {
                return p;
            }
        }
        self.0.alloc(l)
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() >= DMIN && DONATED.load(std::sync::atomic::Ordering::Relaxed) {
            let p = donor_take(l);
            if !p.is_null() {
                std::ptr::write_bytes(p, 0, l.size());
                return p;
            }
        }
        self.0.alloc_zeroed(l)
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() >= DMIN && DONATED.load(std::sync::atomic::Ordering::Relaxed) && in_donor(p) {
            return;
        }
        self.0.dealloc(p, l)
    }
    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let donated = DONATED.load(std::sync::atomic::Ordering::Relaxed);
        if donated && (l.size() >= DMIN && in_donor(p) || new >= DMIN) {
            let nl = Layout::from_size_align_unchecked(new, l.align());
            let q = self.alloc(nl);
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                self.dealloc(p, l);
            }
            return q;
        }
        self.0.realloc(p, l, new)
    }
}

/// Like `WcRegions`, but each region is a chain of 4 KiB blocks carved from
/// per-owner 64 MiB slabs: no per-region capacity guess, no spill, slack of
/// at most one block per region, adapts to uneven work per thread.
pub struct BlockRegions<T: Copy, const K: usize> {
    stage: Vec<Stage<T, K>>,
    nst: Vec<u8>,
    huge: bool,
    slabs: Vec<(Option<HugeBuf<u8>>, Vec<u8>, *mut u8)>,
    slab_next: usize,
    /// per region: block addresses, items in the last block
    blocks: Vec<Vec<*mut T>>,
    fill: Vec<u32>,
    bitems: usize,
}

unsafe impl<T: Copy + Send, const K: usize> Send for BlockRegions<T, K> {}
unsafe impl<T: Copy + Sync, const K: usize> Sync for BlockRegions<T, K> {}

const BLK: usize = 32768;
const SLAB: usize = 64 << 20;

impl<T: Copy, const K: usize> BlockRegions<T, K> {
    pub fn new(nreg: usize, huge: bool) -> Self {
        let sz = std::mem::size_of::<T>();
        assert!((K * sz) % 64 == 0 && std::mem::align_of::<T>() <= 64 && K * sz <= BLK);
        BlockRegions {
            stage: vec![Stage(unsafe { std::mem::zeroed() }); nreg],
            nst: vec![0; nreg],
            huge,
            slabs: Vec::new(),
            slab_next: SLAB,
            blocks: (0..nreg).map(|_| Vec::new()).collect(),
            fill: vec![0; nreg],
            bitems: BLK / (K * sz) * K,
        }
    }
    #[cold]
    fn new_slab(&mut self) {
        let (hb, mut vb) = if self.huge { (Some(HugeBuf::<u8>::zeroed(SLAB)), Vec::new()) } else { (None, Vec::<u8>::with_capacity(SLAB + BLK)) };
        let p = match &hb { Some(h) => h.as_mut_ptr(), None => ((vb.as_mut_ptr() as usize + BLK - 1) & !(BLK - 1)) as *mut u8 };
        self.slabs.push((hb, vb, p));
        self.slab_next = 0;
    }
    #[inline(always)]
    fn new_block(&mut self) -> *mut T {
        if self.slab_next + BLK > SLAB {
            self.new_slab();
        }
        let p = unsafe { self.slabs.last().unwrap().2.add(self.slab_next) } as *mut T;
        self.slab_next += BLK;
        p
    }
    #[inline(always)]
    pub fn push(&mut self, r: usize, x: T) {
        unsafe {
            let k = *self.nst.get_unchecked(r) as usize;
            let st = self.stage.get_unchecked_mut(r);
            *st.0.get_unchecked_mut(k) = x;
            if k + 1 < K {
                *self.nst.get_unchecked_mut(r) = (k + 1) as u8;
            } else {
                *self.nst.get_unchecked_mut(r) = 0;
                self.flush_full(r);
            }
        }
    }
    #[inline(always)]
    unsafe fn flush_full(&mut self, r: usize) {
        let f = *self.fill.get_unchecked(r) as usize;
        let dst = if f + K <= self.bitems && !self.blocks.get_unchecked(r).is_empty() {
            self.blocks.get_unchecked(r).last().unwrap().add(f)
        } else {
            let b = self.new_block();
            self.blocks.get_unchecked_mut(r).push(b);
            *self.fill.get_unchecked_mut(r) = 0;
            b
        };
        let src = self.stage.get_unchecked(r) as *const Stage<T, K> as *const u8;
        nt_copy(dst as *mut u8, src, K * std::mem::size_of::<T>());
        *self.fill.get_unchecked_mut(r) += K as u32;
    }
    /// Flushes partial stages; afterwards reads see every item.
    pub fn finish(&mut self) {
        for r in 0..self.nst.len() {
            let k = self.nst[r] as usize;
            let st = self.stage[r];
            for i in 0..k {
                let f = self.fill[r] as usize;
                let dst = if f < self.bitems && !self.blocks[r].is_empty() {
                    unsafe { self.blocks[r].last().unwrap().add(f) }
                } else {
                    let b = self.new_block();
                    self.blocks[r].push(b);
                    self.fill[r] = 0;
                    b
                };
                unsafe { dst.write(st.0[i]) };
                self.fill[r] += 1;
            }
            self.nst[r] = 0;
        }
        sfence();
    }
    #[inline]
    pub fn len_of(&self, r: usize) -> usize {
        let nb = self.blocks[r].len();
        if nb == 0 {
            0
        } else {
            (nb - 1) * self.bitems + self.fill[r] as usize
        }
    }
    #[inline(always)]
    pub fn at(&self, r: usize, i: usize) -> &T {
        unsafe { &*self.blocks.get_unchecked(r).get_unchecked(i / self.bitems).add(i % self.bitems) }
    }
    #[inline(always)]
    pub fn each(&self, r: usize, mut f: impl FnMut(usize, &T)) {
        let bl = &self.blocks[r];
        let nb = bl.len();
        let mut idx = 0;
        for (j, &b) in bl.iter().enumerate() {
            let n = if j + 1 == nb { self.fill[r] as usize } else { self.bitems };
            let s = unsafe { std::slice::from_raw_parts(b, n) };
            for x in s {
                f(idx, x);
                idx += 1;
            }
        }
    }
    /// The slabs (base, bytes), e.g. to recycle them once the regions are dead.
    pub fn slabs(&self) -> Vec<(*mut u8, usize)> {
        let n = self.slabs.len();
        self.slabs.iter().enumerate().map(|(i, s)| (s.2, if i + 1 == n { self.slab_next } else { SLAB })).collect()
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;
    #[test]
    fn block_regions_roundtrip() {
        for &(nreg, per, huge) in &[(256usize, 1000usize, false), (7, 50000, true), (3, 1, false), (1024, 37, true)] {
            let mut w = BlockRegions::<[u64; 4], 4>::new(nreg, huge);
            let mut c = BlockRegions::<[u64; 3], 8>::new(nreg, huge);
            for i in 0..per {
                for r in 0..nreg {
                    w.push(r, [r as u64, i as u64, 7, 7]);
                    c.push(r, [r as u64, i as u64, 9]);
                }
            }
            w.finish();
            c.finish();
            for r in 0..nreg {
                assert_eq!(w.len_of(r), per);
                assert_eq!(c.len_of(r), per);
                let mut n = 0;
                w.each(r, |i, x| {
                    assert_eq!(*x, [r as u64, i as u64, 7, 7]);
                    assert_eq!(w.at(r, i), x);
                    n += 1;
                });
                c.each(r, |i, x| {
                    assert_eq!(*x, [r as u64, i as u64, 9]);
                    assert_eq!(c.at(r, i), x);
                });
                assert_eq!(n, per);
            }
        }
    }
}
