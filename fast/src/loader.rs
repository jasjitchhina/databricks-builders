//! TEMPORARY simple (single-threaded) loader implementing the parse/clean/link
//! rules, producing `Cleaned`. To be replaced by h3's parallel parser.
use crate::model::*;
use rustc_hash::FxHashMap;
use std::path::Path;

pub fn split_fields<'a>(line: &'a [u8], n: usize, out: &mut Vec<Option<&'a [u8]>>) -> bool {
    out.clear();
    if line.is_empty() {
        return false;
    }
    for f in line.split(|&b| b == b',') {
        let v = if f.first() == Some(&b'"') {
            if f.len() < 2 || *f.last().unwrap() != b'"' {
                return false;
            }
            let inner = &f[1..f.len() - 1];
            if inner.contains(&b'"') {
                return false;
            }
            inner
        } else {
            if f.contains(&b'"') {
                return false;
            }
            f
        };
        out.push(if v.is_empty() { None } else { Some(v) });
        if out.len() > n {
            return false;
        }
    }
    out.len() == n
}

fn d2(b: &[u8]) -> Option<u32> {
    if b[0].is_ascii_digit() && b[1].is_ascii_digit() {
        Some(((b[0] - b'0') * 10 + (b[1] - b'0')) as u32)
    } else {
        None
    }
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse ts to epoch ms. None = bad form. F3 overflow -> Some(i64::MAX) (out of window).
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    if s.iter().all(|b| b.is_ascii_digit()) {
        let mut v: i64 = 0;
        for &b in s {
            v = match v.checked_mul(10).and_then(|x| x.checked_add((b - b'0') as i64)) {
                Some(x) => x,
                None => return Some(i64::MAX),
            };
        }
        return Some(v);
    }
    if s.len() != 20 && s.len() != 25 {
        return None;
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    if !s[0..4].iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y = s[0..4].iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64);
    let mo = d2(&s[5..7])?;
    let d = d2(&s[8..10])?;
    let h = d2(&s[11..13])?;
    let mi = d2(&s[14..16])?;
    let se = d2(&s[17..19])?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 || d < 1 {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][mo as usize - 1];
    if d > dim {
        return None;
    }
    let mut off_min: i64 = 0;
    if s.len() == 20 {
        if s[19] != b'Z' {
            return None;
        }
    } else {
        let sign = match s[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if s[22] != b':' {
            return None;
        }
        let oh = d2(&s[20..22])?;
        let om = d2(&s[23..25])?;
        if oh > 23 || om > 59 {
            return None;
        }
        off_min = sign * (oh as i64 * 60 + om as i64);
    }
    let days = days_from_civil(y, mo, d);
    let secs = days * 86400 + h as i64 * 3600 + mi as i64 * 60 + se as i64 - off_min * 60;
    Some(secs * 1000)
}

fn csv_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "csv").unwrap_or(false) && p.is_file())
            .collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

/// Calls f for each data line (header skipped) of each file.
fn for_lines(files: &[std::path::PathBuf], mut f: impl FnMut(&[u8])) {
    for p in files {
        let data = std::fs::read(p).expect("read");
        let mut it = data.split(|&b| b == b'\n');
        it.next(); // header
        let n_lines = data.iter().filter(|&&b| b == b'\n').count();
        // a trailing '\n' makes split yield a final empty slice that is not a line
        let ends_nl = data.last() == Some(&b'\n');
        let total = if ends_nl { n_lines } else { n_lines + 1 };
        for (i, line) in it.enumerate() {
            if i + 1 >= total {
                break;
            }
            f(line);
        }
    }
}

fn read_manifest(data: &Path) -> (i64, i64) {
    let s = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let get = |k: &str| -> i64 {
        let i = s.find(&format!("\"{}\"", k)).expect("key") + k.len() + 2;
        let rest = &s[i..];
        let a = rest.find('"').unwrap() + 1;
        let b = a + rest[a..].find('"').unwrap();
        parse_ts(rest[a..b].as_bytes()).expect("manifest ts")
    };
    (get("window_start"), get("window_end"))
}

struct Uf {
    p: Vec<u32>,
}
impl Uf {
    fn find(&mut self, mut x: u32) -> u32 {
        while self.p[x as usize] != x {
            let g = self.p[self.p[x as usize] as usize];
            self.p[x as usize] = g;
            x = g;
        }
        x
    }
}

#[derive(Default)]
struct Flat {
    base_ms: i64,
    ws_ms: i64,
    we_ms: i64,
    person_names: Vec<Box<[u8]>>,
    ev_person: Vec<u32>,
    ev_ts: Vec<u32>,
    ev_price: Vec<u32>,
    ev_cat: Vec<u16>,
    cat_names: Vec<Box<[u8]>>,
    cl_person: Vec<u32>,
    cl_ts: Vec<u32>,
    cl_camp: Vec<u16>,
    cl_id: Vec<Box<[u8]>>,
    camp_ids: Vec<Box<[u8]>>,
    camp_channels: Vec<Box<[u8]>>,
}

pub fn load(data: &Path) -> Cleaned {
    let c = load_flat(data);
    // rank-preserving click_id packing (temp loader: rank over all ids)
    let mut ids: Vec<&[u8]> = c.cl_id.iter().map(|b| &**b).collect();
    ids.sort_unstable();
    ids.dedup();
    let rank = |b: &[u8]| ids.binary_search(&b).unwrap() as u128;
    let mut parts: Vec<Part> = (0..NPART).map(|_| Part::default()).collect();
    for (g, name) in c.person_names.iter().enumerate() {
        parts[g % NPART].person_names.push(name.clone());
    }
    for i in 0..c.ev_ts.len() {
        let g = c.ev_person[i] as usize;
        parts[g % NPART].ev.push(Ev { person: (g / NPART) as u32, ts: c.ev_ts[i], price: c.ev_price[i], cat: c.ev_cat[i] });
    }
    for i in 0..c.cl_ts.len() {
        let g = c.cl_person[i] as usize;
        parts[g % NPART].cl.push(Cl { click_id: rank(&c.cl_id[i]), person: (g / NPART) as u32, ts: c.cl_ts[i], camp: c.cl_camp[i] });
    }
    Cleaned {
        base_ms: c.base_ms,
        ws_ms: c.ws_ms,
        we_ms: c.we_ms,
        cat_names: c.cat_names,
        camp_ids: c.camp_ids,
        camp_channels: c.camp_channels,
        parts,
    }
}

fn load_flat(data: &Path) -> Flat {
    let (ws, we) = read_manifest(data);
    let base = ws - 7 * DAY_MS;
    let mut c = Flat { base_ms: base, ws_ms: ws, we_ms: we, ..Default::default() };

    // products
    let mut cat_idx: FxHashMap<Vec<u8>, u16> = FxHashMap::default();
    let mut prod: FxHashMap<Vec<u8>, u16> = FxHashMap::default();
    for_lines(&[data.join("products.csv")], |l| {
        let mut fields = Vec::with_capacity(8);
        if split_fields(l, 2, &mut fields) {
            if let (Some(p), Some(cat)) = (fields[0], fields[1]) {
                let n = cat_idx.len() as u16;
                let ci = *cat_idx.entry(cat.to_vec()).or_insert(n);
                prod.insert(p.to_vec(), ci);
            }
        }
    });
    let unknown = {
        let n = cat_idx.len() as u16;
        *cat_idx.entry(b"unknown".to_vec()).or_insert(n)
    };
    let mut cats = vec![Box::<[u8]>::default(); cat_idx.len()];
    for (k, v) in cat_idx {
        cats[v as usize] = k.into_boxed_slice();
    }
    c.cat_names = cats;

    // campaigns
    let mut camp: FxHashMap<Vec<u8>, u16> = FxHashMap::default();
    for_lines(&[data.join("campaigns.csv")], |l| {
        let mut fields = Vec::with_capacity(8);
        if split_fields(l, 2, &mut fields) {
            if let (Some(id), Some(ch)) = (fields[0], fields[1]) {
                camp.insert(id.to_vec(), c.camp_ids.len() as u16);
                c.camp_ids.push(id.into());
                c.camp_channels.push(ch.into());
            }
        }
    });

    // links
    let mut ids: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
    let mut id_names: Vec<Box<[u8]>> = vec![];
    let mut uf = Uf { p: vec![] };
    let intern = |s: &[u8], ids: &mut FxHashMap<Vec<u8>, u32>, names: &mut Vec<Box<[u8]>>, uf: &mut Uf| -> u32 {
        if let Some(&i) = ids.get(s) {
            return i;
        }
        let i = names.len() as u32;
        ids.insert(s.to_vec(), i);
        names.push(s.into());
        uf.p.push(i);
        i
    };
    for_lines(&csv_files(&data.join("links")), |l| {
        let mut fields = Vec::with_capacity(8);
        if split_fields(l, 2, &mut fields) {
            if let (Some(a), Some(b)) = (fields[0], fields[1]) {
                let ia = intern(a, &mut ids, &mut id_names, &mut uf);
                let ib = intern(b, &mut ids, &mut id_names, &mut uf);
                let ra = uf.find(ia);
                let rb = uf.find(ib);
                if ra != rb {
                    uf.p[ra as usize] = rb;
                }
            }
        }
    });
    // person per linked id: root -> person idx
    let n_linked = id_names.len();
    let mut root_min: FxHashMap<u32, u32> = FxHashMap::default(); // root -> id with min name
    for i in 0..n_linked as u32 {
        let r = uf.find(i);
        let e = root_min.entry(r).or_insert(i);
        if id_names[i as usize] < id_names[*e as usize] {
            *e = i;
        }
    }
    let mut root_person: FxHashMap<u32, u32> = FxHashMap::default();
    let mut id_person: Vec<u32> = vec![0; n_linked];
    for i in 0..n_linked as u32 {
        let r = uf.find(i);
        let pi = *root_person.entry(r).or_insert_with(|| {
            c.person_names.push(id_names[root_min[&r] as usize].clone());
            (c.person_names.len() - 1) as u32
        });
        id_person[i as usize] = pi;
    }
    let mut person_of: FxHashMap<Vec<u8>, u32> = FxHashMap::default();
    for (k, &i) in ids.iter() {
        person_of.insert(k.clone(), id_person[i as usize]);
    }
    let mut person = |u: &[u8], names: &mut Vec<Box<[u8]>>| -> u32 {
        if let Some(&p) = person_of.get(u) {
            return p;
        }
        let p = names.len() as u32;
        names.push(u.into());
        person_of.insert(u.to_vec(), p);
        p
    };

    // events: dedup on event_id keeping min ingest_ts
    struct Ev {
        ingest: i64,
        line: Vec<u8>,
    }
    let mut evs: FxHashMap<Vec<u8>, Ev> = FxHashMap::default();
    for_lines(&csv_files(&data.join("events")), |l| {
        let mut fields = Vec::with_capacity(8);
        if !split_fields(l, 7, &mut fields) {
            return;
        }
        let Some(eid) = fields[0] else { return };
        // ingest_ts is always valid F1; if absent/invalid treat as malformed-ish (keep max)
        let ing = fields[3].and_then(parse_ts).unwrap_or(i64::MAX);
        match evs.get_mut(eid) {
            Some(e) => {
                if ing < e.ingest {
                    e.ingest = ing;
                    e.line = l.to_vec();
                }
            }
            None => {
                evs.insert(eid.to_vec(), Ev { ingest: ing, line: l.to_vec() });
            }
        }
    });
    let mut kept: Vec<(Vec<u8>, Ev)> = evs.into_iter().collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, e) in kept {
        let mut fields = Vec::with_capacity(8);
        split_fields(&e.line, 7, &mut fields);
        let (Some(u), Some(ts), Some(ty), Some(pid)) = (fields[1], fields[2], fields[4], fields[5]) else { continue };
        let Some(t) = parse_ts(ts) else { continue };
        if t < ws || t >= we {
            continue;
        }
        let price: u32 = match ty {
            b"view" | b"cart" => 0,
            b"purchase" => {
                let Some(p) = fields[6] else { continue };
                if !p.iter().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let mut v: u64 = 0;
                for &b in p {
                    v = (v * 10 + (b - b'0') as u64).min(u64::MAX / 100);
                }
                if v < 1 || v > 10_000_000 {
                    continue;
                }
                v as u32
            }
            _ => continue,
        };
        if e.ingest - t > DAY_MS {
            continue;
        }
        let p = person(u, &mut c.person_names);
        c.ev_person.push(p);
        c.ev_ts.push((t - base) as u32);
        c.ev_price.push(price);
        c.ev_cat.push(*prod.get(pid).unwrap_or(&unknown));
    }

    // clicks
    let mut seen: FxHashMap<Vec<u8>, ()> = FxHashMap::default();
    for_lines(&csv_files(&data.join("clicks")), |l| {
        let mut fields = Vec::with_capacity(8);
        if !split_fields(l, 4, &mut fields) {
            return;
        }
        let Some(cid) = fields[0] else { return };
        if seen.contains_key(cid) {
            return;
        }
        seen.insert(cid.to_vec(), ());
        let (Some(u), Some(cp), Some(ts)) = (fields[1], fields[2], fields[3]) else { return };
        let Some(t) = parse_ts(ts) else { return };
        if t < base || t >= we {
            return;
        }
        let Some(&ci) = camp.get(cp) else { return };
        let p = person(u, &mut c.person_names);
        c.cl_person.push(p);
        c.cl_ts.push((t - base) as u32);
        c.cl_camp.push(ci);
        c.cl_id.push(cid.into());
    });
    c
}
