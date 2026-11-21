//! Parquet output for user_daily / campaign_daily.
use crate::engine::Output;
use crate::model::Cleaned;
use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use rayon::prelude::*;
use std::path::Path;
use std::sync::Arc;

const USER_SCHEMA: &str = "message user_daily {
  required binary user_id (STRING);
  required int32 day (DATE);
  required int64 sessions;
  required int64 events;
  required int64 revenue_cents;
  optional binary top_category (STRING);
}";

const CAMP_SCHEMA: &str = "message campaign_daily {
  required binary campaign_id (STRING);
  required binary channel (STRING);
  required int32 day (DATE);
  required int64 sessions;
  required int64 purchasing_sessions;
  required int64 revenue_cents;
  required int64 assisted_sessions;
}";

fn props() -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_max_row_group_row_count(None)
            .build(),
    )
}

pub enum Col {
    Str(Vec<ByteArray>),
    OptStr(Vec<ByteArray>, Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
}

fn write_file(path: &Path, schema: &str, cols: Vec<Col>) {
    let schema = Arc::new(parse_message_type(schema).unwrap());
    let f = std::fs::File::create(path).expect("create parquet");
    let f = std::io::BufWriter::with_capacity(1 << 20, f);
    let mut w = SerializedFileWriter::new(f, schema, props()).unwrap();
    let mut rg = w.next_row_group().unwrap();
    let mut it = cols.into_iter();
    while let Some(mut cw) = rg.next_column().unwrap() {
        match it.next().unwrap() {
            Col::Str(v) => {
                cw.typed::<ByteArrayType>().write_batch(&v, None, None).unwrap();
            }
            Col::OptStr(v, d) => {
                cw.typed::<ByteArrayType>().write_batch(&v, Some(&d), None).unwrap();
            }
            Col::I32(v) => {
                cw.typed::<Int32Type>().write_batch(&v, None, None).unwrap();
            }
            Col::I64(v) => {
                cw.typed::<Int64Type>().write_batch(&v, None, None).unwrap();
            }
        }
        cw.close().unwrap();
    }
    rg.close().unwrap();
    w.close().unwrap();
}

fn ba(b: &[u8]) -> ByteArray {
    ByteArray::from(b.to_vec())
}

pub fn write(out_dir: &Path, c: &Cleaned, o: &Output) {
    let ud = out_dir.join("user_daily");
    let cd = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud).unwrap();
    std::fs::create_dir_all(&cd).unwrap();

    // user_daily: split row groups into ~nthreads parts written in parallel
    let all: Vec<&crate::engine::UserRow> = o.user_rows.iter().flatten().collect();
    let nparts = if all.len() < 100_000 { 1 } else { rayon::current_num_threads().clamp(1, 16) };
    let per = all.len().div_ceil(nparts).max(1);
    let parts: Vec<&[&crate::engine::UserRow]> = if all.is_empty() { vec![&all[..]] } else { all.chunks(per).collect() };
    let camp_job = || {
        let ncamp = c.camp_ids.len();
        let (mut id, mut ch, mut day, mut s, mut ps, mut rev, mut asst) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        for k in 0..=ncamp {
            for di in 0..o.ndays {
                let a = o.camp[k * o.ndays + di];
                if a.sessions == 0 && a.assisted == 0 {
                    continue;
                }
                if k == ncamp {
                    id.push(ba(b"organic"));
                    ch.push(ba(b"organic"));
                } else {
                    id.push(ba(&c.camp_ids[k]));
                    ch.push(ba(&c.camp_channels[k]));
                }
                day.push(o.day0 + di as i32);
                s.push(a.sessions as i64);
                ps.push(a.purchasing as i64);
                rev.push(a.revenue as i64);
                asst.push(a.assisted as i64);
            }
        }
        write_file(
            &cd.join("part-0.parquet"),
            CAMP_SCHEMA,
            vec![Col::Str(id), Col::Str(ch), Col::I32(day), Col::I64(s), Col::I64(ps), Col::I64(rev), Col::I64(asst)],
        );
    };
    rayon::join(camp_job, || {
        parts.par_iter().enumerate().for_each(|(pi, rows)| {
            let n = rows.len();
            let mut uid = Vec::with_capacity(n);
            let mut day = Vec::with_capacity(n);
            let mut s = Vec::with_capacity(n);
            let mut e = Vec::with_capacity(n);
            let mut r = Vec::with_capacity(n);
            let mut top = Vec::with_capacity(n);
            let mut def = Vec::with_capacity(n);
            for row in rows.iter() {
                uid.push(ba(&c.parts[row.part as usize].person_names[row.person as usize]));
                day.push(row.day);
                s.push(row.sessions as i64);
                e.push(row.events as i64);
                r.push(row.revenue as i64);
                match row.top_cat {
                    Some(t) => {
                        top.push(ba(&c.cat_names[t as usize]));
                        def.push(1);
                    }
                    None => def.push(0),
                }
            }
            write_file(
                &ud.join(format!("part-{pi}.parquet")),
                USER_SCHEMA,
                vec![Col::Str(uid), Col::I32(day), Col::I64(s), Col::I64(e), Col::I64(r), Col::OptStr(top, def)],
            );
        });
    });
}
