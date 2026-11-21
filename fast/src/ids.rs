//! Exact ID keys. An ID of at most 15 bytes packs into a u128: its bytes
//! big-endian in the top 15 bytes, zero padded, and its length in the low byte.
//! Plain u128 order on such keys is exactly byte-wise order on the IDs, and
//! equality is exact. Longer IDs (the generator never writes one) are interned
//! into a global arena and get low byte 0xFF; `key_cmp` / `key_bytes` handle them.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub type Key = u128;

pub const LONG: u128 = 0xFF;

struct Arena {
    map: HashMap<Box<[u8]>, u32>,
    strs: Vec<Box<[u8]>>,
}

static ARENA: Mutex<Option<Arena>> = Mutex::new(None);

#[inline(always)]
pub fn pack(b: &[u8]) -> Key {
    let n = b.len();
    if n <= 15 {
        let mut a = [0u8; 16];
        a[..n].copy_from_slice(b);
        u128::from_be_bytes(a) | n as u128
    } else {
        pack_long(b)
    }
}

#[cold]
#[inline(never)]
fn pack_long(b: &[u8]) -> Key {
    let mut g = ARENA.lock().unwrap();
    let a = g.get_or_insert_with(|| Arena { map: HashMap::new(), strs: Vec::new() });
    let idx = match a.map.get(b) {
        Some(&i) => i,
        None => {
            let i = a.strs.len() as u32;
            a.strs.push(b.into());
            a.map.insert(b.into(), i);
            i
        }
    };
    ((idx as u128 + 1) << 8) | LONG
}

#[inline(always)]
pub fn is_long(k: Key) -> bool {
    (k & 0xFF) == LONG
}

pub fn any_long() -> bool {
    ARENA.lock().unwrap().as_ref().map_or(false, |a| !a.strs.is_empty())
}

pub fn key_bytes(k: Key) -> Box<[u8]> {
    if is_long(k) {
        let g = ARENA.lock().unwrap();
        let idx = ((k >> 8) - 1) as usize;
        g.as_ref().unwrap().strs[idx].clone()
    } else {
        let n = (k & 0xFF) as usize;
        let a = k.to_be_bytes();
        a[..n].into()
    }
}

#[inline(always)]
pub fn key_cmp(a: Key, b: Key) -> Ordering {
    if !is_long(a) && !is_long(b) {
        a.cmp(&b)
    } else {
        key_bytes(a).cmp(&key_bytes(b))
    }
}

/// 64-bit mix of a key; good in both high and low bits.
#[inline(always)]
pub fn hk(k: Key) -> u64 {
    let lo = k as u64;
    let hi = (k >> 64) as u64;
    let mut x = lo ^ hi.rotate_left(29).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 32)
}

pub const NPART: usize = 256;

/// The part a person lives in, by the key of their name.
#[inline(always)]
pub fn part_of(name: Key) -> usize {
    (hk(name) >> 56) as usize
}

/// Open-addressing read-only map Key -> Key. Key 0 (the empty ID) is the
/// empty-slot marker; empty IDs never get here.
pub struct KeyMap {
    mask: usize,
    len: usize,
    slots: Vec<(Key, Key)>,
}

impl KeyMap {
    pub fn with_capacity(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(16);
        KeyMap { mask: cap - 1, len: 0, slots: vec![(0, 0); cap] }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    pub fn insert(&mut self, k: Key, v: Key) {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut i = hk(k) as usize & self.mask;
        loop {
            let s = &mut self.slots[i];
            if s.0 == 0 {
                *s = (k, v);
                self.len += 1;
                return;
            }
            if s.0 == k {
                s.1 = v;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[cold]
    fn grow(&mut self) {
        let old = std::mem::take(&mut self.slots);
        let cap = old.len() * 2;
        self.slots = vec![(0, 0); cap];
        self.mask = cap - 1;
        self.len = 0;
        for (k, v) in old {
            if k != 0 {
                self.insert(k, v);
            }
        }
    }
    #[inline(always)]
    pub fn prefetch(&self, k: Key) {
        let i = hk(k) as usize & self.mask;
        prefetch(unsafe { self.slots.as_ptr().add(i) } as *const u8);
    }
    #[inline(always)]
    pub fn get(&self, k: Key) -> Option<Key> {
        let mut i = hk(k) as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.0 == 0 {
                return None;
            }
            if s.0 == k {
                return Some(s.1);
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order() {
        let v: Vec<&[u8]> = vec![b"", b"\0", b"a", b"a\0", b"a\0\0", b"ab", b"b", b"U1", b"u1", b" u1", b"u1 ", b"012345678901234"];
        for x in &v {
            for y in &v {
                assert_eq!(pack(x).cmp(&pack(y)), x.cmp(y), "{:?} {:?}", x, y);
                assert_eq!(&*key_bytes(pack(x)), *x);
            }
        }
        let l1 = pack(b"0123456789012345");
        let l2 = pack(b"0123456789012346");
        assert!(is_long(l1));
        assert_eq!(key_cmp(l1, l2), Ordering::Less);
        assert_eq!(key_cmp(pack(b"1"), l1), Ordering::Greater);
        assert_eq!(l1, pack(b"0123456789012345"));
    }
}

#[inline(always)]
pub fn prefetch(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
}

/// KeyMap sharded by the top 8 bits of the hash, so shards can be built in parallel.
pub struct ShardedMap {
    pub shards: Vec<KeyMap>,
}

impl ShardedMap {
    #[inline(always)]
    pub fn shard_of(k: Key) -> usize {
        (hk(k) >> 56) as usize
    }
    #[inline(always)]
    pub fn get(&self, k: Key) -> Option<Key> {
        unsafe { self.shards.get_unchecked(Self::shard_of(k)) }.get(k)
    }
    #[inline(always)]
    pub fn prefetch(&self, k: Key) {
        unsafe { self.shards.get_unchecked(Self::shard_of(k)) }.prefetch(k)
    }
}
