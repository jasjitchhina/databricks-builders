//! Parse + clean + dedup + link: DATA_DIR -> model::Cleaned.

use crate::csv::{self, pack_in, Chunk};
use crate::alloc::WcRegions;
use crate::ids::{self, hk, key_bytes, key_cmp, pack, prefetch, Key, KeyMap, ShardedMap, NPART};
use crate::model::{Cl, Cleaned, Ev, Part};
use crate::ts::{parse_ts, DAY_MS};
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static EV_NS: AtomicUsize = AtomicUsize::new(0);
static CL_NS: AtomicUsize = AtomicUsize::new(0);
struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
const NB_BITS: u32 = 8;
const NB: usize = 1 << NB_BITS; // dedup buckets
const CHUNK: usize = 2 << 20;
const MAX_PRICE: u64 = 10_000_000;
const NOT_OK: u32 = u32::MAX;
const PF: usize = 12; // prefetch distance

/// Runs `f(state, item)` for items 0..n on `threads` threads, each with its own
/// state from `init`; returns the states.
pub fn par_items<S: Send, I: Fn(usize) -> S + Sync, F: Fn(&mut S, usize) + Sync>(
    threads: usize,
    n: usize,
    init: I,
    f: F,
) -> Vec<S> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let (init, f, next) = (&init, &f, &next);
                sc.spawn(move || {
                    let mut s = init(t);
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

/// A parsed, well-formed event line with a non-empty event_id.
#[derive(Clone, Copy)]
struct ERec {
    eid: Key,
    user: Key,
    ingest: i64,
    /// ts - base, or NOT_OK if the copy fails validation / is late.
    ts: u32,
    /// 0 for view/cart; else 1 + index into the thread's purchase side table.
    pc: u32,
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

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let p = parent[x as usize];
        let gp = parent[p as usize];
        parent[x as usize] = gp;
        x = gp;
    }
    x
}

const LS: usize = 256; // link shards (== ShardedMap shards)

/// Union-find over all link lines; returns id -> name for every linked id
/// whose name is not itself.
fn build_links(dir: &Path, threads: usize) -> ShardedMap {
    let mut t = timer();
    let files = csv::list_csv(&dir.join("links"));
    let maps: Vec<Option<Mmap>> = files.iter().map(|p| csv::map(p)).collect();
    let chunks = csv::chunks(&maps, 1 << 20);
    // per thread: endpoints (key) in order: pair j = (2j, 2j+1); and shard buckets of (key, endpoint ref)
    struct LS_ {
        n_ep: u32,
        t: u32,
        shards: Vec<Vec<(Key, u32)>>,
    }
    let per = par_items(
        threads,
        chunks.len(),
        |t| LS_ { n_ep: 0, t: t as u32, shards: (0..LS).map(|_| Vec::new()).collect() },
        |st, i| {
            let c = chunks[i];
            let m: &[u8] = maps[c.file].as_ref().unwrap();
            csv::scan_fast::<2, _>(m, c.start, c.end, |[a, b]| {
                if !a.is_empty() && !b.is_empty() && a != b {
                    for k in [pack_in(m, a), pack_in(m, b)] {
                        let r = (st.t << 27) | st.n_ep;
                        st.n_ep += 1;
                        st.shards[ShardedMap::shard_of(k)].push((k, r));
                    }
                }
            });
        },
    );
    t(" L parse");
    assert!(per.iter().all(|p| p.n_ep < (1 << 27)));
    // endpoint -> gid, written by shard tasks
    let ep: Vec<Vec<AtomicU32>> = (0..threads)
        .map(|t| {
            let n = per.iter().find(|p| p.t as usize == t).map_or(0, |p| p.n_ep) as usize;
            (0..n).map(|_| AtomicU32::new(0)).collect()
        })
        .collect();
    let per_by_t: Vec<&LS_> = {
        let mut v: Vec<&LS_> = per.iter().collect();
        v.sort_by_key(|p| p.t);
        v
    };
    // per shard: intern -> local ids, keys
    let shard_keys: Vec<Mutex<Vec<Key>>> = (0..LS).map(|_| Mutex::new(Vec::new())).collect();
    par_items(threads, LS, |_| (), |_, s| {
        let n: usize = per_by_t.iter().map(|p| p.shards[s].len()).sum();
        let mut tab = KeyMap::with_capacity(n);
        let mut keys = Vec::with_capacity(n / 2 + 1);
        for p in &per_by_t {
            for &(k, r) in &p.shards[s] {
                let local = match tab.get(k) {
                    Some(v) => v as u32,
                    None => {
                        let v = keys.len() as u32;
                        keys.push(k);
                        tab.insert(k, v as Key);
                        v
                    }
                };
                ep[(r >> 27) as usize][(r & ((1 << 27) - 1)) as usize].store(((s as u32) << 24) | local, Ordering::Relaxed);
            }
        }
        assert!(keys.len() < (1 << 24));
        *shard_keys[s].lock().unwrap() = keys;
    });
    let shard_keys: Vec<Vec<Key>> = shard_keys.into_iter().map(|m| m.into_inner().unwrap()).collect();
    t(" L intern");
    let mut base = vec![0u32; LS + 1];
    for s in 0..LS {
        base[s + 1] = base[s] + shard_keys[s].len() as u32;
    }
    let m = base[LS] as usize;
    let gid = |x: u32| base[(x >> 24) as usize] + (x & 0xFF_FFFF);
    let mut keys: Vec<Key> = Vec::with_capacity(m);
    for sk in &shard_keys {
        keys.extend_from_slice(sk);
    }
    // Union by key order: the root of every tree is its byte-wise smallest
    // key, i.e. the person's name.
    let mut parent: Vec<u32> = (0..m as u32).collect();
    for e in &ep {
        for pair in e.chunks_exact(2) {
            let a = gid(pair[0].load(Ordering::Relaxed));
            let b = gid(pair[1].load(Ordering::Relaxed));
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra != rb {
                if key_cmp(keys[ra as usize], keys[rb as usize]) == std::cmp::Ordering::Less {
                    parent[rb as usize] = ra;
                } else {
                    parent[ra as usize] = rb;
                }
            }
        }
    }
    t(" L unionfind");
    let parent = &parent;
    let keys = &keys;
    let lists: Vec<Mutex<Vec<(Key, Key)>>> = (0..LS).map(|_| Mutex::new(Vec::new())).collect();
    par_items(threads, LS, |_| (), |_, s| {
        let mut v = Vec::new();
        for i in base[s] as usize..base[s + 1] as usize {
            let mut r = i as u32;
            while parent[r as usize] != r {
                r = parent[r as usize];
            }
            if r as usize != i {
                v.push((keys[i], keys[r as usize]));
            }
        }
        *lists[s].lock().unwrap() = v;
    });
    let lists: Vec<Vec<(Key, Key)>> = lists.into_iter().map(|m| m.into_inner().unwrap()).collect();
    t(" L names");
    let map = ShardedMap::new(lists.iter().map(|v| v.len()).max().unwrap_or(0));
    par_items(threads, LS, |_| (), |_, s| {
        for &(k, nm) in &lists[s] {
            unsafe { map.insert_shard(s, k, nm) };
        }
    });
    t(" L map");
    map
}

struct Lookups {
    products: KeyMap, // product key -> cat idx
    unknown_cat: u16,
    campaigns: KeyMap, // campaign key -> idx
}

struct ParseState {
    ev: WcRegions<ERec, 4>,
    side: Vec<(u32, u16)>,
    cl: WcRegions<CRec, 4>,
    cbuf: Vec<CRec>,
}

#[inline(always)]
fn person_of(links: &ShardedMap, user: Key) -> Key {
    links.get(user).unwrap_or(user)
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
    let mut chunks: Vec<Chunk> = csv::chunks(&maps, CHUNK);
    // clicks first: they are cheaper per byte, events then balance the tail
    chunks.sort_by_key(|c| (c.file < n_ev_files) as u8);
    let ev_bytes: usize = maps[..n_ev_files].iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let cl_bytes: usize = maps[n_ev_files..].iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let ev_est = ev_bytes / 75;
    let ev_cap = ev_est * 5 / 4 / NB / threads + 64;
    let cl_cap = cl_bytes / 45 * 5 / 4 / NPART / threads + 64;
    let long_click = AtomicBool::new(false);
    let huge = std::env::var_os("FAST_NO_THP").is_none();
    t("map+chunk");

    let cl_lo = base;
    let states = par_items(
        threads,
        chunks.len(),
        |_| ParseState {
            ev: WcRegions::new_with(NB, ev_cap, huge),
            side: Vec::with_capacity(ev_cap * NB / 20),
            cl: WcRegions::new_with(NPART, cl_cap, huge),
            cbuf: Vec::new(),
        },
        |st, i| {
            let c = chunks[i];
            let m: &[u8] = maps[c.file].as_ref().unwrap();
            let _fence = Defer(crate::alloc::sfence);
            let _t0 = Instant::now();
            let _g = Defer(|| { let ns = _t0.elapsed().as_nanos() as usize; if c.file < n_ev_files { EV_NS.fetch_add(ns, Ordering::Relaxed); } else { CL_NS.fetch_add(ns, Ordering::Relaxed); } });
            if c.file < n_ev_files {
                csv::scan_fast::<7, _>(m, c.start, c.end, |f| {
                    if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut st.side) {
                        st.ev.push(bucket_of(&r), r);
                    }
                });
            } else {
                st.cbuf.clear();
                csv::scan_fast::<4, _>(m, c.start, c.end, |[cid, user, camp, ts]| {
                    if cid.is_empty() || user.is_empty() {
                        return;
                    }
                    let Some(t) = parse_ts(ts) else { return };
                    if t < cl_lo || t >= we {
                        return;
                    }
                    let Some(ci) = lk.campaigns.get(pack_in(m, camp)) else { return };
                    let ck = pack_in(m, cid);
                    if ids::is_long(ck) {
                        long_click.store(true, Ordering::Relaxed);
                    }
                    // person resolved below (batched with prefetch); hold the user key for now
                    st.cbuf.push(CRec { cid: ck, person: pack_in(m, user), ts: (t - base) as u32, camp: ci as u16 });
                });
                let n = st.cbuf.len();
                for j in 0..n {
                    if j + PF < n {
                        links.prefetch(st.cbuf[j + PF].person);
                    }
                    let mut r = st.cbuf[j];
                    r.person = person_of(&links, r.person);
                    st.cl.push(ids::part_of(r.person), r);
                }
            }
        },
    );
    if std::env::var_os("FAST_VERBOSE").is_some() { eprintln!("[clean]   cpu: events {:.1} ms, clicks {:.1} ms", EV_NS.load(Ordering::Relaxed) as f64 / 1e6, CL_NS.load(Ordering::Relaxed) as f64 / 1e6); }
    let mut states = std::mem::ManuallyDrop::new(states);
    par_items(threads, states.len(), |_| (), |_, i| {
        let st = unsafe { &mut *((*states).as_ptr().add(i) as *mut ParseState) };
        st.ev.finish();
        st.cl.finish();
    });
    t("parse");

    // dedup by event_id, per bucket
    let states = &states;
    let out_cap = ev_est * 5 / 4 / NPART / threads + 64;
    let deduped = par_items(
        threads,
        NB,
        |_| (Vec::<u64>::new(), Vec::<(Key, u32, u32, u16)>::new(), Vec::<u32>::new(), WcRegions::<PRec, 4>::new_with(NPART, out_cap, huge)),
        |(table, surv, losers, out), b| {
            let _fence = Defer(crate::alloc::sfence);
            let n: usize = states.iter().map(|s| s.ev.len_of(b)).sum();
            if n == 0 {
                return;
            }
            let cap = (n + n / 2).next_power_of_two().max(64);
            table.clear();
            table.resize(cap, 0);
            let mask = cap - 1;
            let get = |r: u32| -> &ERec { unsafe { states.get_unchecked((r >> 26) as usize) }.ev.at(b, (r & ((1 << 26) - 1)) as usize) };
            losers.clear();
            for (ti, s) in states.iter().enumerate() {
                s.ev.each(b, |ri, r| {
                    let h = hk(r.eid);
                    let tag = h >> 32;
                    let mut i = (h >> NB_BITS) as usize & mask;
                    let me = ((ti as u32) << 26) | ri as u32;
                    loop {
                        let slot = unsafe { table.get_unchecked_mut(i) };
                        if *slot == 0 {
                            *slot = (tag << 32) | (me as u64 + 1);
                            break;
                        }
                        if (*slot >> 32) == tag {
                            let o = (*slot as u32).wrapping_sub(1);
                            let old = get(o);
                            if old.eid == r.eid {
                                if r.ingest < old.ingest {
                                    *slot = (tag << 32) | (me as u64 + 1);
                                    losers.push(o);
                                } else {
                                    losers.push(me);
                                }
                                break;
                            }
                        }
                        i = (i + 1) & mask;
                    }
                });
            }
            losers.sort_unstable();
            surv.clear();
            let mut li = 0;
            for (ti, s) in states.iter().enumerate() {
                s.ev.each(b, |ri, r| {
                    let me = ((ti as u32) << 26) | ri as u32;
                    if li < losers.len() && losers[li] == me {
                        li += 1;
                        return;
                    }
                    if r.ts == NOT_OK {
                        return;
                    }
                    let (price, cat) = if r.pc == 0 { (0, 0) } else { s.side[r.pc as usize - 1] };
                    surv.push((r.user, r.ts, price, cat));
                });
            }
            let n = surv.len();
            for j in 0..n {
                if j + PF < n {
                    links.prefetch(surv[j + PF].0);
                }
                let (user, ts, price, cat) = surv[j];
                let person = person_of(&links, user);
                out.push(ids::part_of(person), PRec { person, ts, price, cat });
            }
        },
    );
    let mut deduped = std::mem::ManuallyDrop::new(deduped);
    for d in deduped.iter_mut() {
        d.3.finish();
    }
    t("dedup");

    // per part: dense person ids, clicks
    let deduped = &deduped;
    let mut parts_out: Vec<Mutex<Option<Part>>> = (0..NPART).map(|_| Mutex::new(None)).collect();
    {
        let parts_ref = &parts_out;
        par_items(threads, NPART, |_| (), |_, p| {
            let n: usize = deduped.iter().map(|(_, _, _, o)| o.len_of(p)).sum();
            let mut local = KeyMap::with_capacity(1024);
            let mut names: Vec<Key> = Vec::new();
            let mut ev: Vec<Ev> = Vec::with_capacity(n);
            for (_, _, _, o) in deduped.iter() {
                o.each(p, |_, r| {
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
                });
            }
            let mut cl: Vec<Cl> = Vec::new();
            for s in states.iter() {
                s.cl.each(p, |_, r| {
                    if let Some(v) = local.get(r.person) {
                        cl.push(Cl { click_id: r.cid, person: v as u32, ts: r.ts, camp: r.camp });
                    }
                });
            }
            let person_names = names.into_iter().map(key_bytes).collect();
            *parts_ref[p].lock().unwrap() = Some(Part { person_names, ev, cl });
        });
    }
    let mut parts: Vec<Part> = parts_out.iter_mut().map(|m| m.get_mut().unwrap().take().unwrap()).collect();
    t("parts");

    if long_click.load(Ordering::Relaxed) && std::env::var_os("FAST_DUMP_NO_RANK").is_none() {
        rank_click_ids(&mut parts);
        t("rank clicks");
    }

    // Intermediates are leaked on purpose: freeing GBs (munmap + TLB
    // shootdowns) costs ~0.1-0.2 s, and the process exits right after output.
    std::mem::forget(links);
    std::mem::forget(maps);
    t("leak");
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
fn parse_event(m: &[u8], f: [&[u8]; 7], ws: i64, we: i64, base: i64, lk: &Lookups, side: &mut Vec<(u32, u16)>) -> Option<ERec> {
    let [eid, user, ts, ing, typ, prod, price] = f;
    if eid.is_empty() {
        return None;
    }
    let ingest = parse_ts(ing).unwrap_or(i64::MAX);
    let ek = pack_in(m, eid);
    let mut rec = ERec { eid: ek, user: 0, ingest, ts: NOT_OK, pc: 0 };
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
                let cat = lk.products.get(pack_in(m, prod)).map_or(lk.unknown_cat, |c| c as u16);
                side.push((p, cat));
                rec.pc = side.len() as u32;
            }
            _ => break 'v,
        }
        rec.user = pack_in(m, user);
        rec.ts = (t - base) as u32;
    }
    Some(rec)
}

#[inline(always)]
fn bucket_of(r: &ERec) -> usize {
    (hk(r.eid) & (NB as u64 - 1)) as usize
}

/// Single-thread microbenchmark of the event parse path over in-memory files
/// (min of 5 reps), for optimization work only.
pub fn microbench(dir: &Path) {
    let (ws, we) = read_manifest(&dir.join("manifest.json"));
    let base = ws - 7 * DAY_MS;
    let mut products = KeyMap::with_capacity(60000);
    data_lines(&dir.join("products.csv"), |line| {
        if let Some([p, _c]) = csv::split::<2>(line) {
            products.insert(pack(p), 1);
        }
    });
    let lk = Lookups { products, unknown_cat: 0, campaigns: KeyMap::with_capacity(16) };
    let mut data: Vec<u8> = Vec::new();
    for f in csv::list_csv(&dir.join("events")) {
        let b = std::fs::read(f).unwrap();
        if let Some(h) = memchr::memchr(b'\n', &b) {
            data.extend_from_slice(&b[h + 1..]);
        }
    }
    data.extend_from_slice(&[0u8; 64]);
    let n = data.len() - 64;
    let lines = memchr::memchr_iter(b'\n', &data[..n]).count();
    let run = |name: &str, f: &mut dyn FnMut() -> u64| {
        let mut best = f64::MAX;
        let mut chk = 0;
        for _ in 0..5 {
            let t = Instant::now();
            chk = f();
            best = best.min(t.elapsed().as_secs_f64());
        }
        eprintln!("{:<10} {:>7.1} ns/line  {:>6.2} GB/s  (chk {})", name, best * 1e9 / lines as f64, n as f64 / best / 1e9, chk);
    };
    let m = &data[..];
    run("newlines", &mut || memchr::memchr_iter(b'\n', &m[..n]).count() as u64);
    run("scan", &mut || {
        let mut x = 0u64;
        csv::scan::<7, _>(m, 0, n, |f| x += f[2].len() as u64);
        x
    });
    run("scan_fast", &mut || {
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| x += f[2].len() as u64);
        x
    });
    run("scan+ts", &mut || {
        let mut x = 0u64;
        csv::scan::<7, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts(f[2]).unwrap_or(0) as u64 ^ parse_ts(f[3]).unwrap_or(0) as u64));
        x
    });
    run("scan+pack", &mut || {
        let mut x = 0u64;
        csv::scan::<7, _>(m, 0, n, |f| x = x.wrapping_add(hk(pack_in(m, f[0])) ^ pack_in(m, f[1]) as u64));
        x
    });
    run("noout", &mut || {
        let mut side = Vec::new();
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { x = x.wrapping_add(r.ts as u64 ^ r.user as u64 ^ r.eid as u64 ^ r.ingest as u64) });
        x
    });
    let cap = lines / NB * 5 / 4 + 64;
    run("vecs", &mut || {
        let mut side = Vec::new();
        let mut ev: Vec<Vec<ERec>> = (0..NB).map(|_| Vec::with_capacity(cap)).collect();
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { unsafe { ev.get_unchecked_mut(bucket_of(&r)) }.push(r) });
        ev.iter().map(|v| v.len() as u64).sum()
    });
    let mut reuse: Vec<Vec<ERec>> = (0..NB).map(|_| Vec::with_capacity(cap)).collect();
    run("vecs-warm", &mut || {
        let mut side = Vec::new();
        for v in reuse.iter_mut() { v.clear(); }
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { unsafe { reuse.get_unchecked_mut(bucket_of(&r)) }.push(r) });
        reuse.iter().map(|v| v.len() as u64).sum()
    });
    run("regions", &mut || {
        let mut side = Vec::new();
        let mut ev = crate::alloc::Regions::<ERec>::new(NB, cap);
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { ev.push(bucket_of(&r), r) });
        (0..NB).map(|b| ev.len_of(b) as u64).sum()
    });
    let mut rw = crate::alloc::Regions::<ERec>::new_with(NB, cap, false);
    run("regions-warm", &mut || {
        let mut side = Vec::new();
        rw.clear();
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { rw.push(bucket_of(&r), r) });
        (0..NB).map(|b| rw.len_of(b) as u64).sum()
    });
    let mut wc = crate::alloc::WcRegions::<ERec, 4>::new(NB, cap);
    run("wc-warm", &mut || {
        let mut side = Vec::new();
        wc.clear();
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { wc.push(bucket_of(&r), r) });
        wc.finish();
        (0..NB).map(|b| wc.len_of(b) as u64).sum()
    });
    run("wc-fresh", &mut || {
        let mut side = Vec::new();
        let mut wc = crate::alloc::WcRegions::<ERec, 4>::new(NB, cap);
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { wc.push(bucket_of(&r), r) });
        wc.finish();
        (0..NB).map(|b| wc.len_of(b) as u64).sum()
    });
    run("wc-huge", &mut || {
        let mut side = Vec::new();
        let mut wc = crate::alloc::WcRegions::<ERec, 4>::new_with(NB, cap, true);
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { wc.push(bucket_of(&r), r) });
        wc.finish();
        (0..NB).map(|b| wc.len_of(b) as u64).sum()
    });
    let mut sw: Vec<ERec> = Vec::with_capacity(lines);
    run("seq-warm", &mut || {
        let mut side = Vec::new();
        sw.clear();
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { sw.push(r) });
        sw.len() as u64
    });
    run("seq", &mut || {
        let mut side = Vec::new();
        let mut ev: Vec<ERec> = Vec::with_capacity(lines);
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Some(r) = parse_event(m, f, ws, we, base, &lk, &mut side) { ev.push(r) });
        ev.len() as u64
    });
}
