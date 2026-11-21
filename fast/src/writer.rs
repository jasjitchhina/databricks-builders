//! Output directories + campaign_daily (user_daily is written by engine workers).
use crate::engine::Output;
use crate::model::Cleaned;
use crate::pq;
use std::path::{Path, PathBuf};

pub fn prepare_dirs(out_dir: &Path) -> (PathBuf, PathBuf) {
    let ud = out_dir.join("user_daily");
    let cd = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("mkdir user_daily");
    std::fs::create_dir_all(&cd).expect("mkdir campaign_daily");
    (ud, cd)
}

pub fn write_campaigns(cd: &Path, c: &Cleaned, o: &Output) {
    let ncamp = c.camp_ids.len();
    let mut cols = pq::camp_cols();
    let mut n = 0;
    for k in 0..=ncamp {
        for di in 0..o.ndays {
            let a = o.camp[k * o.ndays + di];
            if a.sessions == 0 && a.assisted == 0 {
                continue;
            }
            if k == ncamp {
                cols[0].push_str(b"organic");
                cols[1].push_str(b"organic");
            } else {
                cols[0].push_str(&c.camp_ids[k]);
                cols[1].push_str(&c.camp_channels[k]);
            }
            cols[2].push_i32(o.day0 + di as i32);
            cols[3].push_i64(a.sessions as i64);
            cols[4].push_i64(a.purchasing as i64);
            cols[5].push_i64(a.revenue as i64);
            cols[6].push_i64(a.assisted as i64);
            n += 1;
        }
    }
    std::fs::write(cd.join("part-0.parquet"), pq::encode(&cols, n)).expect("write campaign_daily");
}
