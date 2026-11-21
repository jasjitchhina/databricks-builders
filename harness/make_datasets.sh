#!/usr/bin/env bash
# make_datasets.sh — (re)creates every dataset under ~/data used by verify.sh / bench.sh, then the oracle answers.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd); D=${DATA_ROOT:-$HOME/data}; G=$D/bin/generate
mkdir -p "$D/bin" "$D/scratch"
[ -x "$G" ] || { (cd "$H/../generator" && cargo build --release -q) && cp "$H/../generator/target/release/generate" "$G"; }
gen() { [ -f "$D/$4/manifest.json" ] || "$G" --seed "$1" --events "$2" --clicks "$3" --output "$D/$4"; }
gen 1 1000000 250000 dev
for s in 2 3 4 5; do gen $s 1000000 250000 s$s-1m; done
gen 15 1000000 250000 s15-1m-feb29      # window 2028-02-17..03-02
gen 62 1000000 250000 s62-1m-feb29      # window 2028-02-21..03-06
gen 7 20000000 5000000 s7-20m
gen 11 60000000 15000000 s11-60m
# adversarial
[ -d "$D/adv-perturb-s21" ] || { gen 21 1000000 250000 scratch/src-s21; python3 "$H/adv_perturb.py" "$D/scratch/src-s21" "$D/adv-perturb-s21" 21; }
[ -d "$D/adv-synth-s2" ] || python3 "$H/adv_synth.py" "$D/adv-synth-s2" 2
[ -d "$D/adv-synth-s3" ] || python3 "$H/adv_synth.py" "$D/adv-synth-s3" 3
[ -d "$D/adv-synth-s4" ] || CHAIN=400 STAR=400 python3 "$H/adv_synth.py" "$D/adv-synth-s4" 4   # short chains: xcheck-friendly
[ -d "$D/adv-shape-onefile" ] || python3 "$H/adv_shapes.py" "$D/s4-1m" "$D/adv-shape"   # + adv-shape-hugeline: see notes (onefile + 100MB/5M-comma/40MB-quoted lines)
for d in "$D"/adv-*; do python3 "$H/check_invariants.py" "$d"; done
"$H/oracle_cache.sh" s2-1m s3-1m s15-1m-feb29 s62-1m-feb29 adv-perturb-s21 adv-synth-s2 adv-synth-s3 adv-synth-s4 adv-shape-onefile adv-shape-manyfiles adv-shape-longids adv-shape-sparse
