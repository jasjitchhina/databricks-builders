#!/usr/bin/env bash
# verify.sh: correctness gate. Nothing merges to main unless this exits 0.
#
#   ./verify.sh [TARGET]
#
# TARGET: an executable (run natively, gets DATA_DIR/OUTPUT_DIR/SCRATCH_DIR env and args DATA OUT)
#         or a docker image tag. Default: build the repo-root Dockerfile as verify-<worktree>:latest.
# Checks, in order (all run; exit 1 if any fails):
#   1. all fixtures/                      (expected CSVs, scripts/run_fixtures.py)
#   2. check-dev                          (~/data/dev vs dev-answer/)
#   3. row-exact diff vs the oracle cache (~/data/oracle/<ds>/out/, h1's layout) on $VERIFY_SETS
#      default: s2-1m s3-1m s15-1m-feb29 + every ~/data/adv-* + s7-20m (20M)
# Env: QUICK=1 skips the 20M set. VERIFY_SETS="a b" overrides the oracle sets.
#      ALLOW_MISSING_ORACLE=1 skips (instead of failing) sets with no oracle answer yet.
# Builds and runs take the SHARED /tmp/bench.lock. Oracle answers are made by harness/oracle_cache.sh.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")" && pwd)
DATA_ROOT=${DATA_ROOT:-$HOME/data}
export PYTHONPATH=$HOME/pylib${PYTHONPATH:+:$PYTHONPATH}
TARGET=${1:-}
if [ -z "$TARGET" ]; then
  TARGET="verify-$(basename "$ROOT" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9_.\n-' '-'):latest"
  echo "== building $TARGET" >&2
  flock -s /tmp/bench.lock docker build -q --platform linux/amd64 -t "$TARGET" "$ROOT" >&2 || { echo "VERIFY FAIL: build"; exit 1; }
elif [ -f "$TARGET" ]; then
  TARGET=$(cd "$(dirname "$TARGET")" && pwd)/$(basename "$TARGET")
fi
export TARGET
RUN="$ROOT/harness/run_target.sh"
WORK=$(mktemp -d "$DATA_ROOT/scratch/verify.XXXXXX" 2>/dev/null || { mkdir -p "$DATA_ROOT/scratch"; mktemp -d "$DATA_ROOT/scratch/verify.XXXXXX"; })
trap 'rm -rf "$WORK" 2>/dev/null' EXIT
FAILS=()
COMMIT=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null)

echo "== [0] lint"
LINT=$(git -C "$ROOT" grep -nIE 'target-cpu *= *native|target-cpu=native|-march=native|avx512' -- ':!verify.sh' ':!*.md' 2>/dev/null || true)
if [ -n "$LINT" ]; then echo "FAIL lint: native/avx512 codegen flags found:"; echo "$LINT"; FAILS+=(lint-native); fi
SIZE_KB=$(git -C "$ROOT" ls-files -z | (cd "$ROOT" && xargs -0 du -ck 2>/dev/null) | tail -1 | cut -f1)
if [ "${SIZE_KB:-0}" -gt 45000 ]; then echo "FAIL lint: tracked files ${SIZE_KB} KB (limit 50 MB)"; FAILS+=(lint-size); fi
BIG=$(git -C "$ROOT" ls-files | grep -E '\.(parquet|csv)$' | grep -vE '^(fixtures|dev-answer)/' || true)
if [ -n "$BIG" ]; then echo "FAIL lint: data files tracked: $BIG"; FAILS+=(lint-data); fi
[ ${#FAILS[@]} -eq 0 ] && echo "ok   lint (tracked ${SIZE_KB} KB)"

echo "== [1] fixtures ($TARGET)"
if python3 "$ROOT/scripts/run_fixtures.py" "$ROOT/fixtures" -- "$RUN" > "$WORK/fixtures.txt" 2>&1; then
  tail -1 "$WORK/fixtures.txt"
else
  grep -v '^ok ' "$WORK/fixtures.txt" | head -60; FAILS+=(fixtures)
fi

check() {  # check NAME DATA ANSWER
  local name=$1 data=$2 answer=$3 out="$WORK/out-$1" t0 t1
  t0=$(date +%s.%N)
  if ! "$RUN" "$data" "$out" > "$WORK/run-$name.log" 2>&1; then
    echo "FAIL $name: run exited nonzero"; tail -20 "$WORK/run-$name.log"; FAILS+=("$name"); return
  fi
  t1=$(date +%s.%N)
  if python3 "$ROOT/scripts/compare.py" "$out" "$answer" > "$WORK/cmp-$name.txt" 2>&1; then
    echo "ok   $name ($(awk "BEGIN{printf \"%.1f\", $t1-$t0}")s untimed; $(grep '^user_daily' "$WORK/cmp-$name.txt" | cut -d';' -f1))"
  else
    echo "FAIL $name"; cat "$WORK/cmp-$name.txt"; FAILS+=("$name")
  fi
  rm -rf "$out" 2>/dev/null
}

echo "== [2] check-dev"
check dev "$DATA_ROOT/dev" "$ROOT/dev-answer"

if [ -n "${VERIFY_SETS:-}" ]; then
  SETS=($VERIFY_SETS)
else
  SETS=(s2-1m s3-1m s15-1m-feb29)
  for d in "$DATA_ROOT"/adv-*; do [ -d "$d" ] && SETS+=("$(basename "$d")"); done
  [ "${QUICK:-0}" = 1 ] || SETS+=(s7-20m)
fi
echo "== [3] oracle diffs: ${SETS[*]}"
for ds in "${SETS[@]}"; do
  ans="$DATA_ROOT/oracle/$ds/out"
  if ! ls "$ans"/user_daily/*.parquet "$ans"/campaign_daily/*.parquet >/dev/null 2>&1; then
    if [ "${ALLOW_MISSING_ORACLE:-0}" = 1 ]; then echo "skip $ds: no oracle answer"; continue; fi
    echo "FAIL $ds: no oracle answer at $ans (run harness/oracle_cache.sh $ds)"; FAILS+=("$ds:no-oracle"); continue
  fi
  check "$ds" "$DATA_ROOT/$ds" "$ans"
done

if [ ${#FAILS[@]} -eq 0 ]; then
  echo "VERIFY PASS $COMMIT target=$TARGET sets=[dev ${SETS[*]}]"
  exit 0
fi
echo "VERIFY FAIL $COMMIT target=$TARGET: ${FAILS[*]}"
exit 1
