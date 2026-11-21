//! Back half: group by person, sessionize, attribute clicks, 7-day assists,
//! aggregate into user_daily / campaign_daily rows.
use crate::model::*;
use rayon::prelude::*;

pub struct UserRow {
    pub part: u32,
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
    cur_person: u32,
    eoff: Vec<u32>,
    coff: Vec<u32>,
    evs: Vec<Ev>,
    cls: Vec<Cl>,
}

/// counting sort of items by person into `out`, offsets into `off` (len np+1)
fn group<T: Copy>(items: &[T], np: usize, key: impl Fn(&T) -> u32, off: &mut Vec<u32>, out: &mut Vec<T>) {
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
        for it in items {
            let k = key(it) as usize;
            o.add(cur[k] as usize).write(*it);
            cur[k] += 1;
        }
        out.set_len(items.len());
    }
}

pub fn run(c: &Cleaned, dump: bool) -> Output {
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
        cur_person: 0,
        eoff: vec![],
        coff: vec![],
        evs: vec![],
        cls: vec![],
    };

    // process largest parts first for load balance
    let mut order: Vec<usize> = (0..c.parts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(c.parts[i].ev.len() + c.parts[i].cl.len()));

    let results: Vec<(Vec<Vec<UserRow>>, Vec<CampAcc>, Vec<u8>)> = order
        .par_iter()
        .fold(
            || (vec![], None::<Scratch>),
            |(mut out, mut sc), &pi| {
                let s = sc.get_or_insert_with(new_scratch);
                let part = &c.parts[pi];
                let np = part.person_names.len();
                group(&part.ev, np, |e| e.person, &mut s.eoff, &mut s.evs);
                group(&part.cl, np, |e| e.person, &mut s.coff, &mut s.cls);
                let mut evs = std::mem::take(&mut s.evs);
                let mut cls = std::mem::take(&mut s.cls);
                let eoff = std::mem::take(&mut s.eoff);
                let coff = std::mem::take(&mut s.coff);
                for p in 0..np {
                    let (ea, eb) = (eoff[p] as usize, eoff[p + 1] as usize);
                    if ea == eb {
                        continue;
                    }
                    let pe = &mut evs[ea..eb];
                    let (ca, cb) = (coff[p] as usize, coff[p + 1] as usize);
                    let pc = &mut cls[ca..cb];
                    pe.sort_unstable_by_key(|e| e.ts);
                    pc.sort_unstable_by_key(|x| (x.ts, x.click_id));
                    s.cur_person = p as u32;
                    process_person(&part.person_names[p], pe, pc, s, organic, ndays, day0, &day_of, &pick_top, dump, c);
                }
                s.evs = evs;
                s.cls = cls;
                s.eoff = eoff;
                s.coff = coff;
                // tag rows with part so writer can find names
                out.push(std::mem::take(&mut s.rows).into_iter().map(|mut r| { r.part = pi as u32; r }).collect());
                (out, sc)
            },
        )
        .map(|(rows, sc)| {
            let mut sc = sc.unwrap_or_else(new_scratch);
            (rows, std::mem::take(&mut sc.camp), std::mem::take(&mut sc.dump))
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
        user_rows.extend(rows);
        dumps.push(d);
    }
    Output { day0, ndays, user_rows, camp, session_dump: if dump { Some(dumps) } else { None } }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn process_person(
    name: &[u8],
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
                flush_day(cur_day, d_sess, d_ev, d_rev, s, pick_top);
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
        flush_day(cur_day, d_sess, d_ev, d_rev, s, pick_top);
    }
    // reset attribution window
    for &k in &s.active {
        s.cnt[k as usize] = 0;
    }
    s.active.clear();
}

#[inline]
fn flush_day(
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
    s.rows.push(UserRow { part: 0, person: s.cur_person, day, sessions, events, revenue, top_cat: top });
}
