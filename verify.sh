#!/usr/bin/env bash
# verify.sh: correctness gate. Nothing merges to main unless this exits 0.
#
#   ./verify.sh [TARGET]        or   ./verify.sh -r GITREV   (builds that commit from git archive)
#
# TARGET: an executable (run natively, gets DATA_DIR/OUTPUT_DIR/SCRATCH_DIR env and args DATA OUT)
#         or a docker image tag. Default: build the repo-root Dockerfile as verify-<worktree>:latest.
# Checks, in order (all run; exit 1 if any fails):
#   1. all fixtures/                      (expected CSVs, scripts/run_fixtures.py)
#   2. check-dev                          (~/data/dev vs dev-answer/)
#   3. row-exact diff vs the oracle cache (~/data/oracle/<ds>/out/, h1's layout) on $VERIFY_SETS
#      default: s2-1m s3-1m s15-1m-feb29 + every ~/data/adv-* + s7-20m (20M) + the big sets listed below (20M/60M/100M) once their oracle DONE exists
# Env: QUICK=1 skips the 20M set. VERIFY_SETS="a b" overrides the oracle sets.
#      ALLOW_MISSING_ORACLE=1 skips (instead of failing) sets with no oracle answer yet.
# Builds and runs take the SHARED /tmp/bench.lock. Oracle answers are made by harness/oracle_cache.sh.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")" && pwd)
DATA_ROOT=${DATA_ROOT:-$HOME/data}
export PYTHONPATH=$HOME/pylib${PYTHONPATH:+:$PYTHONPATH}
TARGET=${1:-}
REV=HEAD
COMMIT=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null)
if [ "$TARGET" = "-r" ]; then
  REV=${2:?usage: verify.sh -r GITREV}
  COMMIT=$(git -C "$ROOT" rev-parse --short "$REV^{commit}") || exit 1
  CTX=$(mktemp -d "$DATA_ROOT/scratch/verifyctx.XXXXXX")
  git -C "$ROOT" archive "$COMMIT" | tar -x -C "$CTX"
  TARGET="verify-rev-$COMMIT:latest"
  echo "== building $TARGET from $REV ($COMMIT)" >&2
  flock -s /tmp/bench.lock docker build -q --cpuset-cpus 8-15,24-31 --platform linux/amd64 -t "$TARGET" "$CTX" >&2 || { echo "VERIFY FAIL: build"; rm -rf "$CTX"; exit 1; }
  rm -rf "$CTX"
elif [ -z "$TARGET" ]; then
  TARGET="verify-$(basename "$ROOT" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9_.\n-' '-'):latest"
  echo "== building $TARGET" >&2
  flock -s /tmp/bench.lock docker build -q --cpuset-cpus 8-15,24-31 --platform linux/amd64 -t "$TARGET" "$ROOT" >&2 || { echo "VERIFY FAIL: build"; exit 1; }
elif [ -f "$TARGET" ]; then
  TARGET=$(cd "$(dirname "$TARGET")" && pwd)/$(basename "$TARGET")
fi
export TARGET
RUN="$ROOT/harness/run_target.sh"
WORK=$(mktemp -d "$DATA_ROOT/scratch/verify.XXXXXX" 2>/dev/null || { mkdir -p "$DATA_ROOT/scratch"; mktemp -d "$DATA_ROOT/scratch/verify.XXXXXX"; })
trap 'rm -rf "$WORK" 2>/dev/null' EXIT
FAILS=()

echo "== [0] lint ($REV)"
LINT=$(git -C "$ROOT" grep -nIE 'target-cpu *= *native|target-cpu=native|-march=native|avx512' "$REV" -- ':!verify.sh' ':!*.md' ':!fast/vendor/**' ':!harness/**' 2>/dev/null || true)
if [ -n "$LINT" ]; then echo "FAIL lint: native/avx512 codegen flags found:"; echo "$LINT"; FAILS+=(lint-native); fi
SIZE_KB=$(git -C "$ROOT" ls-tree -r -l "$REV" | awk '{s+=$4} END{printf "%d", s/1024}')
if [ "${SIZE_KB:-0}" -gt 45000 ]; then echo "FAIL lint: tracked files ${SIZE_KB} KB (limit 50 MB)"; FAILS+=(lint-size); fi
BIG=$(git -C "$ROOT" ls-tree -r --name-only "$REV" | grep -E '\.(parquet|csv)$' | grep -vE '^(fixtures|dev-answer)/' || true)
if [ -n "$BIG" ]; then echo "FAIL lint: data files tracked: $BIG"; FAILS+=(lint-data); fi
[ ${#FAILS[@]} -eq 0 ] && echo "ok   lint (tracked ${SIZE_KB} KB)"
# binary-level ISA check: the image entrypoint must contain no AVX-512 (zmm, EVEX hi regs, opmask regs)
if [ ! -f "$TARGET" ] && command -v objdump >/dev/null; then
  BIN_IN=$(docker image inspect -f '{{index .Config.Cmd 0}}' "$TARGET" 2>/dev/null)
  CID=$(docker create "$TARGET" 2>/dev/null) && docker cp "$CID:$BIN_IN" "$WORK/entry.bin" >/dev/null 2>&1; [ -n "${CID:-}" ] && docker rm "$CID" >/dev/null
  if [ -f "$WORK/entry.bin" ] && file "$WORK/entry.bin" | grep -q ELF; then
    N512=$(objdump -d --no-show-raw-insn "$WORK/entry.bin" | grep -cE 'zmm|[xy]mm(1[6-9]|2[0-9]|3[01])|%k[1-7]' || true)
    if [ "$N512" -gt 0 ]; then echo "FAIL isa: $BIN_IN has $N512 AVX-512 instructions"; FAILS+=(isa-avx512); else echo "ok   isa ($BIN_IN: no AVX-512)"; fi
  else echo "warn isa: entrypoint $BIN_IN is not an ELF binary; skipped"; fi
fi

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
  if [ "$name" = dev ] || [ "$name" = adv-synth-s2 ]; then  # independent Parquet readers (pyarrow full validate + DuckDB)
    docker image inspect harness-py:latest >/dev/null 2>&1 || flock -s /tmp/bench.lock docker build -q --cpuset-cpus 8-15,24-31 -t harness-py:latest "$ROOT/harness/pyimg" >/dev/null
    if docker run --rm --network none --cpuset-cpus 8-15,24-31 -v "$out":/o:ro -v "$ROOT/harness":/h:ro harness-py:latest python3 /h/pqcheck.py /o > "$WORK/pq-$name.txt" 2>&1; then
      echo "ok   $name parquet readers (pyarrow+duckdb)"
    else echo "FAIL $name parquet readers"; cat "$WORK/pq-$name.txt"; FAILS+=("$name:parquet"); fi
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
  if [ "${QUICK:-0}" != 1 ]; then
    SETS+=(s7-20m)
    # extra big sets join automatically once their oracle answer exists
    for x in s8-20m s62-20m s41-20m s11-60m s12-60m s43-60m s1993605-60m s2220774-60m s2824689-60m s21-100m s1345955-100m; do [ -f "$DATA_ROOT/oracle/$x/DONE" ] && SETS+=("$x"); done
  fi
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
