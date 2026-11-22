//! semstats DATA_DIR: independent rules.md reference counts for the H6 semantic
//! shortcuts audit (which rows/fields can change the outputs). IDs are hashed to
//! 64 bits (stats only, never used for output). Slow and simple on purpose.
use fast::csv;
use fast::ts::parse_ts;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::hash::Hasher;
use std::path::Path;

const DAY: i64 = 86_400_000;
const WEEK: i64 = 7 * DAY;
const GAP: i64 = 1_800_000;

fn h(b: &[u8]) -> u64 {
    let mut x = FxHasher::default();
    x.write(b);
    x.write_u8(0xff);
    // second mix so short ids do not collide trivially
    let v = x.finish();
    (v ^ (v >> 29)).wrapping_mul(0xbf58476d1ce4e5b9) ^ (b.len() as u64) << 56
}

fn lines(p: &Path, mut f: impl FnMut(&[u8])) {
    let b = std::fs::read(p).unwrap();
    let mut first = true;
    for l in b.split(|&c| c == b'\n') {
        if first {
            first = false;
            continue;
        }
        f(l);
    }
    // a trailing "\n" produces one empty tail slice that is not a line
}

fn files(d: &Path) -> Vec<std::path::PathBuf> {
    csv::list_csv(d)
}

#[derive(Clone, Copy)]
struct Row {
    eid: u64,
    ingest: i64,
    user: u64,
    ts: i64,
    code: u8, // 0 view/cart, 1 purchase, 2 empty user/prod, 3 bad ts, 4 outside window, 5 late, 6 bad type, 7 bad price
    form: u8, // 1 F1, 2 F2, 3 F3
    price: u32,
    loc: u64, // file << 40 | line
}

fn find(p: &mut Vec<u32>, mut x: u32) -> u32 {
    while p[x as usize] != x {
        let g = p[p[x as usize] as usize];
        p[x as usize] = g;
        x = g;
    }
    x
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = Path::new(&a[1]);
    let man = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    let get = |k: &str| {
        let i = man.find(k).unwrap() + k.len();
        let r = &man[i..];
        let q1 = r.find('"').unwrap() + 1;
        let q2 = q1 + r[q1..].find('"').unwrap();
        parse_ts(r[q1..q2].as_bytes()).unwrap()
    };
    let (ws, we) = (get("\"window_start\""), get("\"window_end\""));
    let mut camps: FxHashMap<u64, u32> = FxHashMap::default();
    lines(&dir.join("campaigns.csv"), |l| {
        if let Some([c, _]) = csv::split::<2>(l) {
            let n = camps.len() as u32;
            camps.insert(h(c), n);
        }
    });

    // ---- links ----
    let mut idx: FxHashMap<u64, u32> = FxHashMap::default();
    let mut par: Vec<u32> = Vec::new();
    let (mut ln, mut lbad, mut lself) = (0u64, 0u64, 0u64);
    for f in files(&dir.join("links")) {
        lines(&f, |l| {
            ln += 1;
            let Some([x, y]) = csv::split::<2>(l) else { lbad += 1; return };
            if x.is_empty() || y.is_empty() {
                lbad += 1;
                return;
            }
            if x == y {
                lself += 1;
            }
            let mut id = |k: u64| -> u32 {
                *idx.entry(k).or_insert_with(|| {
                    par.push(par.len() as u32);
                    (par.len() - 1) as u32
                })
            };
            let (i, j) = (id(h(x)), id(h(y)));
            let (ri, rj) = (find(&mut par, i), find(&mut par, j));
            if ri != rj {
                par[ri.max(rj) as usize] = ri.min(rj);
            }
        });
    }
    let n_linked_ids = idx.len();
    // person of a user hash: linked root (tagged) or the user hash itself
    let person = |par: &mut Vec<u32>, u: u64| -> u64 {
        match idx.get(&u) {
            Some(&i) => (find(par, i) as u64) | (1u64 << 63),
            None => u & !(1u64 << 63),
        }
    };

    // ---- events: parse ----
    let evf = files(&dir.join("events"));
    let rows: Vec<Vec<Row>> = std::thread::scope(|s| {
        let per = evf.len().div_ceil(8).max(1);
        let hs: Vec<_> = evf
            .chunks(per)
            .enumerate()
            .map(|(ci, fs)| {
                s.spawn(move || {
                    let mut out = Vec::new();
                    let mut mal = 0u64;
                    let mut emp = 0u64;
                    for (fi, f) in fs.iter().enumerate() {
                        let fidx = (ci * per + fi) as u64;
                        let mut li = 0u64;
                        lines(f, |l| {
                            li += 1;
                            let loc = fidx << 40 | (li - 1);
                            let Some([eid, user, ts, ing, typ, prod, price]) = csv::split::<7>(l) else { mal += 1; return };
                            if eid.is_empty() {
                                emp += 1;
                                return;
                            }
                            let ingest = parse_ts(ing).unwrap_or(i64::MAX);
                            let form = if ts.len() == 20 { 1 } else if ts.len() == 25 { 2 } else { 3 };
                            let mut r = Row { eid: h(eid), ingest, user: h(user), ts: 0, code: 0, form, price: 0, loc };
                            r.code = 'v: {
                                if user.is_empty() || prod.is_empty() {
                                    break 'v 2;
                                }
                                let Some(t) = parse_ts(ts) else { break 'v 3 };
                                r.ts = t;
                                if t < ws || t >= we {
                                    break 'v 4;
                                }
                                if ingest - t > DAY {
                                    break 'v 5;
                                }
                                match typ {
                                    b"view" | b"cart" => 0,
                                    b"purchase" => {
                                        if price.is_empty() || !price.iter().all(|c| c.is_ascii_digit()) {
                                            break 'v 7;
                                        }
                                        let v: u64 = std::str::from_utf8(price).unwrap().parse().unwrap_or(u64::MAX);
                                        if !(1..=10_000_000).contains(&v) {
                                            break 'v 7;
                                        }
                                        r.price = v as u32;
                                        1
                                    }
                                    _ => 6,
                                }
                            };
                            out.push(r);
                        });
                    }
                    (out, mal, emp)
                })
            })
            .collect();
        let mut v = Vec::new();
        let (mut m, mut e) = (0, 0);
        for hh in hs {
            let (o, a, b) = hh.join().unwrap();
            v.push(o);
            m += a;
            e += b;
        }
        eprintln!("events: malformed {m} empty_eid {e}");
        v
    });
    let mut rows: Vec<Row> = rows.into_iter().flatten().collect();
    let nrows = rows.len() as u64;
    let mut code_rows = [0u64; 8];
    for r in &rows {
        code_rows[r.code as usize] += 1;
    }
    rows.sort_unstable_by_key(|r| (r.eid, r.ingest));

    // ---- dedup stats ----
    let mut groups = 0u64;
    let mut dupgroups = 0u64;
    let mut kept_code = [0u64; 8];
    let mut blockers = 0u64; // kept copy invalid, some later copy valid (invalid row that changes the output)
    let mut inv_single = 0u64; // invalid rows with a unique eid (only matter if duplicated: they are not)
    let mut inv_dup_nonkept = 0u64; // invalid non-kept copies (never matter)
    let mut inv_kept_alldead = 0u64; // invalid kept copy, all copies invalid (event dead anyway)
    let mut user_differs = 0u64; // dup groups whose copies name different users
    let mut valid_later_copies = 0u64; // valid non-kept copies (dropped by dedup)
    let mut keep_ev: Vec<u64> = Vec::new();
    let mut surv: Vec<(u64, i64, u32, u8)> = Vec::new(); // (user, ts, price, purchase)
    let mut i = 0;
    while i < rows.len() {
        let mut j = i + 1;
        while j < rows.len() && rows[j].eid == rows[i].eid {
            j += 1;
        }
        groups += 1;
        let g = &rows[i..j];
        let k = g[0];
        kept_code[k.code as usize] += 1;
        let kept_ok = k.code <= 1;
        if g.len() == 1 {
            if !kept_ok {
                inv_single += 1;
            }
        } else {
            dupgroups += 1;
            if g.iter().any(|r| r.user != k.user) {
                user_differs += 1;
            }
            for r in &g[1..] {
                if r.code > 1 {
                    inv_dup_nonkept += 1;
                } else {
                    valid_later_copies += 1;
                }
            }
            if !kept_ok {
                if g[1..].iter().any(|r| r.code <= 1) {
                    blockers += 1;
                } else {
                    inv_kept_alldead += 1;
                }
            }
        }
        if kept_ok {
            keep_ev.push(k.loc);
            surv.push((k.user, k.ts, k.price, k.code));
        }
        i = j;
    }
    let forms = {
        let mut f = [0u64; 4];
        for r in &rows {
            f[r.form as usize] += 1;
        }
        f
    };
    drop(rows);

    // survivors -> persons
    let mut linked_surv = 0u64;
    let mut sp: Vec<(u64, i64, u32, u8)> = surv
        .iter()
        .map(|&(u, t, p, c)| {
            let pp = person(&mut par, u);
            if pp >> 63 == 1 {
                linked_surv += 1;
            }
            (pp, t, p, c)
        })
        .collect();
    drop(surv);
    sp.sort_unstable_by_key(|x| (x.0, x.1));
    // sessions: person -> sorted session starts
    let mut sess: FxHashMap<u64, Vec<i64>> = FxHashMap::default();
    let mut nsess = 0u64;
    let mut purch = 0u64;
    {
        let mut i = 0;
        while i < sp.len() {
            let mut j = i;
            let p = sp[i].0;
            let mut v = Vec::new();
            let mut last = i64::MIN;
            while j < sp.len() && sp[j].0 == p {
                let t = sp[j].1;
                if v.is_empty() || t - last > GAP {
                    v.push(t);
                }
                last = t;
                if sp[j].3 == 1 {
                    purch += 1;
                }
                j += 1;
            }
            nsess += v.len() as u64;
            sess.insert(p, v);
            i = j;
        }
    }
    let nsurv = sp.len() as u64;
    drop(sp);

    // ---- clicks ----
    let mut cl_lines = 0u64;
    let mut cl_mal = 0u64;
    let mut cl_inv = [0u64; 5]; // empty cid, empty user, bad ts, outside window, unknown campaign
    let mut seen: FxHashSet<u64> = FxHashSet::default();
    let mut cl_dup = 0u64;
    let mut valid: Vec<(u64, i64, u32, u64)> = Vec::new(); // person, ts, camp, cid
    let cl_lo = ws - WEEK;
    let mut keep_cl: Vec<u64> = Vec::new();
    for (fidx, f) in files(&dir.join("clicks")).iter().enumerate() {
        let mut li = 0u64;
        lines(f, |l| {
            li += 1;
            let loc = (fidx as u64) << 40 | (li - 1);
            cl_lines += 1;
            let Some([cid, user, camp, ts]) = csv::split::<4>(l) else { cl_mal += 1; return };
            if cid.is_empty() {
                cl_inv[0] += 1;
                return;
            }
            let ch = h(cid);
            if !seen.insert(ch) {
                cl_dup += 1;
                return;
            }
            if user.is_empty() {
                cl_inv[1] += 1;
                return;
            }
            let Some(t) = parse_ts(ts) else { cl_inv[2] += 1; return };
            if t < cl_lo || t >= we {
                cl_inv[3] += 1;
                return;
            }
            let Some(&c) = camps.get(&h(camp)) else { cl_inv[4] += 1; return };
            valid.push((h(user), t, c, ch));
            keep_cl.push(loc);
        });
    }
    let mut no_person_events = 0u64;
    let mut no_session_in_week = 0u64; // person has sessions but none in [ts, ts+7d]
    let mut assist_rel = 0u64; // in [s-7d, s] for some s
    let mut pv: Vec<(u64, i64, u32, u64)> = valid.iter().map(|&(u, t, c, id)| (person(&mut par, u), t, c, id)).collect();
    pv.sort_unstable_by_key(|x| (x.0, x.1, x.3));
    let mut credited: FxHashSet<u64> = FxHashSet::default();
    let mut tie_diffcamp = 0u64; // same person, same ms, different campaign (click_id tie-break can matter)
    for k in 0..pv.len() {
        let (p, t, c, _) = pv[k];
        if k > 0 && pv[k - 1].0 == p && pv[k - 1].1 == t && pv[k - 1].2 != c {
            tie_diffcamp += 1;
        }
        let Some(ss) = sess.get(&p) else { no_person_events += 1; continue };
        let lo = ss.partition_point(|&s| s < t);
        if lo < ss.len() && ss[lo] - t <= WEEK {
            assist_rel += 1;
        } else {
            no_session_in_week += 1;
        }
    }
    // credited clicks: per session, latest ts <= s within 7d, tie smallest cid
    {
        let mut i = 0;
        while i < pv.len() {
            let mut j = i;
            while j < pv.len() && pv[j].0 == pv[i].0 {
                j += 1;
            }
            if let Some(ss) = sess.get(&pv[i].0) {
                let g = &pv[i..j];
                for &s in ss {
                    // last index with ts <= s
                    let e = g.partition_point(|x| x.1 <= s);
                    if e == 0 {
                        continue;
                    }
                    let tbest = g[e - 1].1;
                    if s - tbest > WEEK {
                        continue;
                    }
                    let f = g.partition_point(|x| x.1 < tbest);
                    credited.insert(g[f].3); // sorted by cid inside equal ts
                }
            }
            i = j;
        }
    }

    if let Ok(out) = std::env::var("SEMSTATS_OUT") {
        write_subset(dir, Path::new(&out), keep_ev, keep_cl);
    }
    println!("dataset {}", dir.display());
    println!("event rows(parsed, eid nonempty) {nrows}  ts forms F1 {} F2 {} F3 {}", forms[1], forms[2], forms[3]);
    println!("row codes: view/cart {} purchase {} empty_user/prod {} bad_ts {} out_window {} late {} bad_type {} bad_price {}", code_rows[0], code_rows[1], code_rows[2], code_rows[3], code_rows[4], code_rows[5], code_rows[6], code_rows[7]);
    println!("eid groups {groups}  dup groups {dupgroups}  dup lines {}  user-differs groups {user_differs}", nrows - groups);
    println!("kept codes: view/cart {} purchase {} invalid {}", kept_code[0], kept_code[1], kept_code[2..].iter().sum::<u64>());
    println!("invalid rows: unique-eid {inv_single} | dup non-kept {inv_dup_nonkept} | kept, all copies dead {inv_kept_alldead} | BLOCKERS (kept invalid, later valid copy) {blockers}");
    println!("valid non-kept copies {valid_later_copies}");
    println!("survivors {nsurv} (linked users {linked_surv})  purchases {purch}  sessions {nsess}  persons-with-events {}  linked ids {n_linked_ids}  links lines {ln} bad {lbad} self {lself}", sess.len());
    println!("clicks lines {cl_lines} malformed {cl_mal} empty_cid {} dup {cl_dup} empty_user {} bad_ts {} out_window {} unknown_camp {}", cl_inv[0], cl_inv[1], cl_inv[2], cl_inv[3], cl_inv[4]);
    println!("valid clicks {}  person w/o events {no_person_events}  no session in [ts,ts+7d] {no_session_in_week}  relevant(assist>=1) {assist_rel}  credited(distinct) {}  same-ms diff-camp ties {tie_diffcamp}", pv.len(), credited.len());
}

/// Writes a copy of the dataset holding only the given (file, line) rows of events/
/// clicks (same file names and order), links minus malformed/empty-side lines, and the
/// small files unchanged. Its outputs must equal the original's.
fn write_subset(dir: &Path, out: &Path, mut ev: Vec<u64>, mut cl: Vec<u64>) {
    use std::io::Write;
    ev.sort_unstable();
    cl.sort_unstable();
    for (sub, keep) in [("events", Some(&ev)), ("clicks", Some(&cl)), ("links", None)] {
        let od = out.join(sub);
        std::fs::create_dir_all(&od).unwrap();
        for (fidx, f) in files(&dir.join(sub)).iter().enumerate() {
            let b = std::fs::read(f).unwrap();
            let mut w = std::io::BufWriter::new(std::fs::File::create(od.join(f.file_name().unwrap())).unwrap());
            let mut it = b.split(|&c| c == b'\n');
            w.write_all(it.next().unwrap()).unwrap();
            w.write_all(b"\n").unwrap();
            let body: Vec<&[u8]> = it.collect();
            let n = if b.ends_with(b"\n") { body.len() - 1 } else { body.len() };
            for (li, l) in body[..n].iter().enumerate() {
                let ok = match keep {
                    Some(k) => k.binary_search(&((fidx as u64) << 40 | li as u64)).is_ok(),
                    None => matches!(csv::split::<2>(l), Some([x, y]) if !x.is_empty() && !y.is_empty()),
                };
                if ok {
                    w.write_all(l).unwrap();
                    w.write_all(b"\n").unwrap();
                }
            }
        }
    }
    for f in ["products.csv", "campaigns.csv", "manifest.json"] {
        std::fs::copy(dir.join(f), out.join(f)).unwrap();
    }
}
