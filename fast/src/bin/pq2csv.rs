//! pq2csv DIR TABLE: prints sorted CSV of all *.parquet in DIR (for diffing vs expected CSVs).
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::Path::new(&a[1]);
    let cols: Vec<&str> = a[2].split(',').collect();
    let mut lines = vec![];
    let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    for p in paths {
        let n = p.file_name().unwrap().to_string_lossy().to_string();
        if !n.ends_with(".parquet") || n.starts_with('.') || n.starts_with('_') { continue; }
        let r = SerializedFileReader::new(std::fs::File::open(&p).unwrap()).unwrap();
        for row in r.get_row_iter(None).unwrap() {
            let row = row.unwrap();
            let mut vals = vec![String::new(); cols.len()];
            for (name, f) in row.get_column_iter() {
                let i = cols.iter().position(|c| c == name).unwrap_or_else(|| panic!("extra column {name}"));
                vals[i] = match f {
                    Field::Null => String::new(),
                    Field::Str(s) => s.clone(),
                    Field::Date(d) => {
                        // days since epoch -> YYYY-MM-DD
                        let z = *d as i64 + 719468;
                        let era = z.div_euclid(146097);
                        let doe = z - era * 146097;
                        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
                        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                        let mp = (5 * doy + 2) / 153;
                        let dd = doy - (153 * mp + 2) / 5 + 1;
                        let m = if mp < 10 { mp + 3 } else { mp - 9 };
                        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
                        format!("{y:04}-{m:02}-{dd:02}")
                    }
                    other => other.to_string(),
                };
            }
            lines.push(vals.join(","));
        }
    }
    lines.sort();
    for l in lines { println!("{l}"); }
}
