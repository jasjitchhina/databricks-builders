// planscan FIRST LAST: per-seed dirt rates of the official generator (plan.rs draws them first on Stream::Dataset,
// independent of --events/--clicks): prints "seed event_dups click_dups bad_ts late". Used by harness/extremes.sh.
#![allow(dead_code)]
#[path = "../../../generator/src/random.rs"]
mod random;
use random::{Random, Stream};
fn main() {
    let a: Vec<u64> = std::env::args().skip(1).map(|x| x.parse().unwrap()).collect();
    for seed in a[0]..a[1] {
        let mut r = Random::new(seed, Stream::Dataset, 0);
        let _ws = r.below(1000);
        let ed = r.rate(0.02, 0.04);
        let cd = r.rate(0.01, 0.03);
        let bt = r.rate(0.003, 0.007);
        let late = r.rate(0.005, 0.015);
        println!("{seed} {ed:.5} {cd:.5} {bt:.5} {late:.5}");
    }
}
