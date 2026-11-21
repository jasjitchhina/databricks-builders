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

unsafe impl GlobalAlloc for HugeAlloc {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG && l.align() <= HP {
            map_big(l.size())
        } else {
            System.alloc(l)
        }
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if l.size() >= BIG && l.align() <= HP {
            map_big(l.size())
        } else {
            System.alloc_zeroed(l)
        }
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() >= BIG && l.align() <= HP {
            libc::munmap(p as *mut libc::c_void, round(l.size()));
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
        if old_big && new_big && round(new) == round(l.size()) {
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
