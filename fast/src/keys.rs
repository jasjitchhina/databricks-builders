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
    /// `(pack(m, a), pack(m, b))`, possibly in one vector pass.
    #[inline(always)]
    fn pack2(m: &[u8], a: &[u8], b: &[u8]) -> (Option<Self>, Option<Self>) {
        (Self::pack(m, a), Self::pack(m, b))
    }
    fn hash(self) -> u64;
    /// Byte-wise `a < b`.
    fn less(a: Self, b: Self) -> bool;
    fn bytes(self) -> Box<[u8]>;
    /// Appends the ID's bytes to `out` (no allocation for compact keys).
    fn append_bytes(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bytes());
    }
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

/// SIMD compact encode of a field of length n <= 10 whose 16 bytes from
/// `p` are readable. Same result as the scalar `pack_scalar`.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn pack_simd(p: *const u8, n: usize) -> Option<u64> {
    use std::arch::x86_64::*;
    let b = _mm_loadu_si128(p as *const __m128i);
    // nibble classification: valid iff CLASS_LO[lo] & CLASS_HI[hi] != 0
    //   bit0: hi 3, lo 0-9 (digits)   bit1: hi 4/6, lo 1-15 (A-O, a-o)
    //   bit2: hi 5/7, lo 0-10 (P-Z, p-z)   bit3: hi 5, lo 15 ('_')
    let lo_n = _mm_and_si128(b, _mm_set1_epi8(0x0F));
    let hi_n = _mm_and_si128(_mm_srli_epi16(b, 4), _mm_set1_epi8(0x0F));
    let class_lo = _mm_setr_epi8(5, 7, 7, 7, 7, 7, 7, 7, 7, 7, 6, 2, 2, 2, 2, 10);
    let class_hi = _mm_setr_epi8(0, 0, 0, 1, 2, 12, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0);
    let cls = _mm_and_si128(_mm_shuffle_epi8(class_lo, lo_n), _mm_shuffle_epi8(class_hi, hi_n));
    let invalid = _mm_cmpeq_epi8(cls, _mm_setzero_si128());
    // code = b - OFF[hi] (digits 47, upper 54, lower 59), '_' (95) -> 37
    let off_hi = _mm_setr_epi8(0, 0, 0, 47, 54, 54, 59, 59, 0, 0, 0, 0, 0, 0, 0, 0);
    let us = _mm_cmpeq_epi8(b, _mm_set1_epi8(b'_' as i8));
    let off = _mm_add_epi8(_mm_shuffle_epi8(off_hi, hi_n), _mm_and_si128(us, _mm_set1_epi8(4)));
    let code = _mm_sub_epi8(b, off);
    let valid = !(_mm_movemask_epi8(invalid) as u32);
    let need = (1u32 << n) - 1;
    if valid & need != need {
        return None;
    }
    // zero lanes >= n
    let idx = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let keep = _mm_cmpgt_epi8(_mm_set1_epi8(n as i8), idx);
    let code = _mm_and_si128(code, keep);
    // pairs: c0*64 + c1 (u16), then quads: (c0c1)*4096 + c2c3 (u32, 24 bits)
    let pairs = _mm_maddubs_epi16(code, _mm_setr_epi8(64, 1, 64, 1, 64, 1, 64, 1, 64, 1, 64, 1, 64, 1, 64, 1));
    let quads = _mm_madd_epi16(pairs, _mm_setr_epi16(4096, 1, 4096, 1, 4096, 1, 4096, 1));
    let l0 = _mm_cvtsi128_si32(quads) as u32 as u64;
    let l1 = _mm_extract_epi32(quads, 1) as u32 as u64;
    let l2 = _mm_extract_epi32(quads, 2) as u32 as u64;
    Some((l0 << 40) | (l1 << 16) | (l2 >> 8))
}

/// Two `pack_simd`s in one 256-bit pass (a in the low lane, b in the high lane).
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn pack2_simd(pa: *const u8, na: usize, pb: *const u8, nb: usize) -> (Option<u64>, Option<u64>) {
    use std::arch::x86_64::*;
    let b = _mm256_set_m128i(_mm_loadu_si128(pb as *const __m128i), _mm_loadu_si128(pa as *const __m128i));
    let lo_n = _mm256_and_si256(b, _mm256_set1_epi8(0x0F));
    let hi_n = _mm256_and_si256(_mm256_srli_epi16(b, 4), _mm256_set1_epi8(0x0F));
    let class_lo = _mm256_broadcastsi128_si256(_mm_setr_epi8(5, 7, 7, 7, 7, 7, 7, 7, 7, 7, 6, 2, 2, 2, 2, 10));
    let class_hi = _mm256_broadcastsi128_si256(_mm_setr_epi8(0, 0, 0, 1, 2, 12, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0));
    let cls = _mm256_and_si256(_mm256_shuffle_epi8(class_lo, lo_n), _mm256_shuffle_epi8(class_hi, hi_n));
    let invalid = _mm256_cmpeq_epi8(cls, _mm256_setzero_si256());
    let off_hi = _mm256_broadcastsi128_si256(_mm_setr_epi8(0, 0, 0, 47, 54, 54, 59, 59, 0, 0, 0, 0, 0, 0, 0, 0));
    let us = _mm256_cmpeq_epi8(b, _mm256_set1_epi8(b'_' as i8));
    let off = _mm256_add_epi8(_mm256_shuffle_epi8(off_hi, hi_n), _mm256_and_si256(us, _mm256_set1_epi8(4)));
    let code = _mm256_sub_epi8(b, off);
    let valid = !(_mm256_movemask_epi8(invalid) as u32);
    let idx = _mm256_broadcastsi128_si256(_mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
    let nv = _mm256_set_m128i(_mm_set1_epi8(nb as i8), _mm_set1_epi8(na as i8));
    let code = _mm256_and_si256(code, _mm256_cmpgt_epi8(nv, idx));
    let pairs = _mm256_maddubs_epi16(code, _mm256_set1_epi16(0x0140));
    let quads = _mm256_madd_epi16(pairs, _mm256_set1_epi32(0x0001_1000));
    let lo = _mm256_castsi256_si128(quads);
    let hi = _mm256_extracti128_si256(quads, 1);
    let k = |q: __m128i| -> u64 {
        let l0 = _mm_cvtsi128_si32(q) as u32 as u64;
        let l1 = _mm_extract_epi32(q, 1) as u32 as u64;
        let l2 = _mm_extract_epi32(q, 2) as u32 as u64;
        (l0 << 40) | (l1 << 16) | (l2 >> 8)
    };
    let need_a = (1u32 << na) - 1;
    let need_b = ((1u32 << nb) - 1) << 16;
    let ra = if valid & need_a == need_a { Some(k(lo)) } else { None };
    let rb = if valid & need_b == need_b { Some(k(hi)) } else { None };
    (ra, rb)
}

#[inline(always)]
fn pack_scalar(f: &[u8]) -> Option<u64> {
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

impl IdKey for u64 {
    const WIDE: bool = false;
    #[inline(always)]
    fn pack(m: &[u8], f: &[u8]) -> Option<Self> {
        let n = f.len();
        if n > 10 {
            return None;
        }
        #[cfg(target_feature = "avx2")]
        {
            let off = f.as_ptr() as usize - m.as_ptr() as usize;
            if off + 16 <= m.len() {
                return unsafe { pack_simd(f.as_ptr(), n) };
            }
        }
        let _ = m;
        pack_scalar(f)
    }
    #[inline(always)]
    fn pack2(m: &[u8], a: &[u8], b: &[u8]) -> (Option<Self>, Option<Self>) {
        #[cfg(target_feature = "avx2")]
        {
            let (na, nb) = (a.len(), b.len());
            let oa = a.as_ptr() as usize - m.as_ptr() as usize;
            let ob = b.as_ptr() as usize - m.as_ptr() as usize;
            if na <= 10 && nb <= 10 && oa + 16 <= m.len() && ob + 16 <= m.len() {
                return unsafe { pack2_simd(a.as_ptr(), na, b.as_ptr(), nb) };
            }
        }
        (Self::pack(m, a), Self::pack(m, b))
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
        self.append_bytes(&mut v);
        v.into()
    }
    #[inline]
    fn append_bytes(self, out: &mut Vec<u8>) {
        for i in 0..10 {
            let code = ((self >> (58 - 6 * i)) & 63) as usize;
            if code == 0 {
                break;
            }
            out.push(DECODE[code]);
        }
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
    /// Empties the map and makes room for `n` keys without growing
    /// (reallocates only if the current table is too small).
    pub fn reset_for(&mut self, n: usize) {
        let cap = (n * 2).next_power_of_two().max(16);
        if self.slots.len() < cap {
            *self = Self::with_capacity(n);
        } else {
            self.clear();
        }
    }
    /// Empties the map, keeping its capacity.
    pub fn clear(&mut self) {
        if self.len > 0 {
            self.slots.fill((K::default(), V::default()));
            self.len = 0;
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
    /// Blocked Bloom filter over the inserted keys (one u64 word per key, 3
    /// bits; idea and numbers from h1 c399cf9). No false negatives, so a
    /// negative answer is exact; ~16 bits/key keeps it cache-resident while
    /// the map itself is DRAM-sized. Most lookups (users that are their own
    /// name) stop here.
    filter: crate::alloc::HugeBuf<u64>,
    fmask: usize,
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
        let words = (max_per_shard * Self::SHARDS * 16 / 64).next_power_of_two().max(8);
        GSharded {
            slots: crate::alloc::HugeBuf::zeroed(per * Self::SHARDS),
            shard_bits: per.trailing_zeros(),
            mask: per - 1,
            filter: crate::alloc::HugeBuf::zeroed(words),
            fmask: words - 1,
        }
    }
    /// Filter word and bits, from a remix of the key hash (independent of the
    /// shard/slot bits).
    #[inline(always)]
    fn fbits(&self, h: u64) -> (usize, u64) {
        // h is already a full 64-bit mix: take the word from bits 14.. (above the slot
        // bits) and the three bit positions from bits 35..52 (below the shard bits 56..63,
        // which are constant within a shard); insert and test use the same function.
        let w = ((h >> 14) as usize) & self.fmask;
        let b = (1u64 << ((h >> 35) & 63)) | (1u64 << ((h >> 41) & 63)) | (1u64 << ((h >> 47) & 63));
        (w, b)
    }
    #[inline(always)]
    fn maybe_h(&self, h: u64) -> bool {
        let (w, b) = self.fbits(h);
        unsafe { *self.filter.as_ptr().add(w) & b == b }
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
                let (w, b) = self.fbits(k.hash());
                let f = &*(self.filter.as_ptr().add(w) as *const std::sync::atomic::AtomicU64);
                f.fetch_or(b, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn slot(&self, k: K) -> (*const (K, K), usize) {
        self.slot_h(k.hash())
    }
    #[inline(always)]
    fn slot_h(&self, h: u64) -> (*const (K, K), usize) {
        (unsafe { self.slots.as_ptr().add(((h >> 56) as usize) << self.shard_bits) }, h as usize & self.mask)
    }
    /// `get` with the key's hash precomputed (h == k.hash()).
    #[inline(always)]
    pub fn get_h(&self, k: K, h: u64) -> Option<K> {
        if !self.maybe_h(h) {
            return None;
        }
        let (base, mut i) = self.slot_h(h);
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
    /// Pipeline helpers that compute the filter bits once per key:
    /// `fbits_h` -> (word, bits); `filter_prefetch_w`; `filter_test_w`;
    /// `slot_prefetch_h`; `get_slot_h` (no filter re-check).
    #[inline(always)]
    pub fn fbits_h(&self, h: u64) -> (usize, u64) {
        self.fbits(h)
    }
    #[inline(always)]
    pub fn filter_prefetch_w(&self, w: usize) {
        prefetch(unsafe { self.filter.as_ptr().add(w) } as *const u8);
    }
    #[inline(always)]
    pub fn filter_test_w(&self, w: usize, b: u64) -> bool {
        unsafe { *self.filter.as_ptr().add(w) & b == b }
    }
    #[inline(always)]
    pub fn slot_prefetch_h(&self, h: u64) {
        let (base, i) = self.slot_h(h);
        prefetch(unsafe { base.add(i) } as *const u8);
    }
    #[inline(always)]
    pub fn get_slot_h(&self, k: K, h: u64) -> Option<K> {
        let (base, mut i) = self.slot_h(h);
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
    pub fn prefetch_filter_h(&self, h: u64) {
        let (w, _) = self.fbits(h);
        prefetch(unsafe { self.filter.as_ptr().add(w) } as *const u8);
    }
    #[inline(always)]
    pub fn prefetch_if_h(&self, h: u64) {
        if self.maybe_h(h) {
            let (base, i) = self.slot_h(h);
            prefetch(unsafe { base.add(i) } as *const u8);
        }
    }
    #[inline(always)]
    pub fn get(&self, k: K) -> Option<K> {
        if !self.maybe_h(k.hash()) {
            return None;
        }
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
    /// Pipeline stage 1: bring the filter word in.
    #[inline(always)]
    pub fn prefetch_filter(&self, k: K) {
        let (w, _) = self.fbits(k.hash());
        prefetch(unsafe { self.filter.as_ptr().add(w) } as *const u8);
    }
    /// Pipeline stage 2: prefetch the slot only if the key may be present.
    #[inline(always)]
    pub fn prefetch_if(&self, k: K) {
        if self.maybe_h(k.hash()) {
            self.prefetch(k);
        }
    }
}

#[inline(always)]
pub fn part_of<K: IdKey>(name: K) -> usize {
    part_of_h(name.hash())
}
/// `part_of` from the name's hash (independent of the shard bits (top 8) and slot bits (low)).
#[inline(always)]
pub fn part_of_h(h: u64) -> usize {
    ((h >> 40) & 0xFF) as usize
}

#[cfg(test)]
mod tests {
    #[test]
    fn pack2_matches_pack() {
        let alpha = b"09AZaz_-.!/:Mm5Qq\x00\xff \"";
        let mut st = 0x1234_5678_9abc_def0u64;
        let mut rnd = || { st ^= st << 13; st ^= st >> 7; st ^= st << 17; st };
        let mut buf = vec![0u8; 64];
        for _ in 0..300_000 {
            for b in buf.iter_mut() {
                let r = rnd();
                *b = if r % 5 == 0 { alpha[(r >> 8) as usize % alpha.len()] } else { b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz"[(r >> 8) as usize % 63] };
            }
            let r = rnd();
            let (sa, la) = ((r % 20) as usize, ((r >> 8) % 13) as usize);
            let (sb, lb) = (((r >> 16) % 40) as usize, ((r >> 24) % 13) as usize);
            let m = &buf[..];
            let (a, b) = (&m[sa..sa + la], &m[sb..sb + lb]);
            assert_eq!(<u64 as IdKey>::pack2(m, a, b), (<u64 as IdKey>::pack(m, a), <u64 as IdKey>::pack(m, b)), "{:?} {:?}", a, b);
            let (a2, b2) = (&m[m.len() - la..], &m[m.len() - lb..]);
            assert_eq!(<u64 as IdKey>::pack2(m, a2, b2), (<u64 as IdKey>::pack(m, a2), <u64 as IdKey>::pack(m, b2)));
        }
    }

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
        // SIMD path (field inside a padded buffer) == scalar path, valid or not
        let alpha2 = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz /:@[`{\x7f\xff\x00-";
        for _ in 0..200000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = (seed >> 59) as usize % 12;
            let mut buf = vec![b'q'; 40];
            for j in 0..n {
                let r = (seed >> (j * 5 % 50)) as usize;
                buf[3 + j] = if (seed >> 60) & 3 == 0 { alpha2[r % alpha2.len()] } else { alpha[r % alpha.len()] };
            }
            let f = &buf[3..3 + n];
            assert_eq!(<u64 as IdKey>::pack(&buf, f), pack_scalar(f), "{:?}", f);
        }
        for bad in [&b"u1 "[..], b" u1", b"u-1", b"u1\r", b"\xc3\xbc", b"01234567890", b"a\"b", b"\0"] {
            assert_eq!(<u64 as IdKey>::pack(bad, bad), None, "{:?}", bad);
        }
    }
}
