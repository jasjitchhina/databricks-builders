#!/usr/bin/env bash
# extremes.sh SHA [JOBS] < list — correctness on official-generator seeds at the corners of plan.rs's per-seed
# rate ranges (event dups 2-4%, click dups 1-3%, bad ts 0.3-0.7%, late 0.5-1.5%), 1M-5M rows.
# list lines: NAME SEED EVENTS CLICKS. For each: generate -> main's oracle -> scorer binary (/app/fast of main's
# image, as the soak) -> compare; then h1's independent xcheck -> compare. Results: ~/data/xt/results.txt;
# failing datasets are kept (passing ones deleted). Corner seeds come from ~/data/scratch/planscan (seed->rates).
set -uo pipefail
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(dirname "$H")
SHA=$1; JOBS=${2:-2}; CPUS=${XT_CPUS:-12-15,28-31}
GEN=$HOME/data/bin/generate; BIN=$HOME/data/soak/bin/$SHA; X=$HOME/data/xt; mkdir -p "$X"
export PYTHONPATH=$HOME/pylib
[ -x "$BIN/fast" ] || { echo "no scorer binary at $BIN (soak build_main builds it)"; exit 1; }
one() {
  local name=$1 seed=$2 ev=$3 cl=$4 R=$X/$1 t0 r
  rm -rf "$R"; mkdir -p "$R"; t0=$(date +%s)
  taskset -c "$CPUS" "$GEN" --seed "$seed" --events "$ev" --clicks "$cl" --output "$R/data" >/dev/null 2>&1 || { echo "$name GENFAIL"; return; }
  taskset -c "$CPUS" "$BIN/oracle.sh" "$R/data" "$R/oracle" > "$R/oracle.log" 2>&1 || { echo "$name seed=$seed ORACLE_CRASH"; return; }
  mkdir -p "$R/out" "$R/scr"
  DATA_DIR="$R/data" OUTPUT_DIR="$R/out" SCRATCH_DIR="$R/scr" NCPU=8 taskset -c "$CPUS" "$BIN/fast" > "$R/fast.log" 2>&1 || { echo "$name seed=$seed FAST_EXIT=$?"; return; }
  if python3 "$ROOT/scripts/compare.py" "$R/out" "$R/oracle" > "$R/cmp.txt" 2>&1; then r="fast=MATCH"; else r="fast=DIFFER"; fi
  if taskset -c "$CPUS" timeout 7200 python3 "$BIN/oracle/xcheck.py" "$R/data" "$R/xc" > "$R/xc.log" 2>&1 && python3 "$ROOT/scripts/compare.py" "$R/xc" "$R/oracle" > "$R/xcmp.txt" 2>&1; then r="$r xcheck=MATCH"; else r="$r xcheck=DIFFER"; fi
  local rows; rows=$(grep -aoE 'user_daily: rows [0-9]+' "$R/cmp.txt" | head -1)
  echo "$name seed=$seed ev=$ev cl=$cl $r ($rows, $(( $(date +%s) - t0 ))s)"
  case "$r" in *DIFFER*) ;; *) rm -rf "$R";; esac
}
export -f one; export X GEN BIN CPUS ROOT
xargs -P "$JOBS" -L 1 bash -c 'one "$@"' _ | tee -a "$X/results.txt"
