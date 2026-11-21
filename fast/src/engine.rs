//! Back half: group by person, sessionize, attribute clicks, 7-day assists,
//! aggregate into user_daily / campaign_daily rows.
use crate::model::*;
use crate::{DAY_MS, GAP_MS, WEEK_MS};
use crate::pq;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
/// experiment switch (enginebench A/B)
pub static VARIANT: AtomicU64 = AtomicU64::new(0);
pub static T_GROUP: AtomicU64 = AtomicU64::new(0);
pub static T_SORT: AtomicU64 = AtomicU64::new(0);
pub static T_MAXPART: AtomicU64 = AtomicU64::new(0);
pub static T_WORKER_MAX: AtomicU64 = AtomicU64::new(0);
pub static T_PROC: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Default)]
pub struct CampAcc {
    pub sessions: u64,
    pub purchasing: u64,
    pub revenue: u64,
    pub assisted: u64,
}

pub struct Output {
    pub day0: i32,
    pub ndays: usize,
    /// [(ncamp+1) * ndays], last campaign index = organic
    pub camp: Vec<CampAcc>,
    pub user_rows: usize,
    pub session_dump: Option<Vec<Vec<u8>>>,
}

struct Scratch {
    cnt: Vec<u32>,
    /// per active campaign: index (within the current day) of the first session it assists
    since: Vec<u32>,
    active: Vec<u16>,
    pos: Vec<u32>,
    catsum: Vec<u64>,
    cat_touched: Vec<u16>,
    sess_cat: Vec<u64>,
    sess_touched: Vec<u16>,
    /// day-major [di * (ncamp+1) + k]
    camp: Vec<CampAcc>,
    /// day-major [di * (ncamp+1) + k], compact so one session's assists hit one 8KB row
    asst: Vec<u32>,
    ncamp1: usize,
    ucols: Vec<pq::ColBuf>,
    urows: usize,
    dump: Vec<u8>,
    eoff: Vec<u32>,
    coff: Vec<u32>,
    evs: Vec<Ev>,
    cls: Vec<Cl>,
    pevs: Vec<PEv>,
    pcls: Vec<PCl>,
    rtmp: Vec<u64>,
}

pub trait EvT: Copy {
    fn ts(&self) -> u32;
    fn price(&self) -> u32;
    fn cat(&self) -> u16;
}
impl EvT for Ev {
    #[inline(always)]
    fn ts(&self) -> u32 {
        self.ts
    }
    #[inline(always)]
    fn price(&self) -> u32 {
        self.price
    }
    #[inline(always)]
    fn cat(&self) -> u16 {
        self.cat
    }
}
/// packed event: ts(31) << 33 | price(24) << 9 | cat(9); u64 order sorts by ts
#[derive(Clone, Copy)]
pub struct PEv(u64);
impl EvT for PEv {
    #[inline(always)]
    fn ts(&self) -> u32 {
        (self.0 >> 33) as u32
    }
    #[inline(always)]
    fn price(&self) -> u32 {
        ((self.0 >> 9) & 0xFF_FFFF) as u32
    }
    #[inline(always)]
    fn cat(&self) -> u16 {
        (self.0 & 511) as u16
    }
}
pub trait ClT: Copy {
    fn ts(&self) -> u32;
    fn camp(&self) -> u16;
    fn cid(&self, part: &Part) -> u128;
}
impl ClT for Cl {
    #[inline(always)]
    fn ts(&self) -> u32 {
        self.ts
    }
    #[inline(always)]
    fn camp(&self) -> u16 {
        self.camp
    }
    #[inline(always)]
    fn cid(&self, _: &Part) -> u128 {
        self.click_id
    }
}
/// packed click: ts(31) << 33 | camp(12) << 21 | idx-in-part(21)
#[derive(Clone, Copy)]
pub struct PCl(u64);
impl ClT for PCl {
    #[inline(always)]
    fn ts(&self) -> u32 {
        (self.0 >> 33) as u32
    }
    #[inline(always)]
    fn camp(&self) -> u16 {
        ((self.0 >> 21) & 4095) as u16
    }
    #[inline(always)]
    fn cid(&self, part: &Part) -> u128 {
        part.cl[(self.0 & 0x1F_FFFF) as usize].click_id
    }
}

/// Sort u64 keys by their top 31 bits (ts << 33) with an LSD radix sort (3 passes
/// of 11/11/9 bits); ties keep no particular order. For large slices only.
fn radix_ts(v: &mut [u64], tmp: &mut Vec<u64>) {
    let n = v.len();
    tmp.clear();
    tmp.reserve(n);
    unsafe { tmp.set_len(n) };
    let mut h = [[0u32; 2048]; 3];
    for &x in v.iter() {
        h[0][((x >> 33) & 2047) as usize] += 1;
        h[1][((x >> 44) & 2047) as usize] += 1;
        h[2][(x >> 55) as usize] += 1;
    }
    let mut src_is_v = true;
    for (d, shift) in [(0usize, 33u32), (1, 44), (2, 55)] {
        let hd = &mut h[d];
        if hd.iter().any(|&c| c as usize == n) {
            continue;
        }
        let mut acc = 0u32;
        for c in hd.iter_mut() {
            let x = *c;
            *c = acc;
            acc += x;
        }
        let (src, dst): (&[u64], &mut [u64]) = if src_is_v { (&*v, &mut tmp[..]) } else { (&tmp[..], unsafe { &mut *(v as *mut [u64]) }) };
        // SAFETY: src and dst never alias (v vs tmp)
        let src: &[u64] = unsafe { std::slice::from_raw_parts(src.as_ptr(), n) };
        for &x in src {
            let k = ((x >> shift) & 2047) as usize;
            let p = hd[k] as usize;
            hd[k] += 1;
            unsafe { *dst.get_unchecked_mut(p) = x };
        }
        src_is_v = !src_is_v;
    }
    if !src_is_v {
        v.copy_from_slice(&tmp[..]);
    }
}

#[inline]
fn sort_ts(v: &mut [u64], tmp: &mut Vec<u64>) {
    if v.len() > RADIX_MIN.load(Relaxed) as usize {
        radix_ts(v, tmp);
    } else {
        v.sort_unstable();
    }
}
pub static RADIX_MIN: AtomicU64 = AtomicU64::new(512);

/// counting sort of items by person into `out` (mapped), offsets into `off` (len np+1)
fn group<T: Copy, U: Copy>(
    items: &[T],
    np: usize,
    key: impl Fn(&T) -> u32,
    map: impl Fn(usize, &T) -> U,
    off: &mut Vec<u32>,
    out: &mut Vec<U>,
) {
    off.clear();
    off.resize(np + 1, 0);
    for it in items {
        off[key(it) as usize + 1] += 1;
    }
    for i in 0..np {
        off[i + 1] += off[i];
    }
    out.clear();
    out.reserve(items.len());
    // scatter using a cursor copy
    let mut cur: Vec<u32> = off[..np].to_vec();
    unsafe {
        let o = out.as_mut_ptr();
        for (i, it) in items.iter().enumerate() {
            let k = key(it) as usize;
            o.add(cur[k] as usize).write(map(i, it));
            cur[k] += 1;
        }
        out.set_len(items.len());
    }
}

/// Dataset-wide constants the back half needs (no per-part data). Built from the
/// small files + manifest, so it exists before any part does.
pub struct Ctx {
    pub base_ms: i64,
    pub ncamp: usize,
    pub ncat: usize,
    pub organic: u16,
    base_day: i64,
    base_rem: u64,
    pub day0: i32,
    pub ndays: usize,
    rank: Vec<u16>,
    packable: bool,
    pub dump: bool,
    pub cat_names: Vec<Box<[u8]>>,
    pub camp_ids: Vec<Box<[u8]>>,
    max_cat: usize,
}

impl Ctx {
    pub fn new(base_ms: i64, ws_ms: i64, we_ms: i64, cat_names: &[Box<[u8]>], camp_ids: &[Box<[u8]>], dump: bool) -> Ctx {
        let ncamp = camp_ids.len();
        let ncat = cat_names.len();
        // day(rel) = floor((base_ms + rel) / DAY) = base_day + (base_rem + rel) / DAY
        let base_day = base_ms.div_euclid(DAY_MS);
        let base_rem = (base_ms - base_day * DAY_MS) as u64;
        let day0 = ws_ms.div_euclid(DAY_MS) as i32;
        let ndays = ((we_ms - 1).div_euclid(DAY_MS) as i32 - day0 + 1).max(1) as usize;
        // category rank by name (for byte-wise smallest tie-break)
        let mut order: Vec<u16> = (0..ncat as u16).collect();
        order.sort_by(|&a, &b| cat_names[a as usize].cmp(&cat_names[b as usize]));
        let mut rank = vec![0u16; ncat];
        for (r, &k) in order.iter().enumerate() {
            rank[k as usize] = r as u16;
        }
        // packed u64 fast path limits (else generic structs)
        let max_ts = (we_ms - base_ms).max(0) as u64;
        let packable = std::env::var_os("FAST_GENERIC").is_none()
            && VARIANT.load(Relaxed) != 1
            && ncat <= 512
            && ncamp + 1 <= 4096
            && max_ts < (1u64 << 31);
        Ctx {
            base_ms,
            ncamp,
            ncat,
            organic: ncamp as u16,
            base_day,
            base_rem,
            day0,
            ndays,
            rank,
            packable,
            dump,
            cat_names: cat_names.to_vec(),
            camp_ids: camp_ids.to_vec(),
            max_cat: cat_names.iter().map(|c| c.len()).max().unwrap_or(0),
        }
    }
    #[inline(always)]
    fn day_of(&self, rel: u32) -> i32 {
        (self.base_day + ((self.base_rem + rel as u64) / DAY_MS as u64) as i64) as i32
    }
    #[inline]
    fn pick_top(&self, sums: &[u64], touched: &[u16]) -> Option<u16> {
        let mut best: Option<u16> = None;
        for &k in touched {
            best = match best {
                None => Some(k),
                Some(b) => {
                    let (s, sb) = (sums[k as usize], sums[b as usize]);
                    if s > sb || (s == sb && self.rank[k as usize] < self.rank[b as usize]) {
                        Some(k)
                    } else {
                        Some(b)
                    }
                }
            };
        }
        best
    }
}

/// Per-thread back-half state: feed it parts, then `finish`.
pub struct Worker {
    s: Scratch,
}

/// What one worker hands back.
pub struct WorkerOut {
    pub urows: usize,
    /// camp-major [k * ndays + di]
    pub camp: Vec<CampAcc>,
    pub dump: Vec<u8>,
}

impl Worker {
    pub fn new(ctx: &Ctx) -> Worker {
        let (ncamp, ncat, ndays) = (ctx.ncamp, ctx.ncat, ctx.ndays);
        Worker {
            s: Scratch {
                cnt: vec![0; ncamp],
                since: vec![0; ncamp],
                active: vec![],
                pos: vec![0; ncamp],
                catsum: vec![0; ncat],
                cat_touched: vec![],
                sess_cat: vec![0; ncat],
                sess_touched: vec![],
                camp: vec![CampAcc::default(); (ncamp + 1) * ndays],
                asst: vec![0; (ncamp + 1) * ndays],
                ncamp1: ncamp + 1,
                ucols: pq::user_cols(),
                urows: 0,
                dump: vec![],
                eoff: vec![],
                coff: vec![],
                evs: vec![],
                cls: vec![],
                pevs: vec![],
                pcls: vec![],
                rtmp: vec![],
            },
        }
    }

    /// Sessionize/attribute/aggregate every person of one part.
    pub fn part(&mut self, ctx: &Ctx, part: &Part) {
        let s = &mut self.s;
        let np = part.person_names.len();
        let t0 = std::time::Instant::now();
        let variant = VARIANT.load(Relaxed);
        if ctx.packable && part.cl.len() < (1 << 21) {
            group(&part.ev, np, |e| e.person, |_, e| PEv(((e.ts as u64) << 33) | ((e.price as u64) << 9) | e.cat as u64), &mut s.eoff, &mut s.pevs);
            group(&part.cl, np, |e| e.person, |i, e| PCl(((e.ts as u64) << 33) | ((e.camp as u64) << 21) | i as u64), &mut s.coff, &mut s.pcls);
            let mut evs = std::mem::take(&mut s.pevs);
            let mut cls = std::mem::take(&mut s.pcls);
            T_GROUP.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
            for p in 0..np {
                if variant == 3 {
                    break;
                }
                let (ea, eb) = (s.eoff[p] as usize, s.eoff[p + 1] as usize);
                if ea == eb {
                    continue;
                }
                let (ca, cb) = (s.coff[p] as usize, s.coff[p + 1] as usize);
                let pe = &mut evs[ea..eb];
                let pc = &mut cls[ca..cb];
                // SAFETY: PEv/PCl are single-u64 newtypes
                sort_ts(unsafe { std::slice::from_raw_parts_mut(pe.as_mut_ptr() as *mut u64, pe.len()) }, &mut s.rtmp);
                sort_ts(unsafe { std::slice::from_raw_parts_mut(pc.as_mut_ptr() as *mut u64, pc.len()) }, &mut s.rtmp);
                if variant == 2 {
                    continue;
                }
                process_person(&part.person_names[p], pe, pc, part, s, ctx);
            }
            s.pevs = evs;
            s.pcls = cls;
        } else {
            group(&part.ev, np, |e| e.person, |_, e| *e, &mut s.eoff, &mut s.evs);
            group(&part.cl, np, |e| e.person, |_, e| *e, &mut s.coff, &mut s.cls);
            let mut evs = std::mem::take(&mut s.evs);
            let mut cls = std::mem::take(&mut s.cls);
            for p in 0..np {
                let (ea, eb) = (s.eoff[p] as usize, s.eoff[p + 1] as usize);
                if ea == eb {
                    continue;
                }
                let (ca, cb) = (s.coff[p] as usize, s.coff[p + 1] as usize);
                let pe = &mut evs[ea..eb];
                let pc = &mut cls[ca..cb];
                pe.sort_unstable_by_key(|e| e.ts);
                pc.sort_unstable_by_key(|x| x.ts);
                process_person(&part.person_names[p], pe, pc, part, s, ctx);
            }
            s.evs = evs;
            s.cls = cls;
        }
        let el = t0.elapsed().as_nanos() as u64;
        T_PROC.fetch_add(el, Relaxed);
        T_MAXPART.fetch_max(el, Relaxed);
    }

    /// Write this worker's user_daily rows (if `user_dir`) and return its campaign accumulators.
    pub fn finish(self, ctx: &Ctx, w: usize, user_dir: Option<&Path>) -> WorkerOut {
        let mut sc = self.s;
        if let Some(dir) = user_dir {
            if sc.urows > 0 || w == 0 {
                let bytes = pq::encode(&sc.ucols, sc.urows);
                std::fs::write(dir.join(format!("part-{w}.parquet")), bytes).expect("write user_daily");
            }
        }
        let (ncamp, ndays) = (ctx.ncamp, ctx.ndays);
        // convert day-major scratch into camp-major output layout
        let mut cm = vec![CampAcc::default(); (ncamp + 1) * ndays];
        for di in 0..ndays {
            for k in 0..=ncamp {
                let mut a = sc.camp[di * (ncamp + 1) + k];
                a.assisted = sc.asst[di * (ncamp + 1) + k] as u64;
                cm[k * ndays + di] = a;
            }
        }
        WorkerOut { urows: sc.urows, camp: cm, dump: std::mem::take(&mut sc.dump) }
    }
}

/// Merge worker outputs.
pub fn merge(ctx: &Ctx, results: Vec<WorkerOut>) -> Output {
    let mut camp = vec![CampAcc::default(); (ctx.ncamp + 1) * ctx.ndays];
    let mut user_rows = 0;
    let mut dumps = vec![];
    for r in results {
        for (x, y) in camp.iter_mut().zip(r.camp.iter()) {
            x.sessions += y.sessions;
            x.purchasing += y.purchasing;
            x.revenue += y.revenue;
            x.assisted += y.assisted;
        }
        user_rows += r.urows;
        dumps.push(r.dump);
    }
    if std::env::var("FAST_TIMING").is_ok() {
        eprintln!("engine: group {}ms parts {}ms (thread-summed) max part {}ms", T_GROUP.load(Relaxed) / 1000000, T_PROC.load(Relaxed) / 1000000, T_MAXPART.load(Relaxed) / 1000000);
    }
    Output { day0: ctx.day0, ndays: ctx.ndays, camp, user_rows, session_dump: if ctx.dump { Some(dumps) } else { None } }
}

/// Runs the back half over an already-built `Cleaned`. If `user_dir` is given, each
/// worker writes its user_daily rows to `user_dir/part-<w>.parquet`.
pub fn run(c: &Cleaned, dump: bool, user_dir: Option<&Path>) -> Output {
    let ctx = Ctx::new(c.base_ms, c.ws_ms, c.we_ms, &c.cat_names, &c.camp_ids, dump);
    // process largest parts first for load balance
    let mut order: Vec<usize> = (0..c.parts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(c.parts[i].ev.len() + c.parts[i].cl.len()));
    let nthreads = crate::threads();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let ctx_ref = &ctx;
    let worker = |w: usize| {
        let tw = std::time::Instant::now();
        let mut wk = Worker::new(ctx_ref);
        loop {
            let oi = next.fetch_add(1, Relaxed);
            if oi >= order.len() {
                break;
            }
            wk.part(ctx_ref, &c.parts[order[oi]]);
        }
        T_WORKER_MAX.fetch_max(tw.elapsed().as_nanos() as u64, Relaxed);
        wk.finish(ctx_ref, w, user_dir)
    };
    let results: Vec<WorkerOut> = std::thread::scope(|scope| {
        let worker = &worker;
        let hs: Vec<_> = (0..nthreads).map(|w| scope.spawn(move || worker(w))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    merge(&ctx, results)
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn process_person<E: EvT, C: ClT>(
    name: &[u8],
    pe: &[E],
    pc: &[C],
    part: &Part,
    s: &mut Scratch,
    ctx: &Ctx,
) {
    let (organic, day0, dump) = (ctx.organic, ctx.day0, ctx.dump);
    let mut lo = 0usize;
    let mut hi = 0usize;
    // current user-day accumulator
    let mut cur_day: i32 = i32::MIN;
    let mut d_sess = 0u32;
    let mut d_ev = 0u32;
    let mut d_rev = 0u64;

    let mut i = 0usize;
    while i < pe.len() {
        let start = pe[i].ts();
        let day = ctx.day_of(start);
        // user-day aggregation: flush previous day before accumulating this session
        if day != cur_day {
            if cur_day != i32::MIN {
                flush_day(name, cur_day, d_sess, d_ev, d_rev, s, ctx);
                // assists: close out the old day for every active campaign
                let row = (cur_day - day0) as usize * s.ncamp1;
                for &k in &s.active {
                    s.asst[row + k as usize] += d_sess - s.since[k as usize];
                    s.since[k as usize] = 0;
                }
            }
            cur_day = day;
            d_sess = 0;
            d_ev = 0;
            d_rev = 0;
        }
        let mut prev = start;
        let mut n = 0u32;
        let mut rev = 0u64;
        let mut j = i;
        while j < pe.len() {
            let e = pe[j];
            if e.ts() - prev > GAP_MS {
                break;
            }
            prev = e.ts();
            n += 1;
            if e.price() > 0 {
                rev += e.price() as u64;
                let k = e.cat() as usize;
                if s.catsum[k] == 0 {
                    s.cat_touched.push(e.cat());
                }
                s.catsum[k] += e.price() as u64;
                if dump {
                    if s.sess_cat[k] == 0 {
                        s.sess_touched.push(e.cat());
                    }
                    s.sess_cat[k] += e.price() as u64;
                }
            }
            j += 1;
        }
        let purchased = rev > 0; // every purchase has price >= 1
        i = j;

        // attribution window [start - 7d, start]
        while hi < pc.len() && pc[hi].ts() <= start {
            let k = pc[hi].camp() as usize;
            if s.cnt[k] == 0 {
                s.pos[k] = s.active.len() as u32;
                s.active.push(pc[hi].camp());
                s.since[k] = d_sess; // assists this and later sessions of the day
            }
            s.cnt[k] += 1;
            hi += 1;
        }
        let wlo = start - WEEK_MS; // start >= window_start = base + 7d
        while lo < hi && pc[lo].ts() < wlo {
            let k = pc[lo].camp() as usize;
            s.cnt[k] -= 1;
            if s.cnt[k] == 0 {
                // assisted the day's sessions [since, d_sess)
                s.asst[(day - day0) as usize * s.ncamp1 + k] += d_sess - s.since[k];
                let ps = s.pos[k] as usize;
                let last = *s.active.last().unwrap();
                s.active[ps] = last;
                s.pos[last as usize] = ps as u32;
                s.active.pop();
            }
            lo += 1;
        }
        let credited = if hi > lo {
            // latest ts; among equal ts the smallest click_id
            let t = pc[hi - 1].ts();
            let mut best = hi - 1;
            let mut q = hi - 1;
            while q > lo && pc[q - 1].ts() == t {
                q -= 1;
                if pc[q].cid(part) < pc[best].cid(part) {
                    best = q;
                }
            }
            pc[best].camp()
        } else {
            organic
        };

        let di = (day - day0) as usize;
        debug_assert!(di < ctx.ndays);
        let row = di * s.ncamp1;
        let ca = &mut s.camp[row + credited as usize];
        ca.sessions += 1;
        ca.purchasing += purchased as u64;
        ca.revenue += rev;

        if dump {
            let top = ctx.pick_top(&s.sess_cat, &s.sess_touched);
            for &k in &s.sess_touched {
                s.sess_cat[k as usize] = 0;
            }
            s.sess_touched.clear();
            let mut assists: Vec<&[u8]> = s.active.iter().map(|&k| &*ctx.camp_ids[k as usize]).collect();
            assists.sort();
            let d = &mut s.dump;
            d.extend_from_slice(name);
            d.extend_from_slice(format!(",{},{},{},", ctx.base_ms + start as i64, n, rev).as_bytes());
            if let Some(t) = top {
                d.extend_from_slice(&ctx.cat_names[t as usize]);
            }
            d.push(b',');
            if credited == organic {
                d.extend_from_slice(b"organic");
            } else {
                d.extend_from_slice(&ctx.camp_ids[credited as usize]);
            }
            d.push(b',');
            d.extend_from_slice(&assists.join(&b";"[..]));
            d.push(b'\n');
        }

        d_sess += 1;
        d_ev += n;
        d_rev += rev;
    }
    if cur_day != i32::MIN {
        flush_day(name, cur_day, d_sess, d_ev, d_rev, s, ctx);
    }
    // close out assists of the last day and reset attribution window
    let row = (cur_day - day0) as usize * s.ncamp1;
    for &k in &s.active {
        s.asst[row + k as usize] += d_sess - s.since[k as usize];
        s.cnt[k as usize] = 0;
        s.since[k as usize] = 0;
    }
    s.active.clear();
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn flush_day(
    name: &[u8],
    day: i32,
    sessions: u32,
    events: u32,
    revenue: u64,
    s: &mut Scratch,
    ctx: &Ctx,
) {
    let top = ctx.pick_top(&s.catsum, &s.cat_touched);
    for &k in &s.cat_touched {
        s.catsum[k as usize] = 0;
    }
    s.cat_touched.clear();
    let u = &mut s.ucols;
    u[0].push_str(name);
    u[1].push_i32(day);
    u[2].push_i64(sessions as i64);
    u[3].push_i64(events as i64);
    u[4].push_i64(revenue as i64);
    match top {
        Some(t) => u[5].push_str(&ctx.cat_names[t as usize]),
        None => u[5].push_null(),
    }
    s.urows += 1;
}
