#!/bin/bash
# oracle/fuzz.sh FIRST LAST : oracle vs xcheck on fuzz seeds; keeps failing datasets in $FUZZ_DIR
HERE=$(cd "$(dirname "$0")" && pwd)
export PYTHONPATH="$HOME/pylib"
FD=${FUZZ_DIR:-$HOME/data/fuzz}; mkdir -p "$FD"
fail=0
for s in $(seq "$1" "$2"); do
  d=$(mktemp -d); python3 "$HERE/fuzz.py" "$s" "$d/in"
  if ! python3 "$HERE/oracle.py" "$d/in" "$d/o" 2>"$d/oerr" || ! python3 "$HERE/xcheck.py" "$d/in" "$d/x" 2>"$d/xerr"; then
    echo "CRASH seed $s"; tail -2 "$d/oerr" "$d/xerr"; fail=1; rm -rf "$FD/$s"; mv "$d" "$FD/$s"; continue; fi
  if ! python3 "$HERE/../scripts/compare.py" "$d/o" "$d/x" >"$d/cmp" 2>&1; then
    echo "DIFF seed $s"; fail=1; rm -rf "$FD/$s"; mv "$d" "$FD/$s"; continue; fi
  rm -rf "$d"
done
exit $fail
