#!/bin/bash
# gate.sh: unit tests + stage-dump checks in every key/category mode; exit 1 on any failure (use before committing).
set -u
HERE=$(cd "$(dirname "$0")/.." && pwd)
FX=${FX_DUMPS:?}
cd $HERE && cargo test --release --lib 2>&1 | grep -q "test result: ok" || { echo "GATE FAIL: unit tests"; exit 1; }
SETS_DEFAULT="dev s2-1m s3-1m s15-1m-feb29 adv-perturb-s21 adv-synth-s2 adv-synth-s4 adv-shape-longids adv-shape-manyfiles adv-shape-onefile adv-shape-sparse adv-shape-hugeline"
for mode in "" "FAST_WIDE=1" "FAST_SIDE_CAT=1" "FAST_NB_BITS=7" "FAST_STRICT_INGEST=1"; do
  if ! env $mode FX_DUMPS=$FX SETS="${SETS:-$SETS_DEFAULT}" $HERE/tools/check_all.sh > /tmp/gate.$$.log 2>&1; then
    echo "GATE FAIL mode=[$mode]"; grep -vE "^ok" /tmp/gate.$$.log | head -8; rm -f /tmp/gate.$$.log; exit 1
  fi
  echo "gate ok mode=[$mode]"
done
rm -f /tmp/gate.$$.log
echo "GATE PASS"
