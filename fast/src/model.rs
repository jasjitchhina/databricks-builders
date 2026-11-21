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

#[derive(Default)]
pub struct Part {
    /// part-local person index -> person name (byte-wise smallest linked ID).
    pub person_names: Vec<Box<[u8]>>,
    /// surviving events (after dedup, validation, late drop), unsorted.
    pub ev: Vec<Ev>,
    /// valid clicks (not deduped: copies are identical), unsorted. Only clicks
    /// of persons that have at least one surviving event are included.
    pub cl: Vec<Cl>,
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
