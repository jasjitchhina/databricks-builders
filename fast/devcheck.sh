#!/bin/bash
# devcheck.sh DATA ANSWER_DIR(with user_daily.csv,campaign_daily.csv) [BIN]
here=$(cd "$(dirname "$0")" && pwd)
BIN=${3:-$here/target/release/fast}; PQ=$here/target/release/pq2csv
tmp=$(mktemp -d); rc=0
FAST_TIMING=1 "$BIN" "$1" "$tmp/out" || exit 1
for t in user_daily campaign_daily; do
  cols=$(head -1 "$2/$t.csv")
  tail -n +2 "$2/$t.csv" | LC_ALL=C sort > "$tmp/exp"
  "$PQ" "$tmp/out/$t" "$cols" | LC_ALL=C sort > "$tmp/act"
  if cmp -s "$tmp/exp" "$tmp/act"; then echo "$t MATCH $(wc -l < $tmp/exp) rows"; else rc=1; echo "$t DIFFER"; diff "$tmp/exp" "$tmp/act" | head -10; fi
done
rm -rf "$tmp"; exit $rc
