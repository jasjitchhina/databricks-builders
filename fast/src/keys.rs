//! Generic ID keys for the clean stage.
//!
//! * `u128`: the exact packing of `ids` (any ID; long ones via the arena).
//! * `u64` ("compact"): an ID of at most 10 bytes drawn from `0-9 A-Z _ a-z`
//!   packed as 6-bit codes, big-endian from bit 63, zero padded. Codes are
//!   assigned in ASCII order and padding (0) sorts below every code, so plain
//!   u64 order is exactly byte-wise order and equality is exact. Any other ID
//!   is not representable: the compact pipeline then reruns in u128 mode.

use crate::ids::{self, prefetch};

pub trait IdKey: Copy + Eq + Send + Sync + Default + 'static {
    const WIDE: bool;
    /// Packs field `f` of mapping `m` (may over-read `m`). None: not representable.
    fn pack(m: &[u8], f: &[u8]) -> Option<Self>;
    fn hash(self) -> u64;
    /// Byte-wise `a < b`.
    fn less(a: Self, b: Self) -> bool;
    fn bytes(self) -> Box<[u8]>;
    /// Order-preserving u128 image (for click_id tie-breaks).
    fn click(self) -> u128;
    fn is_long(self) -> bool;
    #[inline(always)]
    fn is_zero(self) -> bool {
        self == Self::default()
    }
}

impl IdKey for u128 {
    const WIDE: bool = true;
    #[inline(always)]
    fn pack(m: &[u8], f: &[u8]) -> Option<Self> {
        Some(crate::csv::pack_in(m, f))
    }
    #[inline(always)]
    fn hash(self) -> u64 {
        ids::hk(self)
    }
    #[inline(always)]
    fn less(a: Self, b: Self) -> bool {
        ids::key_cmp(a, b) == std::cmp::Ordering::Less
    }
    fn bytes(self) -> Box<[u8]> {
        ids::key_bytes(self)
    }
    #[inline(always)]
    fn click(self) -> u128 {
        self
    }
    #[inline(always)]
    fn is_long(self) -> bool {
        ids::is_long(self)
    }
}

const fn code_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut c = 0usize;
    let mut code = 1u8;
    while c < 256 {
        let b = c as u8;
        if b.is_ascii_digit() || b.is_ascii_uppercase() || b == b'_' || b.is_ascii_lowercase() {
            t[c] = code;
            code += 1;
        }
        c += 1;
    }
    t
}
const fn decode_table() -> [u8; 64] {
    let mut t = [0u8; 64];
    let mut c = 0usize;
    while c < 256 {
        let k = CODE[c];
        if k != 0 {
            t[k as usize] = c as u8;
        }
        c += 1;
    }
    t
}
static CODE: [u8; 256] = code_table();
static DECODE: [u8; 64] = decode_table();

impl IdKey for u64 {
    const WIDE: bool = false;
    #[inline(always)]
    fn pack(_m: &[u8], f: &[u8]) -> Option<Self> {
        let n = f.len();
        if n > 10 {
            return None;
        }
        let mut k: u64 = 0;
        let mut ok: u8 = 1;
        for (i, &c) in f.iter().enumerate() {
            let code = CODE[c as usize];
            ok &= (code != 0) as u8;
            k |= (code as u64) << (58 - 6 * i);
        }
        if ok == 0 {
            None
        } else {
            Some(k)
        }
    }
    #[inline(always)]
    fn hash(self) -> u64 {
        let mut x = self ^ (self >> 29);
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 29)
    }
    #[inline(always)]
    fn less(a: Self, b: Self) -> bool {
        a < b
    }
    fn bytes(self) -> Box<[u8]> {
        let mut v = Vec::with_capacity(10);
        for i in 0..10 {
            let code = ((self >> (58 - 6 * i)) & 63) as usize;
            if code == 0 {
                break;
            }
            v.push(DECODE[code]);
        }
        v.into()
    }
    #[inline(always)]
    fn click(self) -> u128 {
        self as u128
    }
    #[inline(always)]
    fn is_long(self) -> bool {
        false
    }
}

/// Open-addressing map K -> V; the zero key marks an empty slot (IDs are never empty).
pub struct GMap<K: IdKey, V: Copy + Default> {
    mask: usize,
    len: usize,
    slots: Vec<(K, V)>,
}

impl<K: IdKey, V: Copy + Default> GMap<K, V> {
    pub fn with_capacity(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(16);
        GMap { mask: cap - 1, len: 0, slots: vec![(K::default(), V::default()); cap] }
    }
    #[inline(always)]
    pub fn get(&self, k: K) -> Option<V> {
        let mut i = k.hash() as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.0.is_zero() {
                return None;
            }
            if s.0 == k {
                return Some(s.1);
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Returns the existing value, or inserts `v` and returns None.
    #[inline(always)]
    pub fn get_or_insert(&mut self, k: K, v: V) -> Option<V> {
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut i = k.hash() as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked_mut(i) };
            if s.0.is_zero() {
                *s = (k, v);
                self.len += 1;
                return None;
            }
            if s.0 == k {
                return Some(s.1);
            }
            i = (i + 1) & self.mask;
        }
    }
    #[cold]
    fn grow(&mut self) {
        let old = std::mem::take(&mut self.slots);
        let cap = old.len() * 2;
        self.slots = vec![(K::default(), V::default()); cap];
        self.mask = cap - 1;
        self.len = 0;
        for (k, v) in old {
            if !k.is_zero() {
                self.get_or_insert(k, v);
            }
        }
    }
}

/// Read-only K -> K map in 256 open-addressing shards (top 8 hash bits) laid
/// out in one hugepage buffer; shards are filled in parallel.
pub struct GSharded<K: IdKey> {
    slots: crate::alloc::HugeBuf<(K, K)>,
    shard_bits: u32,
    mask: usize,
}

unsafe impl<K: IdKey> Sync for GSharded<K> {}

impl<K: IdKey> GSharded<K> {
    pub const SHARDS: usize = 256;
    #[inline(always)]
    pub fn shard_of(k: K) -> usize {
        (k.hash() >> 56) as usize
    }
    pub fn new(max_per_shard: usize) -> Self {
        let per = (max_per_shard * 2).next_power_of_two().max(16);
        GSharded { slots: crate::alloc::HugeBuf::zeroed(per * Self::SHARDS), shard_bits: per.trailing_zeros(), mask: per - 1 }
    }
    /// # Safety: k belongs to shard s; one writer per shard.
    #[inline(always)]
    pub unsafe fn insert_shard(&self, s: usize, k: K, v: K) {
        let base = self.slots.as_mut_ptr().add(s << self.shard_bits);
        let mut i = k.hash() as usize & self.mask;
        loop {
            let e = &mut *base.add(i);
            if e.0.is_zero() || e.0 == k {
                *e = (k, v);
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn slot(&self, k: K) -> (*const (K, K), usize) {
        let h = k.hash();
        (unsafe { self.slots.as_ptr().add(((h >> 56) as usize) << self.shard_bits) }, h as usize & self.mask)
    }
    #[inline(always)]
    pub fn get(&self, k: K) -> Option<K> {
        let (base, mut i) = self.slot(k);
        loop {
            let e = unsafe { &*base.add(i) };
            if e.0.is_zero() {
                return None;
            }
            if e.0 == k {
                return Some(e.1);
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    pub fn prefetch(&self, k: K) {
        let (base, i) = self.slot(k);
        prefetch(unsafe { base.add(i) } as *const u8);
    }
}

#[inline(always)]
pub fn part_of<K: IdKey>(name: K) -> usize {
    // independent of the shard bits (top 8) and slot bits (low): use bits 40..48
    ((name.hash() >> 40) & 0xFF) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_order_and_roundtrip() {
        let mut v: Vec<Vec<u8>> = vec![b"a".to_vec(), b"A".to_vec(), b"0".to_vec(), b"_".to_vec(), b"z".to_vec(), b"Z".to_vec(), b"9".to_vec(), b"u1".to_vec(), b"U1".to_vec(), b"u10".to_vec(), b"u1a".to_vec(), b"u1_".to_vec(), b"zzzzzzzzzz".to_vec(), b"0000000000".to_vec(), b"e42huhoug".to_vec(), b"u".to_vec(), b"uu".to_vec()];
        let mut seed = 5u64;
        let alpha = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";
        for _ in 0..3000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = 1 + (seed >> 60) as usize % 10;
            let mut s = Vec::new();
            for j in 0..n {
                s.push(alpha[((seed >> (j * 6)) as usize) % alpha.len()]);
            }
            v.push(s);
        }
        for x in &v {
            let kx = <u64 as IdKey>::pack(x, x).unwrap();
            assert_eq!(&*kx.bytes(), &x[..]);
            for y in v.iter().take(200) {
                let ky = <u64 as IdKey>::pack(y, y).unwrap();
                assert_eq!(kx.cmp(&ky), x.cmp(y), "{:?} {:?}", x, y);
            }
        }
        for bad in [&b"u1 "[..], b" u1", b"u-1", b"u1\r", b"\xc3\xbc", b"01234567890", b"a\"b", b"\0"] {
            assert_eq!(<u64 as IdKey>::pack(bad, bad), None, "{:?}", bad);
        }
    }
}
