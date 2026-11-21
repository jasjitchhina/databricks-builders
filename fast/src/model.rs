//! In-memory interface between the front half (parse/clean/link, h3) and the
//! back half (sessions/attribution/aggregation/parquet, h4).
//!
//! All timestamps are `u32` milliseconds relative to `base_ms = window_start - 7 days`.
//! The click window is [base, window_end) (21 days for a 14-day window, fits u32);
//! the event window is [window_start, window_end).

pub const DAY_MS: i64 = 86_400_000;
pub const WEEK_MS: u32 = 604_800_000;
pub const GAP_MS: u32 = 1_800_000;

#[derive(Default)]
pub struct Cleaned {
    pub base_ms: i64,
    pub ws_ms: i64,
    pub we_ms: i64,

    /// person idx -> name (byte-wise smallest of the person's IDs)
    pub person_names: Vec<Box<[u8]>>,

    /// Surviving events (after dedup, validation, late drop). Any order.
    pub ev_person: Vec<u32>,
    pub ev_ts: Vec<u32>,
    /// price_cents for purchases (>= 1), 0 for view/cart. purchase <=> price > 0.
    pub ev_price: Vec<u32>,
    /// category idx into `cat_names` (only meaningful for purchases).
    pub ev_cat: Vec<u16>,
    /// category idx -> name. Must contain "unknown" if any purchase uses it.
    pub cat_names: Vec<Box<[u8]>>,

    /// Valid deduplicated clicks. Any order.
    pub cl_person: Vec<u32>,
    pub cl_ts: Vec<u32>,
    pub cl_camp: Vec<u16>,
    /// click_id bytes, only used to break ts ties.
    pub cl_id: Vec<Box<[u8]>>,

    /// campaigns.csv, in file order. `organic` is NOT included (back half adds it).
    pub camp_ids: Vec<Box<[u8]>>,
    pub camp_channels: Vec<Box<[u8]>>,
}
