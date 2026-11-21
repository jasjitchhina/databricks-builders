//! Parse + clean + dedup + link: DATA_DIR -> model::Cleaned.

use crate::alloc::WcRegions;
use crate::csv::{self, pack_in, Chunk};
use crate::ids::{key_cmp, pack, Key, KeyMap, NPART};
use crate::keys::{part_of, part_of_h, GMap, GSharded, IdKey};
use crate::model::{Cl, Cleaned, Ev, Part, Rows};
use crate::ts::{parse_ingest_in, parse_ts, parse_ts_in, DAY_MS};
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static EV_NS: AtomicUsize = AtomicUsize::new(0);
static CL_NS: AtomicUsize = AtomicUsize::new(0);
static DD_NS: [AtomicUsize; 3] = [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];
struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
const NB_BITS: u32 = 10;
const CM_PER: usize = 8; // 2-bit counter slots per record in a dedup bucket
const NB: usize = 1 << NB_BITS; // dedup buckets
const CHUNK: usize = 2 << 20;
const MAX_PRICE: u64 = 10_000_000;
const NOT_OK: u32 = u32::MAX;
const PF: usize = 20; // prefetch distance

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
    let b = std::fs::read(p).unwrap_or_else(|e| {
        eprintln!("fatal: read {}: {e}", p.display());
        std::process::exit(2)
    });
    if let Some(h) = memchr::memchr(b'\n', &b) {
        csv::for_lines(&b[h + 1..], &mut f);
    }
}

/// A parsed, well-formed event line with a non-empty event_id.
#[derive(Clone, Copy)]
struct ERec<K: IdKey> {
    eid: K,
    user: K,
    ingest: i64,
    /// ts - base, or NOT_OK if the copy fails validation / is late.
    ts: u32,
    /// 0 for view/cart; else 1 + index into the thread's purchase side table.
    pc: u32,
}

#[derive(Clone, Copy)]
struct CRec<K: IdKey> {
    cid: K,
    person: K,
    ts: u32,
    camp: u16,
}

/// A surviving event routed to its person's part. `pc` = 0 for view/cart;
/// for a purchase, price | cat << 24 when there are at most 256 categories
/// (price <= 10,000,000 < 2^24), else 1 + index into the dedup thread's side table.
#[derive(Clone, Copy)]
struct PRec<K: IdKey> {
    person: K,
    ts: u32,
    pc: u32,
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

/// Link stage 1 output: interned link IDs and the endpoint -> id table.
struct LinkPre<K: IdKey> {
    ep: Vec<Vec<AtomicU32>>,
    shard_keys: Vec<Vec<K>>,
}

/// Link stage 1 (parallel): parse every link line and intern its IDs.
/// None: an ID is not representable as K.
fn links_stage1<K: IdKey>(dir: &Path, threads: usize) -> Option<LinkPre<K>> {
    let mut t = timer();
    let files = csv::list_csv(&dir.join("links"));
    let maps: Vec<Option<Mmap>> = files.iter().map(|p| csv::map(p)).collect();
    let chunks = csv::chunks(&maps, 1 << 20);
    let bad = AtomicBool::new(false);
    // ~20 bytes per link line, 2 endpoints each: presize the shard buckets
    let link_bytes: usize = maps.iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let shard_cap = link_bytes / 20 * 2 * 5 / 4 / threads.max(1) / LS + 8;
    // per thread: endpoints in order (pair j = endpoints 2j, 2j+1), bucketed by shard as (key, endpoint ref)
    struct LS_<K> {
        n_ep: u32,
        t: u32,
        shards: Vec<Vec<(K, u32)>>,
    }
    let per = par_items(
        threads,
        chunks.len(),
        |t| LS_::<K> { n_ep: 0, t: t as u32, shards: (0..LS).map(|_| Vec::with_capacity(shard_cap)).collect() },
        |st, i| {
            if bad.load(Ordering::Relaxed) {
                return;
            }
            let c = chunks[i];
            let m: &[u8] = maps[c.file].as_ref().unwrap();
            csv::scan_fast::<2, _>(m, c.start, c.end, |[a, b]| {
                if !a.is_empty() && !b.is_empty() && a != b {
                    let (Some(ka), Some(kb)) = (K::pack(m, a), K::pack(m, b)) else {
                        bad.store(true, Ordering::Relaxed);
                        return;
                    };
                    for k in [ka, kb] {
                        let r = (st.t << 27) | st.n_ep;
                        st.n_ep += 1;
                        st.shards[GSharded::<K>::shard_of(k)].push((k, r));
                    }
                }
            });
        },
    );
    if bad.load(Ordering::Relaxed) {
        return None;
    }
    t(" L parse");
    assert!(per.iter().all(|p| p.n_ep < (1 << 27)));
    // endpoint -> gid, written by shard tasks
    let ep: Vec<Vec<AtomicU32>> = (0..threads)
        .map(|t| {
            let n = per.iter().find(|p| p.t as usize == t).map_or(0, |p| p.n_ep) as usize;
            (0..n).map(|_| AtomicU32::new(0)).collect()
        })
        .collect();
    let per_by_t: Vec<&LS_<K>> = {
        let mut v: Vec<&LS_<K>> = per.iter().collect();
        v.sort_by_key(|p| p.t);
        v
    };
    // per shard: intern -> local ids, keys
    let shard_keys: Vec<Mutex<Vec<K>>> = (0..LS).map(|_| Mutex::new(Vec::new())).collect();
    par_items(threads, LS, |_| GMap::<K, u32>::with_capacity(0), |tab, s| {
        let n: usize = per_by_t.iter().map(|p| p.shards[s].len()).sum();
        tab.reset_for(n);
        let mut keys = Vec::with_capacity(n / 2 + 1);
        for p in &per_by_t {
            for &(k, r) in &p.shards[s] {
                let local = match tab.get_or_insert(k, keys.len() as u32) {
                    Some(v) => v,
                    None => {
                        keys.push(k);
                        (keys.len() - 1) as u32
                    }
                };
                ep[(r >> 27) as usize][(r & ((1 << 27) - 1)) as usize].store(((s as u32) << 24) | local, Ordering::Relaxed);
            }
        }
        assert!(keys.len() < (1 << 24));
        *shard_keys[s].lock().unwrap() = keys;
    });
    let shard_keys: Vec<Vec<K>> = shard_keys.into_iter().map(|m| m.into_inner().unwrap()).collect();
    t(" L intern");
    std::mem::forget(per);
    std::mem::forget(maps);
    Some(LinkPre { ep, shard_keys })
}

/// Link stage 2: union-find (union by key order, so every root is its
/// component's byte-wise smallest ID = the person's name) and the id -> name
/// map for every linked id whose name is not itself.
fn links_stage2<K: IdKey>(pre: LinkPre<K>, threads: usize) -> GSharded<K> {
    let mut t = timer();
    let LinkPre { ep, shard_keys } = pre;
    let mut base = vec![0u32; LS + 1];
    for s in 0..LS {
        base[s + 1] = base[s] + shard_keys[s].len() as u32;
    }
    let m = base[LS] as usize;
    let gid = |x: u32| base[(x >> 24) as usize] + (x & 0xFF_FFFF);
    let mut keys: Vec<K> = Vec::with_capacity(m);
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
                if K::less(keys[ra as usize], keys[rb as usize]) {
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
    let lists: Vec<Mutex<Vec<(K, K)>>> = (0..LS).map(|_| Mutex::new(Vec::new())).collect();
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
    let lists: Vec<Vec<(K, K)>> = lists.into_iter().map(|m| m.into_inner().unwrap()).collect();
    t(" L names");
    let map = GSharded::<K>::new(lists.iter().map(|v| v.len()).max().unwrap_or(0));
    par_items(threads, LS, |_| (), |_, s| {
        for &(k, nm) in &lists[s] {
            unsafe { map.insert_shard(s, k, nm) };
        }
    });
    t(" L map");
    std::mem::forget(ep);
    std::mem::forget(shard_keys);
    std::mem::forget(lists);
    map
}

struct Lookups {
    products: KeyMap, // product key -> cat idx
    unknown_cat: u16,
    campaigns: KeyMap, // campaign key -> idx
    strict_ingest: bool,
}

impl Lookups {
    #[inline(always)]
    fn campaign(&self, m: &[u8], f: &[u8]) -> Option<u16> {
        self.campaigns.get(pack_in(m, f)).map(|v| v as u16)
    }
    #[inline(always)]
    fn category(&self, m: &[u8], f: &[u8]) -> u16 {
        self.products.get(pack_in(m, f)).map_or(self.unknown_cat, |v| v as u16)
    }
}

struct ParseState<K: IdKey> {
    ev: WcRegions<ERec<K>, 4>,
    side: Vec<(u32, u16)>,
    cl: WcRegions<CRec<K>, 8>,
    cbuf: Vec<CRec<K>>,
    chash: Vec<u64>,
}

#[inline(always)]
fn person_of<K: IdKey>(links: &GSharded<K>, user: K) -> K {
    links.get(user).unwrap_or(user)
}

struct Small {
    ws: i64,
    we: i64,
    base: i64,
    lk: Lookups,
    cat_names: Vec<Box<[u8]>>,
    camp_ids: Vec<Box<[u8]>>,
    camp_channels: Vec<Box<[u8]>>,
}

static WIDE_USED: AtomicBool = AtomicBool::new(false);

/// Bytes of a `Cl::click_id` from the last `clean` call (dump/debug tool).
pub fn click_id_bytes(k: u128) -> Box<[u8]> {
    if WIDE_USED.load(Ordering::Relaxed) {
        crate::ids::key_bytes(k)
    } else {
        (k as u64).bytes()
    }
}

pub fn clean(dir: &Path, threads: usize) -> Cleaned {
    // record refs pack the worker index: links use 5 bits (t << 27), dedup 6 (t << 26)
    let threads = threads.clamp(1, 31);
    // products/campaigns/manifest load on a side thread while links stage 1 runs
    let small_cell: std::sync::OnceLock<Small> = std::sync::OnceLock::new();
    std::thread::scope(|sc| {
        sc.spawn(|| {
            let t0 = Instant::now();
            let _ = small_cell.set(read_small(dir));
            if std::env::var_os("FAST_VERBOSE").is_some() {
                eprintln!("[clean] small files    {:>8.1} ms (side thread)", t0.elapsed().as_secs_f64() * 1e3);
            }
        });
        WIDE_USED.store(false, Ordering::Relaxed);
        if std::env::var_os("FAST_WIDE").is_none() {
            if let Some(c) = clean_impl::<u64>(dir, threads, &small_cell) {
                return c;
            }
            if std::env::var_os("FAST_VERBOSE").is_some() {
                eprintln!("[clean] an ID is not compact: rerunning with u128 keys");
            }
        }
        WIDE_USED.store(true, Ordering::Relaxed);
        clean_impl::<u128>(dir, threads, &small_cell).expect("u128 keys represent every ID")
    })
}

fn read_small(dir: &Path) -> Small {
    let (ws, we) = read_manifest(&dir.join("manifest.json"));
    let base = ws - 7 * DAY_MS;

    // products / campaigns
    let mut cat_names: Vec<Box<[u8]>> = Vec::new();
    let mut cat_idx: HashMap<Vec<u8>, u16> = HashMap::new();
    let mut products = KeyMap::with_capacity(1 << 16);
    data_lines(&dir.join("products.csv"), |line| {
        if let Some([p, c]) = csv::split::<2>(line) {
            let ci = match cat_idx.get(c) {
                Some(&ci) => ci,
                None => {
                    cat_names.push(c.into());
                    let ci = (cat_names.len() - 1) as u16;
                    cat_idx.insert(c.to_vec(), ci);
                    ci
                }
            };
            products.insert(pack(p), ci as Key);
        }
    });
    let unknown_cat = cat_names.len() as u16;
    cat_names.push(b"unknown"[..].into());
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
    let lk = Lookups { products, unknown_cat, campaigns, strict_ingest: std::env::var_os("FAST_STRICT_INGEST").is_some() };
    Small { ws, we, base, lk, cat_names, camp_ids, camp_channels }
}

fn clean_impl<K: IdKey>(dir: &Path, threads: usize, small_cell: &std::sync::OnceLock<Small>) -> Option<Cleaned> {
    let mut t = timer();
    let bad = AtomicBool::new(false);
    let pre = links_stage1::<K>(dir, threads)?;
    t("links stage1");
    let small = small_cell.wait();
    t("small wait");
    let (ws, we, base, lk) = (small.ws, small.we, small.base, &small.lk);
    let unknown_cat = lk.unknown_cat;
    // Stage 2 of links runs on its own thread while the workers parse events;
    // click chunks (which need person = link name) come last and wait for it.
    let link_cell: std::sync::OnceLock<GSharded<K>> = std::sync::OnceLock::new();
    let link_thread_done = AtomicBool::new(false);

    // events + clicks
    let ev_files = csv::list_csv(&dir.join("events"));
    let cl_files = csv::list_csv(&dir.join("clicks"));
    let n_ev_files = ev_files.len();
    let maps: Vec<Option<Mmap>> = ev_files.iter().chain(cl_files.iter()).map(|p| csv::map(p)).collect();
    let mut chunks: Vec<Chunk> = csv::chunks(&maps, CHUNK);
    // events first (no links needed), then clicks (wait for links stage 2)
    chunks.sort_by_key(|c| (c.file >= n_ev_files) as u8);
    let ev_bytes: usize = maps[..n_ev_files].iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let cl_bytes: usize = maps[n_ev_files..].iter().map(|m| m.as_ref().map_or(0, |m| m.len())).sum();
    let ev_est = ev_bytes / 75;
    // dedup buckets: 1024. More buckets speed up dedup at 60M but slow the
    // parse (more write-combining stages competing for L2); with 2-bit
    // counters 1024 is as good as 2048 at 60M and best at 20M (HT-like A/B).
    let nb_bits: u32 = std::env::var("FAST_NB_BITS").ok().and_then(|v| v.parse().ok()).unwrap_or(NB_BITS).clamp(6, 14);
    let nb = 1usize << nb_bits;
    let nbm = (nb - 1) as u64;
    let ev_cap = ev_est * 5 / 4 / nb / threads + 64;
    let cl_cap = cl_bytes / 45 * 5 / 4 / NPART / threads + 64;
    let long_click = AtomicBool::new(false);
    let huge = std::env::var_os("FAST_NO_THP").is_none();
    t("map+chunk");

    let cl_lo = base;
    let states = std::thread::scope(|sc| {
    sc.spawn(|| {
        let _ = link_cell.set(links_stage2(pre, 2));
        link_thread_done.store(true, Ordering::Release);
    });
    par_items(
        threads,
        chunks.len(),
        |_| ParseState::<K> {
            ev: WcRegions::new_with(nb, ev_cap, huge),
            side: Vec::with_capacity(ev_cap * nb / 20),
            cl: WcRegions::new_with(NPART, cl_cap, huge),
            cbuf: Vec::new(),
            chash: Vec::new(),
        },
        |st, i| {
            if bad.load(Ordering::Relaxed) {
                return;
            }
            let c = chunks[i];
            let m: &[u8] = maps[c.file].as_ref().unwrap();
            let _fence = Defer(crate::alloc::sfence);
            let _t0 = Instant::now();
            let _g = Defer(|| { let ns = _t0.elapsed().as_nanos() as usize; if c.file < n_ev_files { EV_NS.fetch_add(ns, Ordering::Relaxed); } else { CL_NS.fetch_add(ns, Ordering::Relaxed); } });
            if c.file < n_ev_files {
                let mut ok = true;
                csv::scan_fast::<7, _>(m, c.start, c.end, |f| match parse_event::<K>(m, f, ws, we, base, lk, &mut st.side) {
                    Ok(Some(r)) => st.ev.push(bucket_of(&r, nbm), r),
                    Ok(None) => {}
                    Err(()) => ok = false,
                });
                if !ok {
                    bad.store(true, Ordering::Relaxed);
                }
            } else {
                let links = link_cell.wait();
                st.cbuf.clear();
                let mut ok = true;
                csv::scan_fast::<4, _>(m, c.start, c.end, |[cid, user, camp, ts]| {
                    if cid.is_empty() || user.is_empty() {
                        return;
                    }
                    let Some(t) = parse_ts_in(m, ts) else { return };
                    if t < cl_lo || t >= we {
                        return;
                    }
                    let Some(ci) = lk.campaign(m, camp) else { return };
                    let (Some(ck), Some(uk)) = (K::pack(m, cid), K::pack(m, user)) else {
                        ok = false;
                        return;
                    };
                    if ck.is_long() {
                        long_click.store(true, Ordering::Relaxed);
                    }
                    // person resolved below (batched with prefetch); hold the user key for now
                    st.cbuf.push(CRec { cid: ck, person: uk, ts: (t - base) as u32, camp: ci as u16 });
                });
                if !ok {
                    bad.store(true, Ordering::Relaxed);
                }
                let n = st.cbuf.len();
                st.chash.clear();
                st.chash.extend(st.cbuf.iter().map(|r| r.person.hash()));
                for j in 0..n {
                    if j + 2 * PF < n {
                        links.prefetch_filter_h(unsafe { *st.chash.get_unchecked(j + 2 * PF) });
                    }
                    if j + PF < n {
                        links.prefetch_if_h(unsafe { *st.chash.get_unchecked(j + PF) });
                    }
                    let mut r = unsafe { *st.cbuf.get_unchecked(j) };
                    let h = unsafe { *st.chash.get_unchecked(j) };
                    let ph = match links.get_h(r.person, h) {
                        Some(nm) => {
                            r.person = nm;
                            nm.hash()
                        }
                        None => h,
                    };
                    st.cl.push(part_of_h(ph), r);
                }
            }
        },
    )
    });
    let links = link_cell.into_inner().expect("links stage 2");
    let _ = link_thread_done.load(Ordering::Acquire);
    if std::env::var_os("FAST_VERBOSE").is_some() { eprintln!("[clean]   cpu: events {:.1} ms, clicks {:.1} ms", EV_NS.load(Ordering::Relaxed) as f64 / 1e6, CL_NS.load(Ordering::Relaxed) as f64 / 1e6); }
    if bad.load(Ordering::Relaxed) {
        // not representable as K: leak (cheaper than freeing) and let the caller rerun wider
        std::mem::forget(states);
        std::mem::forget(maps);
        std::mem::forget(links);
        return None;
    }
    let states = std::mem::ManuallyDrop::new(states);
    par_items(threads, states.len(), |_| (), |_, i| {
        let st = unsafe { &mut *((*states).as_ptr().add(i) as *mut ParseState<K>) };
        st.ev.finish();
        st.cl.finish();
    });
    t("parse");

    // dedup by event_id, per bucket
    let states = &states;
    let out_cap = ev_est * 5 / 4 / NPART / threads + 64;
    // categories incl. unknown fit 8 bits => price and category pack into one u32
    let small_cat = small.cat_names.len() <= 256 && std::env::var_os("FAST_SIDE_CAT").is_none();
    let deduped = par_items(
        threads,
        nb,
        |_| (Vec::<u64>::new(), Vec::<(K, u64, u32, u32, u16)>::new(), Vec::<(u64, u32, ERec<K>)>::new(), WcRegions::<PRec<K>, 4>::new_with(NPART, out_cap, huge), Vec::<u8>::new(), Vec::<(u32, u16)>::new(), Vec::<u64>::new()),
        |(table, surv, cand, out, cnt, pside, hs), b| {
            let _fence = Defer(crate::alloc::sfence);
            let n: usize = states.iter().map(|s| s.ev.len_of(b)).sum();
            if n == 0 {
                return;
            }
            let _q0 = Instant::now();
            // Pass 1: count event_id hashes into byte counters (~16 per record,
            // L2-resident). A record whose counter is 1 has a unique event_id
            // in this bucket (copies share the hash), so it skips the table.
            // 2-bit saturating counters, 4 per byte (0 / 1 / 2+), ~16 slots per record
            let cm = (n * CM_PER).next_power_of_two().max(64);
            let cmask = cm - 1;
            cnt.clear();
            cnt.resize(cm / 4, 0u8);
            hs.clear();
            for s in states.iter() {
                s.ev.each(b, |_, r| {
                    let h = r.eid.hash();
                    hs.push(h);
                    let i = (h >> nb_bits) as usize & cmask;
                    let c = unsafe { cnt.get_unchecked_mut(i >> 2) };
                    let sh = (i & 3) * 2;
                    *c += ((((*c >> sh) & 3) < 2) as u8) << sh;
                });
            }
            // Pass 2: unique -> survivor (if valid); otherwise a dedup candidate.
            surv.clear();
            cand.clear();
            let mut j = 0usize;
            for (ti, s) in states.iter().enumerate() {
                s.ev.each(b, |_ri, r| {
                    let h = unsafe { *hs.get_unchecked(j) };
                    j += 1;
                    let i = (h >> nb_bits) as usize & cmask;
                    if (unsafe { *cnt.get_unchecked(i >> 2) } >> ((i & 3) * 2)) & 3 == 1 {
                        if r.ts != NOT_OK {
                            let (price, cat) = if r.pc == 0 { (0, 0) } else { s.side[r.pc as usize - 1] };
                            surv.push((r.user, r.user.hash(), r.ts, price, cat));
                        }
                    } else {
                        cand.push((h, ti as u32, *r));
                    }
                });
            }
            let _q1 = Instant::now();
            // Pass 3: exact dedup among candidates (true copies + counter
            // collisions): keep the smallest ingest_ts; equal ingest_ts copies
            // are identical, the first one stays.
            // (candidates carry a copy of their record: no random re-fetch)
            let tcap = (cand.len() * 2).next_power_of_two().max(16);
            let mask = tcap - 1;
            table.clear();
            table.resize(tcap, 0);
            for (ci, (h, _, r)) in cand.iter().enumerate() {
                let tag = h >> 32;
                let mut i = (h >> nb_bits) as usize & mask;
                loop {
                    let slot = unsafe { table.get_unchecked_mut(i) };
                    if *slot == 0 {
                        *slot = (tag << 32) | (ci as u64 + 1);
                        break;
                    }
                    if (*slot >> 32) == tag {
                        let o = (*slot as u32).wrapping_sub(1) as usize;
                        let old = unsafe { &cand.get_unchecked(o).2 };
                        if old.eid == r.eid {
                            if r.ingest < old.ingest {
                                *slot = (tag << 32) | (ci as u64 + 1);
                            }
                            break;
                        }
                    }
                    i = (i + 1) & mask;
                }
            }
            for &e in table.iter() {
                if e == 0 {
                    continue;
                }
                let (_, ti, r) = unsafe { cand.get_unchecked((e as u32).wrapping_sub(1) as usize) };
                if r.ts == NOT_OK {
                    continue;
                }
                let s = unsafe { states.get_unchecked(*ti as usize) };
                let (price, cat) = if r.pc == 0 { (0, 0) } else { s.side[r.pc as usize - 1] };
                surv.push((r.user, r.user.hash(), r.ts, price, cat));
            }
            let _q2 = Instant::now();
            let n = surv.len();
            let _qd = Defer(|| { let q3 = Instant::now(); DD_NS[0].fetch_add((_q1 - _q0).as_nanos() as usize, Ordering::Relaxed); DD_NS[1].fetch_add((_q2 - _q1).as_nanos() as usize, Ordering::Relaxed); DD_NS[2].fetch_add((q3 - _q2).as_nanos() as usize, Ordering::Relaxed); });
            // 3-stage pipeline: filter word prefetch (j+2PF), filter test + slot
            // prefetch (j+PF), lookup (j); filter bits computed once per row.
            const RM: usize = 63;
            let mut ring = [(0usize, 0u64, false); RM + 1];
            for j in 0..(2 * PF).min(n) {
                let (w, bb) = links.fbits_h(unsafe { surv.get_unchecked(j) }.1);
                links.filter_prefetch_w(w);
                ring[j & RM] = (w, bb, false);
            }
            for j in 0..PF.min(n) {
                let e = &mut ring[j & RM];
                e.2 = links.filter_test_w(e.0, e.1);
                if e.2 {
                    links.slot_prefetch_h(unsafe { surv.get_unchecked(j) }.1);
                }
            }
            for j in 0..n {
                if j + 2 * PF < n {
                    let (w, bb) = links.fbits_h(unsafe { surv.get_unchecked(j + 2 * PF) }.1);
                    links.filter_prefetch_w(w);
                    ring[(j + 2 * PF) & RM] = (w, bb, false);
                }
                if j + PF < n {
                    let e = &mut ring[(j + PF) & RM];
                    e.2 = links.filter_test_w(e.0, e.1);
                    if e.2 {
                        links.slot_prefetch_h(unsafe { surv.get_unchecked(j + PF) }.1);
                    }
                }
                let (user, h, ts, price, cat) = unsafe { *surv.get_unchecked(j) };
                let maybe = ring[j & RM].2;
                let (person, ph) = match if maybe { links.get_slot_h(user, h) } else { None } {
                    Some(nm) => (nm, nm.hash()),
                    None => (user, h),
                };
                let pc = if price == 0 {
                    0
                } else if small_cat {
                    price | (cat as u32) << 24
                } else {
                    pside.push((price, cat));
                    pside.len() as u32
                };
                out.push(part_of_h(ph), PRec { person, ts, pc });
            }
        },
    );
    if std::env::var_os("FAST_VERBOSE").is_some() { eprintln!("[dedup] cpu: insert {:.1} ms, sort+rewalk {:.1} ms, person+scatter {:.1} ms", DD_NS[0].load(Ordering::Relaxed) as f64 / 1e6, DD_NS[1].load(Ordering::Relaxed) as f64 / 1e6, DD_NS[2].load(Ordering::Relaxed) as f64 / 1e6); }
    let mut deduped = std::mem::ManuallyDrop::new(deduped);
    for d in deduped.iter_mut() {
        d.3.finish();
    }
    t("dedup");

    // per part: dense person ids, clicks
    let recycle = std::env::var_os("FAST_NO_RECYCLE").is_none();
    let deduped = &deduped;
    let mut parts_out: Vec<Mutex<Option<Part>>> = (0..NPART).map(|_| Mutex::new(None)).collect();
    {
        let parts_ref = &parts_out;
        let part_states = par_items(threads, NPART, |t| {
            // recycle this thread's dead event-bucket buffer (dedup is done with it)
            let slabs = if t < states.len() && recycle { vec![states[t].ev.raw()] } else { Vec::new() };
            (unsafe { crate::alloc::Bump::multi(slabs) }, GMap::<K, u32>::with_capacity(4096), Vec::<K>::new())
        }, |(bump, local, names), p| {
            let n: usize = deduped.iter().map(|(_, _, _, o, _, _, _)| o.len_of(p)).sum();
            local.clear();
            names.clear();
            let mut ev_vec: Vec<Ev> = Vec::new();
            let ev_ptr: *mut Ev = match bump.take::<Ev>(n) {
                Some(q) => q,
                None => {
                    ev_vec.reserve_exact(n);
                    ev_vec.as_mut_ptr()
                }
            };
            let mut ne = 0usize;
            for (_, _, _, o, _, pside, _) in deduped.iter() {
                o.each(p, |_, r| {
                    let id = match local.get_or_insert(r.person, names.len() as u32) {
                        Some(v) => v,
                        None => {
                            names.push(r.person);
                            (names.len() - 1) as u32
                        }
                    };
                    let (price, cat) = if r.pc == 0 {
                        (0, 0)
                    } else if small_cat {
                        (r.pc & 0xFF_FFFF, (r.pc >> 24) as u16)
                    } else {
                        pside[r.pc as usize - 1]
                    };
                    unsafe { ev_ptr.add(ne).write(Ev { person: id, ts: r.ts, price, cat }) };
                    ne += 1;
                });
            }
            let ev = if ev_vec.capacity() > 0 || n == 0 {
                unsafe { ev_vec.set_len(ne) };
                Rows::Owned(ev_vec)
            } else {
                Rows::Static(unsafe { std::slice::from_raw_parts_mut(ev_ptr, ne) })
            };
            let ncl_max: usize = states.iter().map(|s| s.cl.len_of(p)).sum();
            let mut cl_vec: Vec<Cl> = Vec::new();
            let cl_ptr: *mut Cl = match bump.take::<Cl>(ncl_max) {
                Some(q) => q,
                None => {
                    cl_vec.reserve_exact(ncl_max);
                    cl_vec.as_mut_ptr()
                }
            };
            let mut nc = 0usize;
            for s in states.iter() {
                s.cl.each(p, |_, r| {
                    if let Some(v) = local.get(r.person) {
                        unsafe { cl_ptr.add(nc).write(Cl { click_id: r.cid.click(), person: v, ts: r.ts, camp: r.camp }) };
                        nc += 1;
                    }
                });
            }
            let cl = if cl_vec.capacity() > 0 || ncl_max == 0 {
                unsafe { cl_vec.set_len(nc) };
                Rows::Owned(cl_vec)
            } else {
                bump.shrink_last(cl_ptr, nc);
                Rows::Static(unsafe { std::slice::from_raw_parts_mut(cl_ptr, nc) })
            };
            let mut person_names = crate::model::Names::with_capacity(names.len(), names.len() * 10);
            for &k in names.iter() {
                person_names.push_with(|b| k.append_bytes(b));
            }
            *parts_ref[p].lock().unwrap() = Some(Part { person_names, ev, cl });
        });
        // Donate what is dead and already faulted in (click regions, part-record
        // regions, unused arena tails) to the global allocator (alloc::DonorAlloc)
        // for the engine's scratch buffers.
        if std::env::var_os("FAST_NO_DONATE").is_none() {
            unsafe {
                for (bump, _, _) in part_states.iter() {
                    for (p, l) in bump.rest_all() {
                        crate::alloc::donate(p, l);
                    }
                }
                for st in states.iter() {
                    let (p, l) = st.cl.raw();
                    crate::alloc::donate(p, l);
                }
                for d in deduped.iter() {
                    let (p, l) = d.3.raw();
                    crate::alloc::donate(p, l);
                }
            }
        }
        std::mem::forget(part_states);
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
    Some(Cleaned {
        base_ms: base,
        ws_ms: ws,
        we_ms: we,
        cat_names: small.cat_names.clone(),
        unknown_cat,
        camp_ids: small.camp_ids.clone(),
        camp_channels: small.camp_channels.clone(),
        parts,
    })
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

/// Ok(None): malformed for dedup purposes (empty event_id). Err: an ID is not representable as K.
#[inline(always)]
fn parse_event<K: IdKey>(m: &[u8], f: [&[u8]; 7], ws: i64, we: i64, base: i64, lk: &Lookups, side: &mut Vec<(u32, u16)>) -> Result<Option<ERec<K>>, ()> {
    let [eid, user, ts, ing, typ, prod, price] = f;
    if eid.is_empty() {
        return Ok(None);
    }
    // rules.md: ingest_ts is always valid F1 (trusted fast path, guarded by shape;
    // FAST_STRICT_INGEST=1 validates it like ts)
    let ingest = if lk.strict_ingest { parse_ts_in(m, ing) } else { parse_ingest_in(m, ing) }.unwrap_or(i64::MAX);
    let Some(ek) = K::pack(m, eid) else { return Err(()) };
    let mut rec = ERec { eid: ek, user: K::default(), ingest, ts: NOT_OK, pc: 0 };
    'v: {
        if user.is_empty() || prod.is_empty() {
            break 'v;
        }
        let Some(t) = parse_ts_in(m, ts) else { break 'v };
        if t < ws || t >= we {
            break 'v;
        }
        if ingest == i64::MAX || ingest - t > DAY_MS {
            break 'v;
        }
        let pc = match typ {
            b"view" | b"cart" => 0,
            b"purchase" => {
                let Some(p) = parse_price(price) else { break 'v };
                let cat = lk.category(m, prod);
                side.push((p, cat));
                side.len() as u32
            }
            _ => break 'v,
        };
        let Some(uk) = K::pack(m, user) else { return Err(()) };
        rec.user = uk;
        rec.pc = pc;
        rec.ts = (t - base) as u32;
    }
    Ok(Some(rec))
}

#[inline(always)]
fn bucket_of<K: IdKey>(r: &ERec<K>, nbm: u64) -> usize {
    (r.eid.hash() & nbm) as usize
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
    let lk = Lookups { products, unknown_cat: 0, campaigns: KeyMap::with_capacity(16), strict_ingest: false };
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
        csv::scan_fast::<7, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts(f[2]).unwrap_or(0) as u64 ^ parse_ts(f[3]).unwrap_or(0) as u64));
        x
    });
    run("scan+ts_in", &mut || {
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[2]).unwrap_or(0) as u64 ^ parse_ts_in(m, f[3]).unwrap_or(0) as u64));
        x
    });
    run("scan+ing", &mut || {
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[3]).unwrap_or(0) as u64));
        x
    });
    run("scan+tsonly", &mut || {
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[2]).unwrap_or(0) as u64));
        x
    });
    run("scan+ts+pk", &mut || {
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[2]).unwrap_or(0) as u64 ^ parse_ts_in(m, f[3]).unwrap_or(0) as u64 ^ <u64 as IdKey>::pack(m, f[0]).unwrap_or(0) ^ <u64 as IdKey>::pack(m, f[1]).unwrap_or(0)));
        x
    });
    run("scan+typ", &mut || {
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| x += match f[4] { b"view" | b"cart" => 1, b"purchase" => 2, _ => 3 });
        x
    });
    fn noout<K: IdKey>(m: &[u8], n: usize, ws: i64, we: i64, base: i64, lk: &Lookups) -> u64 {
        let mut side = Vec::new();
        let mut x = 0u64;
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Ok(Some(r)) = parse_event::<K>(m, f, ws, we, base, lk, &mut side) { x = x.wrapping_add(r.ts as u64 ^ r.user.hash() ^ r.eid.hash() ^ r.ingest as u64) });
        x
    }
    run("noout-u64", &mut || noout::<u64>(m, n, ws, we, base, &lk));
    run("noout-u128", &mut || noout::<u128>(m, n, ws, we, base, &lk));
    let cap = lines / NB * 5 / 4 + 64;
    fn wc<K: IdKey>(m: &[u8], n: usize, ws: i64, we: i64, base: i64, lk: &Lookups, cap: usize, huge: bool) -> u64 {
        let mut side = Vec::new();
        let mut wc = crate::alloc::WcRegions::<ERec<K>, 4>::new_with(NB, cap, huge);
        csv::scan_fast::<7, _>(m, 0, n, |f| if let Ok(Some(r)) = parse_event::<K>(m, f, ws, we, base, lk, &mut side) { wc.push(bucket_of(&r, NB as u64 - 1), r) });
        wc.finish();
        (0..NB).map(|b| wc.len_of(b) as u64).sum()
    }
    run("wc-u64", &mut || wc::<u64>(m, n, ws, we, base, &lk, cap, true));
    run("wc-u128", &mut || wc::<u128>(m, n, ws, we, base, &lk, cap, true));
}

/// Click-path microbenchmark (no person lookup), dev only.
pub fn microbench_clicks(dir: &Path) {
    let small = read_small(dir);
    let mut data: Vec<u8> = Vec::new();
    for f in csv::list_csv(&dir.join("clicks")) {
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
        eprintln!("{:<12} {:>7.1} ns/line  (chk {})", name, best * 1e9 / lines as f64, chk);
    };
    let m = &data[..];
    let (we, base, lk) = (small.we, small.base, &small.lk);
    run("scan4", &mut || { let mut x = 0u64; csv::scan_fast::<4, _>(m, 0, n, |f| x += f[3].len() as u64); x });
    run("+ts", &mut || { let mut x = 0u64; csv::scan_fast::<4, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[3]).unwrap_or(0) as u64)); x });
    run("+camp", &mut || { let mut x = 0u64; csv::scan_fast::<4, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[3]).unwrap_or(0) as u64 + lk.campaigns.get(pack_in(m, f[2])).unwrap_or(0) as u64)); x });
    run("+packs", &mut || { let mut x = 0u64; csv::scan_fast::<4, _>(m, 0, n, |f| x = x.wrapping_add(parse_ts_in(m, f[3]).unwrap_or(0) as u64 + lk.campaigns.get(pack_in(m, f[2])).unwrap_or(0) as u64 + <u64 as IdKey>::pack(m, f[0]).unwrap_or(0) + <u64 as IdKey>::pack(m, f[1]).unwrap_or(0))); x });
    run("full-nolink", &mut || {
        let mut cl = WcRegions::<CRec<u64>, 8>::new_with(NPART, lines / NPART * 2, true);
        csv::scan_fast::<4, _>(m, 0, n, |[cid, user, camp, ts]| {
            if cid.is_empty() || user.is_empty() { return; }
            let Some(t) = parse_ts_in(m, ts) else { return };
            if t < base || t >= we { return; }
            let Some(ci) = lk.campaigns.get(pack_in(m, camp)) else { return };
            let (Some(ck), Some(uk)) = (<u64 as IdKey>::pack(m, cid), <u64 as IdKey>::pack(m, user)) else { return };
            cl.push(part_of(uk), CRec { cid: ck, person: uk, ts: (t - base) as u32, camp: ci as u16 });
        });
        cl.finish();
        (0..NPART).map(|p| cl.len_of(p) as u64).sum()
    });
}
