//! Back half: group by person, sessionize, attribute clicks, 7-day assists,
//! aggregate into user_daily / campaign_daily rows.
use crate::model::*;
use rayon::prelude::*;
use std::cell::UnsafeCell;

#[derive(Clone, Copy)]
#[repr(C)]
struct Ev {
    ts: u32,
    price: u32,
    cat: u16,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct Cl {
    ts: u32,
    idx: u32,
    camp: u16,
}

pub struct UserRow {
    pub person: u32,
    pub day: i32,
    pub sessions: u32,
    pub events: u32,
    pub revenue: u64,
    pub top_cat: Option<u16>,
}

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
    pub user_rows: Vec<Vec<UserRow>>,
    /// [(ncamp+1) * ndays], last campaign index = organic
    pub camp: Vec<CampAcc>,
    pub session_dump: Option<Vec<Vec<u8>>>,
}

struct SyncPtr<T>(UnsafeCell<*mut T>);
unsafe impl<T> Sync for SyncPtr<T> {}
impl<T> SyncPtr<T> {
    fn new(p: *mut T) -> Self {
        SyncPtr(UnsafeCell::new(p))
    }
    #[inline]
    fn get(&self) -> *mut T {
        unsafe { *self.0.get() }
    }
    #[inline]
    unsafe fn write(&self, i: usize, v: T) {
        (*self.0.get()).add(i).write(v)
    }
}

/// Counting sort of `n` items by `key(i)` in [0, nkeys). Returns (offsets[nkeys+1], items).
fn group_by<T: Copy + Send + Sync>(
    n: usize,
    nkeys: usize,
    key: impl Fn(usize) -> u32 + Sync,
    item: impl Fn(usize) -> T + Sync,
) -> (Vec<u32>, Vec<T>) {
    let nt = rayon::current_num_threads().max(1);
    let nchunks = if n < 1 << 16 { 1 } else { nt };
    let chunk = n.div_ceil(nchunks).max(1);
    let ranges: Vec<(usize, usize)> = (0..nchunks).map(|c| (c * chunk, ((c + 1) * chunk).min(n))).collect();
    let mut hists: Vec<Vec<u32>> = ranges
        .par_iter()
        .map(|&(a, b)| {
            let mut h = vec![0u32; nkeys];
            for i in a..b {
                h[key(i) as usize] += 1;
            }
            h
        })
        .collect();
    // offsets: key-major, chunk-minor
    let mut offs = vec![0u32; nkeys + 1];
    let mut acc = 0u32;
    for k in 0..nkeys {
        offs[k] = acc;
        for h in hists.iter_mut() {
            let c = h[k];
            h[k] = acc;
            acc += c;
        }
    }
    offs[nkeys] = acc;
    let mut out: Vec<T> = Vec::with_capacity(n);
    let p = SyncPtr::new(out.as_mut_ptr());
    ranges.par_iter().zip(hists.par_iter_mut()).for_each(|(&(a, b), h)| {
        for i in a..b {
            let k = key(i) as usize;
            let pos = h[k];
            h[k] = pos + 1;
            unsafe { p.write(pos as usize, item(i)) };
        }
    });
    unsafe { out.set_len(n) };
    (offs, out)
}

struct Scratch {
    cnt: Vec<u32>,
    active: Vec<u16>,
    pos: Vec<u32>,
    catsum: Vec<u64>,
    cat_touched: Vec<u16>,
    sess_cat: Vec<u64>,
    sess_touched: Vec<u16>,
    camp: Vec<CampAcc>,
    rows: Vec<UserRow>,
    dump: Vec<u8>,
}

pub fn run(c: &Cleaned, dump: bool) -> Output {
    let np = c.person_names.len();
    let ncamp = c.camp_ids.len();
    let ncat = c.cat_names.len();
    let organic = ncamp as u16;
    let day_of = |rel: u32| -> i32 { (c.base_ms + rel as i64).div_euclid(DAY_MS) as i32 };
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

    let (eoff, mut evs) = group_by(
        c.ev_ts.len(),
        np,
        |i| c.ev_person[i],
        |i| Ev { ts: c.ev_ts[i], price: c.ev_price[i], cat: c.ev_cat[i] },
    );
    let (coff, mut cls) = group_by(
        c.cl_ts.len(),
        np,
        |i| c.cl_person[i],
        |i| Cl { ts: c.cl_ts[i], idx: i as u32, camp: c.cl_camp[i] },
    );

    // person chunks balanced by event count
    let total = evs.len() + cls.len();
    let target = (total / 512).max(4096);
    let mut chunks: Vec<(usize, usize)> = vec![];
    let mut a = 0usize;
    let mut acc = 0usize;
    for p in 0..np {
        acc += (eoff[p + 1] - eoff[p]) as usize + (coff[p + 1] - coff[p]) as usize + 1;
        if acc >= target {
            chunks.push((a, p + 1));
            a = p + 1;
            acc = 0;
        }
    }
    if a < np {
        chunks.push((a, np));
    }

    let evs_p = SyncPtr::new(evs.as_mut_ptr());
    let cls_p = SyncPtr::new(cls.as_mut_ptr());
    let new_scratch = || Scratch {
        cnt: vec![0; ncamp],
        active: vec![],
        pos: vec![0; ncamp],
        catsum: vec![0; ncat],
        cat_touched: vec![],
        sess_cat: vec![0; ncat],
        sess_touched: vec![],
        camp: vec![CampAcc::default(); (ncamp + 1) * ndays],
        rows: vec![],
        dump: vec![],
    };

    let results: Vec<(Vec<UserRow>, Vec<CampAcc>, Vec<u8>)> = chunks
        .par_iter()
        .fold(
            || (vec![], None::<Scratch>),
            |(mut out, mut sc), &(pa, pb)| {
                let s = sc.get_or_insert_with(new_scratch);
                for p in pa..pb {
                    let (ea, eb) = (eoff[p] as usize, eoff[p + 1] as usize);
                    if ea == eb {
                        continue;
                    }
                    // SAFETY: person ranges are disjoint across tasks.
                    let pe = unsafe { std::slice::from_raw_parts_mut(evs_p.get().add(ea), eb - ea) };
                    let (ca, cb) = (coff[p] as usize, coff[p + 1] as usize);
                    let pc = unsafe { std::slice::from_raw_parts_mut(cls_p.get().add(ca), cb - ca) };
                    pe.sort_unstable_by_key(|e| e.ts);
                    pc.sort_unstable_by(|x, y| {
                        x.ts.cmp(&y.ts).then_with(|| c.cl_id[x.idx as usize].cmp(&c.cl_id[y.idx as usize]))
                    });
                    process_person(p as u32, pe, pc, s, organic, ndays, day0, &day_of, &pick_top, dump, c);
                }
                out.push(std::mem::take(&mut s.rows));
                (out, sc)
            },
        )
        .map(|(rows, sc)| {
            let mut sc = sc.unwrap_or_else(new_scratch);
            let flat: Vec<UserRow> = rows.into_iter().flatten().collect();
            (flat, std::mem::take(&mut sc.camp), std::mem::take(&mut sc.dump))
        })
        .collect();

    let mut camp = vec![CampAcc::default(); (ncamp + 1) * ndays];
    let mut user_rows = vec![];
    let mut dumps = vec![];
    for (rows, cacc, d) in results {
        for (x, y) in camp.iter_mut().zip(cacc.iter()) {
            x.sessions += y.sessions;
            x.purchasing += y.purchasing;
            x.revenue += y.revenue;
            x.assisted += y.assisted;
        }
        user_rows.push(rows);
        dumps.push(d);
    }
    Output { day0, ndays, user_rows, camp, session_dump: if dump { Some(dumps) } else { None } }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn process_person(
    p: u32,
    pe: &[Ev],
    pc: &[Cl],
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
        let start = pe[i].ts;
        let day = day_of(start);
        // user-day aggregation: flush previous day before accumulating this session
        if day != cur_day {
            if cur_day != i32::MIN {
                flush_day(p, cur_day, d_sess, d_ev, d_rev, s, pick_top);
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
            if e.ts - prev > GAP_MS {
                break;
            }
            prev = e.ts;
            n += 1;
            if e.price > 0 {
                rev += e.price as u64;
                let k = e.cat as usize;
                if s.catsum[k] == 0 {
                    s.cat_touched.push(e.cat);
                }
                s.catsum[k] += e.price as u64;
                if dump {
                    if s.sess_cat[k] == 0 {
                        s.sess_touched.push(e.cat);
                    }
                    s.sess_cat[k] += e.price as u64;
                }
            }
            j += 1;
        }
        let purchased = j > i && pe[i..j].iter().any(|e| e.price > 0);
        i = j;

        // attribution window [start - 7d, start]
        while hi < pc.len() && pc[hi].ts <= start {
            let k = pc[hi].camp as usize;
            if s.cnt[k] == 0 {
                s.pos[k] = s.active.len() as u32;
                s.active.push(pc[hi].camp);
            }
            s.cnt[k] += 1;
            hi += 1;
        }
        let wlo = start - WEEK_MS; // start >= window_start = base + 7d
        while lo < hi && pc[lo].ts < wlo {
            let k = pc[lo].camp as usize;
            s.cnt[k] -= 1;
            if s.cnt[k] == 0 {
                let ps = s.pos[k] as usize;
                let last = *s.active.last().unwrap();
                s.active[ps] = last;
                s.pos[last as usize] = ps as u32;
                s.active.pop();
            }
            lo += 1;
        }
        let credited = if hi > lo {
            let t = pc[hi - 1].ts;
            let mut q = hi - 1;
            while q > lo && pc[q - 1].ts == t {
                q -= 1;
            }
            pc[q].camp
        } else {
            organic
        };

        let di = (day - day0) as usize;
        debug_assert!(di < ndays);
        let ca = &mut s.camp[credited as usize * ndays + di];
        ca.sessions += 1;
        ca.purchasing += purchased as u64;
        ca.revenue += rev;
        for &k in &s.active {
            s.camp[k as usize * ndays + di].assisted += 1;
        }

        if dump {
            let top = pick_top(&s.sess_cat, &s.sess_touched);
            for &k in &s.sess_touched {
                s.sess_cat[k as usize] = 0;
            }
            s.sess_touched.clear();
            let mut assists: Vec<&[u8]> = s.active.iter().map(|&k| &*c.camp_ids[k as usize]).collect();
            assists.sort();
            let d = &mut s.dump;
            d.extend_from_slice(&c.person_names[p as usize]);
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
        flush_day(p, cur_day, d_sess, d_ev, d_rev, s, pick_top);
    }
    // reset attribution window
    for &k in &s.active {
        s.cnt[k as usize] = 0;
    }
    s.active.clear();
}

#[inline]
fn flush_day(
    p: u32,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: u64,
    s: &mut Scratch,
    pick_top: &impl Fn(&[u64], &[u16]) -> Option<u16>,
) {
    let top = pick_top(&s.catsum, &s.cat_touched);
    for &k in &s.cat_touched {
        s.catsum[k as usize] = 0;
    }
    s.cat_touched.clear();
    s.rows.push(UserRow { person: p, day, sessions, events, revenue, top_cat: top });
}
