#!/bin/bash
# oracle/fuzz_target.sh FIRST LAST BIN [fuzz.py args...]
# Diff BIN (called as BIN DATA_DIR OUT_DIR) against the oracle on fuzz seeds FIRST..LAST.
# Failing datasets are kept in $FUZZ_DIR/<seed> (default ~/data/fuzz-target) with both outputs.
# Run long batches under: taskset -c 8-31
HERE=$(cd "$(dirname "$0")" && pwd)
export PYTHONPATH="$HOME/pylib"
BIN=$(readlink -f "$3")
FD=${FUZZ_DIR:-$HOME/data/fuzz-target}; mkdir -p "$FD"
fail=0
for s in $(seq "$1" "$2"); do
  d=$(mktemp -d); python3 "$HERE/fuzz.py" "$s" "$d/in"
  if ! python3 "$HERE/oracle.py" "$d/in" "$d/o" 2>"$d/oerr"; then
    echo "ORACLE CRASH seed $s"; fail=1; rm -rf "$FD/$s"; mv "$d" "$FD/$s"; continue; fi
  if ! NCPU=${NCPU:-2} "$BIN" "$d/in" "$d/t" >"$d/tout" 2>"$d/terr"; then
    echo "TARGET CRASH seed $s: $(tail -1 "$d/terr")"; fail=1; rm -rf "$FD/$s"; mv "$d" "$FD/$s"; continue; fi
  if ! python3 "$HERE/../scripts/compare.py" "$d/t" "$d/o" >"$d/cmp" 2>&1; then
    echo "DIFF seed $s -> $FD/$s/cmp"; fail=1; rm -rf "$FD/$s"; mv "$d" "$FD/$s"; continue; fi
  rm -rf "$d"
done
exit $fail
