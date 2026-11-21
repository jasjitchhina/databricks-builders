//! In-memory interface between the front half (parse/clean/link, h3) and the
//! back half (sessions/attribution/aggregation/parquet, h4). Agreed in team.db notes 1,5.
//!
//! All timestamps are `u32` milliseconds relative to `base_ms = window_start - 7 days`.

pub const DAY_MS: i64 = 86_400_000;
pub const WEEK_MS: u32 = 604_800_000;
pub const GAP_MS: u32 = 1_800_000;
pub const NPART: usize = 256;

pub struct Cleaned {
    pub base_ms: i64,
    pub ws_ms: i64,
    pub we_ms: i64,
    /// category idx -> name, including "unknown"
    pub cat_names: Vec<Box<[u8]>>,
    /// campaigns.csv order; `organic` NOT included
    pub camp_ids: Vec<Box<[u8]>>,
    pub camp_channels: Vec<Box<[u8]>>,
    /// person-hash partitioned: a person lives in exactly one part
    pub parts: Vec<Part>,
}

#[derive(Default)]
pub struct Part {
    /// part-local person idx -> name (byte-wise smallest ID)
    pub person_names: Vec<Box<[u8]>>,
    /// surviving events (deduped, validated, late dropped), unsorted
    pub ev: Vec<Ev>,
    /// valid clicks, unsorted, NOT necessarily deduped (copies are identical)
    pub cl: Vec<Cl>,
}

#[derive(Clone, Copy, Debug)]
pub struct Ev {
    pub person: u32,
    pub ts: u32,
    /// price_cents for purchases (>=1), 0 for view/cart
    pub price: u32,
    pub cat: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Cl {
    /// order-preserving packed click_id: plain u128 compare == byte-wise compare
    pub click_id: u128,
    pub person: u32,
    pub ts: u32,
    pub camp: u16,
}
