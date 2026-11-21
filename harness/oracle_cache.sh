#!/usr/bin/env bash
# oracle_cache.sh [--force] DATASET...  — writes the oracle's answer for ~/data/DATASET to ~/data/oracle/DATASET/
# ORACLE (default /home/jessechhina/wt/h1/oracle.sh) is called as: ORACLE DATA_DIR OUT_DIR.
# Refuses unless the oracle first reproduces dev-answer exactly (checked once per oracle commit).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA_ROOT=${DATA_ROOT:-$HOME/data}
ORACLE=${ORACLE:-/home/jessechhina/wt/h1/oracle.sh}
export PYTHONPATH=$HOME/pylib${PYTHONPATH:+:$PYTHONPATH}
FORCE=0; [ "${1:-}" = --force ] && { FORCE=1; shift; }
OC=$(git -C "$(dirname "$ORACLE")" rev-parse --short HEAD)
OK="$DATA_ROOT/oracle/.dev-ok-$OC"
mkdir -p "$DATA_ROOT/oracle"
if [ ! -f "$OK" ]; then
  T=$(mktemp -d "$DATA_ROOT/oracle/.devcheck.XXXX")
  "$ORACLE" "$DATA_ROOT/dev" "$T" >&2
  python3 "$ROOT/scripts/compare.py" "$T" "$ROOT/dev-answer" || { echo "oracle $OC does NOT match dev-answer; refusing" >&2; exit 1; }
  rm -rf "$T"; touch "$OK"
fi
for ds in "$@"; do
  dst="$DATA_ROOT/oracle/$ds"
  if [ -f "$dst/DONE" ] && [ $FORCE = 0 ]; then echo "have $ds ($(cat "$dst/DONE"))"; continue; fi
  rm -rf "$dst.tmp" "$dst"; mkdir -p "$dst.tmp"
  s=$(date +%s)
  "$ORACLE" "$DATA_ROOT/$ds" "$dst.tmp" >&2
  echo "oracle=$OC secs=$(( $(date +%s)-s )) when=$(date -Is)" > "$dst.tmp/DONE"
  mv "$dst.tmp" "$dst"
  echo "made $ds ($(cat "$dst/DONE"))"
done
