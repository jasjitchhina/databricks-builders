//! prepdump DATA_DIR DUMP_DIR: runs the h3 cleaning stage and writes
//! events_fast.csv (person,ts_ms,price,category) and clicks_fast.csv
//! (click_id,person,campaign_id,ts_ms), both sorted, for diffing vs the oracle.
use fast::clean::clean;
use std::io::Write;

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let threads: usize = std::env::var("NCPU").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let t0 = std::time::Instant::now();
    let c = clean(std::path::Path::new(&a[1]), threads);
    eprintln!("clean: {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    let ne: usize = c.parts.iter().map(|p| p.ev.len()).sum();
    let nc: usize = c.parts.iter().map(|p| p.cl.len()).sum();
    let np: usize = c.parts.iter().map(|p| p.person_names.len()).sum();
    eprintln!("events {ne} clicks {nc} persons {np}");
    if a.len() < 3 {
        return;
    }
    let out = std::path::Path::new(&a[2]);
    std::fs::create_dir_all(out).unwrap();
    let mut ev = Vec::new();
    let mut cl = Vec::new();
    let mut ids: std::collections::HashMap<u128, Vec<u8>> = Default::default();
    for p in &c.parts {
        for e in &p.ev {
            let cat = if e.price > 0 { s(&c.cat_names[e.cat as usize]) } else { String::new() };
            ev.push(format!("{},{},{},{}", s(&p.person_names[e.person as usize]), e.ts as i64 + c.base_ms, e.price, cat));
        }
        for k in &p.cl {
            ids.entry(k.click_id).or_insert_with(|| fast::ids::key_bytes(k.click_id).into());
            cl.push(format!("{},{},{},{}", s(&ids[&k.click_id]), s(&p.person_names[k.person as usize]), s(&c.camp_ids[k.camp as usize]), k.ts as i64 + c.base_ms));
        }
    }
    ev.sort();
    cl.sort();
    cl.dedup();
    let mut f = std::io::BufWriter::new(std::fs::File::create(out.join("events_fast.csv")).unwrap());
    for l in ev { writeln!(f, "{l}").unwrap(); }
    let mut f = std::io::BufWriter::new(std::fs::File::create(out.join("clicks_fast.csv")).unwrap());
    for l in cl { writeln!(f, "{l}").unwrap(); }
}
