#!/bin/bash
# oracle/stress_sets.sh BIN NCPU SET... : run BIN on each ~/data/SET (fresh out dir), require exit 0 and
# exact match vs the oracle answer (dev: dev-answer). Prints one line per set; exit 1 on any failure.
HERE=$(cd "$(dirname "$0")" && pwd)
export PYTHONPATH="$HOME/pylib"
BIN=$(readlink -f "$1"); N=$2; shift 2
W=$(mktemp -d); fail=0
for ds in "$@"; do
  ans="$HOME/data/oracle/$ds/out"; [ "$ds" = dev ] && ans="$HERE/../dev-answer"
  rm -rf "$W/o"
  s=$(date +%s%N)
  NCPU=$N "$BIN" "$HOME/data/$ds" "$W/o" >"$W/log" 2>&1; rc=$?
  ms=$(( ($(date +%s%N) - s) / 1000000 ))
  if [ $rc -ne 0 ]; then echo "FAIL $ds NCPU=$N exit=$rc ${ms}ms: $(tail -2 "$W/log" | tr '\n' ' ')"; fail=1; continue; fi
  r=$(python3 "$HERE/../scripts/compare.py" "$W/o" "$ans" 2>&1 | tail -1)
  if [ "$r" = MATCH ]; then echo "ok   $ds NCPU=$N ${ms}ms"; else echo "DIFF $ds NCPU=$N"; fail=1; fi
done
rm -rf "$W"; exit $fail
