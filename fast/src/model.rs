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
    pub unknown_cat: u16,
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

#[derive(Default)]
pub struct Part {
    /// part-local person index -> person name (byte-wise smallest linked ID).
    pub person_names: Vec<Box<[u8]>>,
    /// surviving events (after dedup, validation, late drop), unsorted.
    pub ev: Rows<Ev>,
    /// valid clicks (not deduped: copies are identical), unsorted. Only clicks
    /// of persons that have at least one surviving event are included.
    pub cl: Rows<Cl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev {
    pub person: u32,
    pub ts: u32,
    /// price in cents for a purchase (always >= 1); 0 for view/cart.
    pub price: u32,
    /// category index; only meaningful for purchases (0 otherwise).
    pub cat: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cl {
    /// order-preserving key of click_id: plain u128 comparison == byte-wise
    /// comparison of the click IDs; equal iff same click_id.
    pub click_id: u128,
    pub person: u32,
    pub ts: u32,
    pub camp: u16,
}
