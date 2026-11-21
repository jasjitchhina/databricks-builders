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

/// Runs the back half. If `user_dir` is given, each worker writes its user_daily rows
/// to `user_dir/part-<w>.parquet`.
pub fn run(c: &Cleaned, dump: bool, user_dir: Option<&Path>) -> Output {
    let ncamp = c.camp_ids.len();
    let ncat = c.cat_names.len();
    let organic = ncamp as u16;
    // day(rel) = floor((base_ms + rel) / DAY); with base_day = floor(base_ms/DAY) and
    // base_rem = base_ms - base_day*DAY in [0, DAY): day = base_day + (base_rem + rel) / DAY (u64 math)
    let base_day = c.base_ms.div_euclid(DAY_MS);
    let base_rem = (c.base_ms - base_day * DAY_MS) as u64;
    let day_of = move |rel: u32| -> i32 { (base_day + ((base_rem + rel as u64) / DAY_MS as u64) as i64) as i32 };
    let day0 = c.ws_ms.div_euclid(DAY_MS) as i32;
    let ndays = ((c.we_ms - 1).div_euclid(DAY_MS) as i32 - day0 + 1).max(1) as usize;

    // category rank by name (for byte-wise smallest tie-break)
    let mut order: Vec<u16> = (0..ncat as u16).collect();
    order.sort_by(|&a, &b| c.cat_names[a as usize].cmp(&c.cat_names[b as usize]));
    let mut rank = vec![0u16; ncat];
    for (r, &k) in order.iter().enumerate() {
        rank[k as usize] = r as u16;
    }
    let pick_top = |sums: &[u64], touched: &[u16]| -> Option<u16> {
        let mut best: Option<u16> = None;
        for &k in touched {
            let s = sums[k as usize];
            best = match best {
                None => Some(k),
                Some(b) => {
                    let sb = sums[b as usize];
                    if s > sb || (s == sb && rank[k as usize] < rank[b as usize]) {
                        Some(k)
                    } else {
                        Some(b)
                    }
                }
            };
        }
        best
    };

    let new_scratch = || Scratch {
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
    };

    // packed u64 fast path limits (else generic structs)
    let max_ts = (c.we_ms - c.base_ms).max(0) as u64;
    let packable = std::env::var_os("FAST_GENERIC").is_none() && VARIANT.load(Relaxed) != 1 && ncat <= 512 && ncamp + 1 <= 4096 && max_ts < (1u64 << 31);

    // process largest parts first for load balance
    let mut order: Vec<usize> = (0..c.parts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(c.parts[i].ev.len() + c.parts[i].cl.len()));

    let nthreads = crate::threads();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let worker = |w: usize| {
        let tw = std::time::Instant::now();
        let mut sc = new_scratch();
        let s = &mut sc;
        loop {
            let oi = next.fetch_add(1, Relaxed);
            if oi >= order.len() {
                break;
            }
            let pi = order[oi];
            let part = &c.parts[pi];
            let np = part.person_names.len();
            let t0 = std::time::Instant::now();
            let variant = VARIANT.load(Relaxed);
            if packable && part.cl.len() < (1 << 21) {
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
                    // SAFETY: PEv/PCl are repr(transparent)-like newtypes over u64
                    unsafe { std::slice::from_raw_parts_mut(pe.as_mut_ptr() as *mut u64, pe.len()) }.sort_unstable();
                    unsafe { std::slice::from_raw_parts_mut(pc.as_mut_ptr() as *mut u64, pc.len()) }.sort_unstable();
                    if variant == 2 {
                        continue;
                    }
                    process_person(&part.person_names[p], pe, pc, part, s, organic, ndays, day0, &day_of, &pick_top, dump, c);
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
                    process_person(&part.person_names[p], pe, pc, part, s, organic, ndays, day0, &day_of, &pick_top, dump, c);
                }
                s.evs = evs;
                s.cls = cls;
            }
            let el = t0.elapsed().as_nanos() as u64;
            T_PROC.fetch_add(el, Relaxed);
            T_MAXPART.fetch_max(el, Relaxed);
        }
        T_WORKER_MAX.fetch_max(tw.elapsed().as_nanos() as u64, Relaxed);
        if let Some(dir) = user_dir {
            if sc.urows > 0 || w == 0 {
                let bytes = pq::encode(&sc.ucols, sc.urows);
                std::fs::write(dir.join(format!("part-{w}.parquet")), bytes).expect("write user_daily");
            }
        }
        // convert day-major scratch into camp-major output layout
        let mut cm = vec![CampAcc::default(); (ncamp + 1) * ndays];
        for di in 0..ndays {
            for k in 0..=ncamp {
                let mut a = sc.camp[di * (ncamp + 1) + k];
                a.assisted = sc.asst[di * (ncamp + 1) + k] as u64;
                cm[k * ndays + di] = a;
            }
        }
        (sc.urows, cm, std::mem::take(&mut sc.dump))
    };
    let results: Vec<(usize, Vec<CampAcc>, Vec<u8>)> =
        std::thread::scope(|scope| {
            let worker = &worker;
            let hs: Vec<_> = (0..nthreads).map(|w| scope.spawn(move || worker(w))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });

    let tm = std::time::Instant::now();
    let mut camp = vec![CampAcc::default(); (ncamp + 1) * ndays];
    let mut user_rows = 0;
    let mut dumps = vec![];
    for (rows, cacc, d) in results {
        for (x, y) in camp.iter_mut().zip(cacc.iter()) {
            x.sessions += y.sessions;
            x.purchasing += y.purchasing;
            x.revenue += y.revenue;
            x.assisted += y.assisted;
        }
        user_rows += rows;
        dumps.push(d);
    }
    if std::env::var("FAST_TIMING").is_ok() {
        eprintln!("engine: sort {}ms group {}ms proc+sort {}ms (thread-summed) merge {:?} nresults {}", T_SORT.load(Relaxed)/1000000, T_GROUP.load(Relaxed)/1000000, T_PROC.load(Relaxed)/1000000, tm.elapsed(), dumps.len());
    }
    Output { day0, ndays, camp, user_rows, session_dump: if dump { Some(dumps) } else { None } }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn process_person<E: EvT, C: ClT>(
    name: &[u8],
    pe: &[E],
    pc: &[C],
    part: &Part,
    s: &mut Scratch,
    organic: u16,
    ndays: usize,
    day0: i32,
    day_of: &impl Fn(u32) -> i32,
    pick_top: &impl Fn(&[u64], &[u16]) -> Option<u16>,
    dump: bool,
    c: &Cleaned,
) {
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
        let day = day_of(start);
        // user-day aggregation: flush previous day before accumulating this session
        if day != cur_day {
            if cur_day != i32::MIN {
                flush_day(name, cur_day, d_sess, d_ev, d_rev, s, pick_top, c);
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
        debug_assert!(di < ndays);
        let row = di * s.ncamp1;
        let ca = &mut s.camp[row + credited as usize];
        ca.sessions += 1;
        ca.purchasing += purchased as u64;
        ca.revenue += rev;

        if dump {
            let top = pick_top(&s.sess_cat, &s.sess_touched);
            for &k in &s.sess_touched {
                s.sess_cat[k as usize] = 0;
            }
            s.sess_touched.clear();
            let mut assists: Vec<&[u8]> = s.active.iter().map(|&k| &*c.camp_ids[k as usize]).collect();
            assists.sort();
            let d = &mut s.dump;
            d.extend_from_slice(name);
            d.extend_from_slice(format!(",{},{},{},", c.base_ms + start as i64, n, rev).as_bytes());
            if let Some(t) = top {
                d.extend_from_slice(&c.cat_names[t as usize]);
            }
            d.push(b',');
            if credited == organic {
                d.extend_from_slice(b"organic");
            } else {
                d.extend_from_slice(&c.camp_ids[credited as usize]);
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
        flush_day(name, cur_day, d_sess, d_ev, d_rev, s, pick_top, c);
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
    pick_top: &impl Fn(&[u64], &[u16]) -> Option<u16>,
    c: &Cleaned,
) {
    let top = pick_top(&s.catsum, &s.cat_touched);
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
        Some(t) => u[5].push_str(&c.cat_names[t as usize]),
        None => u[5].push_null(),
    }
    s.urows += 1;
}
