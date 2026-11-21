//! Parse + clean + dedup + link: DATA_DIR -> model::Cleaned.

use crate::csv::{self, Chunk};
use crate::ids::{self, hk, key_bytes, key_cmp, pack, Key, KeyMap, NPART};
use crate::model::{Cl, Cleaned, Ev, Part};
use crate::ts::{parse_ts, DAY_MS};
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const NB_BITS: u32 = 10;
const NB: usize = 1 << NB_BITS; // dedup buckets
const CHUNK: usize = 4 << 20;
const MAX_PRICE: u64 = 10_000_000;

/// Runs `f(state, item)` for items 0..n on `threads` threads, each with its own
/// state from `init`; returns the states.
pub fn par_items<S: Send, I: Fn() -> S + Sync, F: Fn(&mut S, usize) + Sync>(
    threads: usize,
    n: usize,
    init: I,
    f: F,
) -> Vec<S> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut s = init();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        f(&mut s, i);
                    }
                    s
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn timer() -> impl FnMut(&str) {
    let t0 = Instant::now();
    let verbose = std::env::var_os("FAST_VERBOSE").is_some();
    let mut last = t0;
    move |what: &str| {
        if verbose {
            let now = Instant::now();
            eprintln!(
                "[clean] {:<14} {:>8.1} ms  (total {:>8.1} ms)",
                what,
                (now - last).as_secs_f64() * 1e3,
                (now - t0).as_secs_f64() * 1e3
            );
            last = now;
        }
    }
}

fn read_manifest(p: &Path) -> (i64, i64) {
    let s = std::fs::read(p).expect("manifest.json");
    let get = |k: &[u8]| -> i64 {
        let at = memchr::memmem::find(&s, k).expect("manifest key") + k.len();
        let rest = &s[at..];
        let q1 = memchr::memchr(b'"', rest).unwrap() + 1;
        let q2 = q1 + memchr::memchr(b'"', &rest[q1..]).unwrap();
        parse_ts(&rest[q1..q2]).expect("manifest ts")
    };
    (get(b"\"window_start\""), get(b"\"window_end\""))
}

fn data_lines(p: &Path, mut f: impl FnMut(&[u8])) {
    let b = std::fs::read(p).unwrap_or_default();
    if let Some(h) = memchr::memchr(b'\n', &b) {
        csv::for_lines(&b[h + 1..], &mut f);
    }
}

#[derive(Clone, Copy)]
struct ERec {
    eid: Key,
    user: Key,
    ingest: i64,
    ts: u32,
    price: u32,
    cat: u16,
    ok: bool,
}

#[derive(Clone, Copy)]
struct CRec {
    cid: Key,
    person: Key,
    ts: u32,
    camp: u16,
}

#[derive(Clone, Copy)]
struct PRec {
    person: Key,
    ts: u32,
    price: u32,
    cat: u16,
}

struct Links {
    /// id -> name, only for ids whose name is not themselves.
    map: KeyMap,
}

impl Links {
    #[inline(always)]
    fn person(&self, user: Key) -> Key {
        self.map.get(user).unwrap_or(user)
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let p = parent[x as usize];
        let gp = parent[p as usize];
        parent[x as usize] = gp;
        x = gp;
    }
    x
}

fn build_links(dir: &Path, threads: usize) -> Links {
    let files = csv::list_csv(&dir.join("links"));
    let maps: Vec<Option<Mmap>> = files.iter().map(|p| csv::map(p)).collect();
    let chunks = csv::chunks(&maps, 1 << 20);
    let per = par_items(threads, chunks.len(), Vec::<(Key, Key)>::new, |out, i| {
        let c = chunks[i];
        let b = &maps[c.file].as_ref().unwrap()[c.start..c.end];
        csv::for_lines(b, |line| {
            if let Some([a, bb]) = csv::split::<2>(line) {
                if !a.is_empty() && !bb.is_empty() && a != bb {
                    out.push((pack(a), pack(bb)));
                }
            }
        });
    });
    let n: usize = per.iter().map(|v| v.len()).sum();
    // intern
    let mut idx = KeyMap::with_capacity(n * 2);
    let mut keys: Vec<Key> = Vec::with_capacity(n * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(n * 2);
    let mut intern = |k: Key, keys: &mut Vec<Key>, parent: &mut Vec<u32>| -> u32 {
        match idx.get(k) {
            Some(v) => v as u32,
            None => {
                let v = keys.len() as u32;
                keys.push(k);
                parent.push(v);
                idx.insert(k, v as Key);
                v
            }
        }
    };
    for v in &per {
        for &(a, b) in v {
            let ia = intern(a, &mut keys, &mut parent);
            let ib = intern(b, &mut keys, &mut parent);
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                // keep the smaller index as root (arbitrary but deterministic)
                if ra < rb {
                    parent[rb as usize] = ra;
                } else {
                    parent[ra as usize] = rb;
                }
            }
        }
    }
    let m = keys.len();
    let mut name: Vec<Key> = vec![0; m];
    let mut roots: Vec<u32> = vec![0; m];
    for i in 0..m {
        let r = find(&mut parent, i as u32);
        roots[i] = r;
        let k = keys[i];
        let cur = name[r as usize];
        if cur == 0 || key_cmp(k, cur) == std::cmp::Ordering::Less {
            name[r as usize] = k;
        }
    }
    let differing = (0..m).filter(|&i| name[roots[i] as usize] != keys[i]).count();
    let mut map = KeyMap::with_capacity(differing);
    for i in 0..m {
        let nm = name[roots[i] as usize];
        if nm != keys[i] {
            map.insert(keys[i], nm);
        }
    }
    Links { map }
}

struct Lookups {
    products: KeyMap, // product key -> cat idx
    unknown_cat: u16,
    campaigns: KeyMap, // campaign key -> idx
}

struct ParseState {
    ev: Vec<Vec<ERec>>,
    cl: Vec<Vec<CRec>>,
}

pub fn clean(dir: &Path, threads: usize) -> Cleaned {
    let mut t = timer();
    let (ws, we) = read_manifest(&dir.join("manifest.json"));
    let base = ws - 7 * DAY_MS;

    // products / campaigns
    let mut cat_names: Vec<Box<[u8]>> = Vec::new();
    let mut cat_idx: HashMap<Vec<u8>, u16> = HashMap::new();
    let mut prod_rows: Vec<(Key, u16)> = Vec::new();
    data_lines(&dir.join("products.csv"), |line| {
        if let Some([p, c]) = csv::split::<2>(line) {
            let ci = *cat_idx.entry(c.to_vec()).or_insert_with(|| {
                cat_names.push(c.into());
                (cat_names.len() - 1) as u16
            });
            prod_rows.push((pack(p), ci));
        }
    });
    let unknown_cat = cat_names.len() as u16;
    cat_names.push(b"unknown"[..].into());
    let mut products = KeyMap::with_capacity(prod_rows.len());
    for &(k, c) in &prod_rows {
        products.insert(k, c as Key);
    }
    let mut camp_ids: Vec<Box<[u8]>> = Vec::new();
    let mut camp_channels: Vec<Box<[u8]>> = Vec::new();
    data_lines(&dir.join("campaigns.csv"), |line| {
        if let Some([c, ch]) = csv::split::<2>(line) {
            camp_ids.push(c.into());
            camp_channels.push(ch.into());
        }
    });
    let mut campaigns = KeyMap::with_capacity(camp_ids.len());
    for (i, c) in camp_ids.iter().enumerate() {
        campaigns.insert(pack(c), i as Key);
    }
    let lk = Lookups { products, unknown_cat, campaigns };
    t("small files");

    let links = build_links(dir, threads);
    t("links");

    // events + clicks
    let ev_files = csv::list_csv(&dir.join("events"));
    let cl_files = csv::list_csv(&dir.join("clicks"));
    let n_ev_files = ev_files.len();
    let maps: Vec<Option<Mmap>> = ev_files.iter().chain(cl_files.iter()).map(|p| csv::map(p)).collect();
    let chunks: Vec<Chunk> = csv::chunks(&maps, CHUNK);
    let ev_bytes: usize = maps[..n_ev_files].iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let cl_bytes: usize = maps[n_ev_files..].iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let ev_cap = ev_bytes / 70 / NB / threads + 16;
    let cl_cap = cl_bytes / 45 / NPART / threads + 16;
    let long_click = AtomicBool::new(false);
    t("map+chunk");

    let cl_lo = base;
    let states = par_items(
        threads,
        chunks.len(),
        || ParseState {
            ev: (0..NB).map(|_| Vec::with_capacity(ev_cap)).collect(),
            cl: (0..NPART).map(|_| Vec::with_capacity(cl_cap)).collect(),
        },
        |st, i| {
            let c = chunks[i];
            let b = &maps[c.file].as_ref().unwrap()[c.start..c.end];
            if c.file < n_ev_files {
                csv::for_lines(b, |line| parse_event(line, ws, we, base, &lk, st));
            } else {
                csv::for_lines(b, |line| {
                    let Some([cid, user, camp, ts]) = csv::split::<4>(line) else { return };
                    if cid.is_empty() || user.is_empty() {
                        return;
                    }
                    let Some(t) = parse_ts(ts) else { return };
                    if t < cl_lo || t >= we {
                        return;
                    }
                    let Some(ci) = lk.campaigns.get(pack(camp)) else { return };
                    let ck = pack(cid);
                    if ids::is_long(ck) {
                        long_click.store(true, Ordering::Relaxed);
                    }
                    let person = links.person(pack(user));
                    st.cl[ids::part_of(person)].push(CRec { cid: ck, person, ts: (t - base) as u32, camp: ci as u16 });
                });
            }
        },
    );
    t("parse");

    // dedup by event_id, per bucket
    let states = &states;
    let deduped = par_items(
        threads,
        NB,
        || (Vec::<(Key, u64)>::new(), (0..NPART).map(|_| Vec::<PRec>::new()).collect::<Vec<_>>()),
        |(table, out), b| {
            let n: usize = states.iter().map(|s| s.ev[b].len()).sum();
            if n == 0 {
                return;
            }
            let cap = (n * 2).next_power_of_two();
            table.clear();
            table.resize(cap, (0, u64::MAX));
            let mask = cap - 1;
            for (ti, s) in states.iter().enumerate() {
                for (ri, r) in s.ev[b].iter().enumerate() {
                    let mut h = (hk(r.eid) >> NB_BITS) as usize & mask;
                    loop {
                        let slot = &mut table[h];
                        if slot.1 == u64::MAX {
                            *slot = (r.eid, ((ti as u64) << 32) | ri as u64);
                            break;
                        }
                        if slot.0 == r.eid {
                            let o = slot.1;
                            let old = &states[(o >> 32) as usize].ev[b][(o & 0xFFFF_FFFF) as usize];
                            if r.ingest < old.ingest {
                                slot.1 = ((ti as u64) << 32) | ri as u64;
                            }
                            break;
                        }
                        h = (h + 1) & mask;
                    }
                }
            }
            for &(_, o) in table.iter() {
                if o == u64::MAX {
                    continue;
                }
                let r = &states[(o >> 32) as usize].ev[b][(o & 0xFFFF_FFFF) as usize];
                if !r.ok {
                    continue;
                }
                let person = links.person(r.user);
                out[ids::part_of(person)].push(PRec { person, ts: r.ts, price: r.price, cat: r.cat });
            }
        },
    );
    t("dedup");

    // per part: dense person ids, clicks
    let deduped = &deduped;
    let mut parts_out: Vec<Mutex<Option<Part>>> = (0..NPART).map(|_| Mutex::new(None)).collect();
    {
        let parts_ref = &parts_out;
        par_items(threads, NPART, || (), |_, p| {
            let n: usize = deduped.iter().map(|(_, o)| o[p].len()).sum();
            let mut local = KeyMap::with_capacity(n / 4 + 16);
            let mut names: Vec<Key> = Vec::new();
            let mut ev: Vec<Ev> = Vec::with_capacity(n);
            for (_, o) in deduped.iter() {
                for r in &o[p] {
                    let id = match local.get(r.person) {
                        Some(v) => v as u32,
                        None => {
                            let v = names.len() as u32;
                            names.push(r.person);
                            local.insert(r.person, v as Key);
                            v
                        }
                    };
                    ev.push(Ev { person: id, ts: r.ts, price: r.price, cat: r.cat });
                }
            }
            let mut cl: Vec<Cl> = Vec::new();
            for s in states.iter() {
                for r in &s.cl[p] {
                    if let Some(v) = local.get(r.person) {
                        cl.push(Cl { click_id: r.cid, person: v as u32, ts: r.ts, camp: r.camp });
                    }
                }
            }
            let person_names = names.into_iter().map(key_bytes).collect();
            *parts_ref[p].lock().unwrap() = Some(Part { person_names, ev, cl });
        });
    }
    let mut parts: Vec<Part> = parts_out.iter_mut().map(|m| m.get_mut().unwrap().take().unwrap()).collect();
    t("parts");

    if long_click.load(Ordering::Relaxed) {
        rank_click_ids(&mut parts);
        t("rank clicks");
    }

    Cleaned { base_ms: base, ws_ms: ws, we_ms: we, cat_names, unknown_cat, camp_ids, camp_channels, parts }
}

/// Some click_id is longer than 15 bytes: replace every click_id key by its
/// byte-wise rank so plain u128 comparison stays exact.
#[cold]
fn rank_click_ids(parts: &mut [Part]) {
    let mut all: Vec<Key> = parts.iter().flat_map(|p| p.cl.iter().map(|c| c.click_id)).collect();
    all.sort_unstable_by(|a, b| key_cmp(*a, *b));
    all.dedup();
    let mut rank = KeyMap::with_capacity(all.len());
    for (i, &k) in all.iter().enumerate() {
        rank.insert(k, i as Key + 1);
    }
    for p in parts.iter_mut() {
        for c in p.cl.iter_mut() {
            c.click_id = rank.get(c.click_id).unwrap();
        }
    }
}

#[inline(always)]
fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v * 10 + d as u64;
        if v > MAX_PRICE {
            // leading zeros keep v small, so this only trips on a real overflow of the limit
            return None;
        }
    }
    if v == 0 {
        None
    } else {
        Some(v as u32)
    }
}

#[inline(always)]
fn parse_event(line: &[u8], ws: i64, we: i64, base: i64, lk: &Lookups, st: &mut ParseState) {
    let Some([eid, user, ts, ing, typ, prod, price]) = csv::split::<7>(line) else { return };
    if eid.is_empty() {
        return;
    }
    let ingest = parse_ts(ing).unwrap_or(i64::MAX);
    let ek = pack(eid);
    let mut rec = ERec { eid: ek, user: 0, ingest, ts: 0, price: 0, cat: 0, ok: false };
    'v: {
        if user.is_empty() || prod.is_empty() {
            break 'v;
        }
        let Some(t) = parse_ts(ts) else { break 'v };
        if t < ws || t >= we {
            break 'v;
        }
        if ingest == i64::MAX || ingest - t > DAY_MS {
            break 'v;
        }
        match typ {
            b"view" | b"cart" => {}
            b"purchase" => {
                let Some(p) = parse_price(price) else { break 'v };
                rec.price = p;
                rec.cat = lk.products.get(pack(prod)).map_or(lk.unknown_cat, |c| c as u16);
            }
            _ => break 'v,
        }
        rec.user = pack(user);
        rec.ts = (t - base) as u32;
        rec.ok = true;
    }
    st.ev[(hk(ek) & (NB as u64 - 1)) as usize].push(rec);
}
