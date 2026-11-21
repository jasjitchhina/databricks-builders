#!/bin/bash
# Run fast binary on all fixtures, diff vs expected CSVs. Usage: fast/fixtures.sh [BIN]
here=$(cd "$(dirname "$0")" && pwd)
BIN=${1:-$here/target/release/fast}
PQ=$here/target/release/pq2csv
FX=$here/../fixtures
tmp=$(mktemp -d)
fail=0
for d in "$FX"/*/; do
  c=$(basename "$d")
  rm -rf "$tmp/out"; mkdir -p "$tmp/out"
  if ! "$BIN" "$d/input" "$tmp/out" 2>"$tmp/err"; then echo "FAIL $c (exit)"; cat "$tmp/err"; fail=1; continue; fi
  ok=1
  for t in user_daily campaign_daily; do
    cols=$(head -1 "$d/expected/$t.csv")
    tail -n +2 "$d/expected/$t.csv" | LC_ALL=C sort > "$tmp/exp"
    "$PQ" "$tmp/out/$t" "$cols" | LC_ALL=C sort > "$tmp/act"
    if ! diff "$tmp/exp" "$tmp/act" > "$tmp/diff"; then ok=0; echo "FAIL $c $t"; cat "$tmp/diff"; fi
  done
  [ $ok = 1 ] || fail=1
done
rm -rf "$tmp"
[ $fail = 0 ] && echo "ALL FIXTURES PASS"
exit $fail
