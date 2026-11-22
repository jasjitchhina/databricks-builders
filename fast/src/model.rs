//! The h3 -> h4 interface: cleaned, deduped, validated, linked rows,
//! partitioned by person.

pub use crate::ids::NPART;

pub struct Cleaned {
    /// window_start - 7 days; every `ts` below is `ts_ms - base_ms` (21 days fits u32).
    pub base_ms: i64,
    pub ws_ms: i64,
    pub we_ms: i64,
    /// category index -> name; includes `unknown` (index `unknown_cat`).
    pub cat_names: Vec<Box<[u8]>>,
    pub unknown_cat: u32,
    /// campaigns.csv order; `organic` is NOT included.
    pub camp_ids: Vec<Box<[u8]>>,
    pub camp_channels: Vec<Box<[u8]>>,
    /// NPART parts; every person lives in exactly one part.
    pub parts: Vec<Part>,
}

/// Rows of a part: an owned Vec, or a slice in memory the clean stage
/// recycled from its dead intermediates (no fresh page faults). Read it as a
/// slice (Deref); `push` only works on the owned form.
pub enum Rows<T: 'static> {
    Owned(Vec<T>),
    Static(&'static mut [T]),
}

impl<T> Default for Rows<T> {
    fn default() -> Self {
        Rows::Owned(Vec::new())
    }
}

impl<T> std::ops::Deref for Rows<T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        match self {
            Rows::Owned(v) => v,
            Rows::Static(s) => s,
        }
    }
}

impl<T> std::ops::DerefMut for Rows<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Rows::Owned(v) => v,
            Rows::Static(s) => s,
        }
    }
}

impl<T> Rows<T> {
    pub fn push(&mut self, x: T) {
        match self {
            Rows::Owned(v) => v.push(x),
            Rows::Static(_) => panic!("push on static rows"),
        }
    }
}

/// Person names of a part in one buffer (one allocation instead of one per
/// person). `names[i]` is the i-th name's bytes.
#[derive(Default)]
pub struct Names {
    buf: Vec<u8>,
    end: Vec<u32>,
}

impl Names {
    pub fn with_capacity(n: usize, bytes: usize) -> Self {
        Names { buf: Vec::with_capacity(bytes), end: Vec::with_capacity(n) }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.end.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.end.is_empty()
    }
    pub fn push(&mut self, name: impl AsRef<[u8]>) {
        self.buf.extend_from_slice(name.as_ref());
        self.end.push(self.buf.len() as u32);
    }
    /// Appends a name written by `f` into the buffer.
    #[inline]
    pub fn push_with(&mut self, f: impl FnOnce(&mut Vec<u8>)) {
        f(&mut self.buf);
        self.end.push(self.buf.len() as u32);
    }
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).map(move |i| &self[i])
    }
}

impl std::ops::Index<usize> for Names {
    type Output = [u8];
    #[inline]
    fn index(&self, i: usize) -> &[u8] {
        let s = if i == 0 { 0 } else { self.end[i - 1] as usize };
        &self.buf[s..self.end[i] as usize]
    }
}

#[derive(Default)]
pub struct Part {
    /// part-local person index -> person name (byte-wise smallest linked ID).
    pub person_names: Names,
    /// surviving events (after dedup, validation, late drop), unsorted.
    pub ev: Rows<Ev>,
    /// valid clicks (not deduped: copies are identical), unsorted. Only clicks
    /// of persons that have at least one surviving event are included.
    pub cl: Rows<Cl>,
    /// rows per part-local person (len == person_names.len()) when the producer
    /// counted them while writing `ev` / `cl`; empty = the engine counts itself.
    /// Saves the engine group a full re-read of the part (memory-bound at 60M).
    pub ev_cnt: Vec<u32>,
    pub cl_cnt: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev {
    pub person: u32,
    pub ts: u32,
    /// price in cents for a purchase (always >= 1); 0 for view/cart.
    pub price: u32,
    /// category index; only meaningful for purchases (0 otherwise).
    pub cat: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cl {
    /// order-preserving key of click_id: plain u128 comparison == byte-wise
    /// comparison of the click IDs; equal iff same click_id.
    pub click_id: u128,
    pub person: u32,
    pub ts: u32,
    pub camp: u32,
}

// The u32 category / campaign indices must not grow the hot row types.
const _: () = assert!(std::mem::size_of::<Ev>() == 16);
const _: () = assert!(std::mem::size_of::<Cl>() == 32);
