#!/usr/bin/env bash
# bench.sh: the ONLY sanctioned way to time the pipeline.
#
#   ./bench.sh [-n RUNS] [-i IMAGE | -r GITREV] [--no-build] [DATASET ...]
#
# -r GITREV builds that commit/branch (e.g. wt/h4, main) from a clean `git archive` export instead of
# this worktree, so any helper's committed state can be benched without touching their worktree.
# DATASET is a dir name under ~/data (dev, s7-20m, s11-60m, ...) or a path. Default: s7-20m.
# Builds the repo-root Dockerfile (shared lock), then takes the EXCLUSIVE lock /tmp/bench.lock
# and runs the image with the scorer's limits RUNS times (default 3) per dataset.
# Reports per-run container wall time (docker StartedAt->FinishedAt, the scorer's clock),
# docker-run wall time, peak cgroup memory (memory.peak, polled every 20 ms), and the median.
# If an oracle answer exists at ~/data/oracle/<dataset>/out/ the first run's output is diffed against it.
# Results are appended to ~/data/bench/results.tsv.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")" && pwd)
DATA_ROOT=${DATA_ROOT:-$HOME/data}
RUNS=3
REV=""
IMAGE=""
BUILD=1
DATASETS=()
while [ $# -gt 0 ]; do
  case "$1" in
    -n) RUNS=$2; shift 2 ;;
    -i) IMAGE=$2; BUILD=0; shift 2 ;;
    -r) REV=$2; shift 2 ;;
    --no-build) BUILD=0; shift ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) DATASETS+=("$1"); shift ;;
  esac
done
[ ${#DATASETS[@]} -gt 0 ] || DATASETS=(s7-20m)

CTX="$ROOT"
if [ -n "$REV" ]; then
  COMMIT=$(git -C "$ROOT" rev-parse --short "$REV^{commit}")
  BRANCH=$REV
  CTX=$(mktemp -d "$DATA_ROOT/scratch/benchctx.XXXXXX")
  git -C "$ROOT" archive "$COMMIT" | tar -x -C "$CTX"
  [ -n "$IMAGE" ] || IMAGE="bench-rev-$COMMIT:latest"
else
  COMMIT=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo nogit)
  DIRTY=$(git -C "$ROOT" status --porcelain 2>/dev/null | grep -v '^??' | head -1 || true)
  [ -n "$DIRTY" ] && COMMIT="$COMMIT-dirty"
  BRANCH=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo nogit)
fi

if [ -z "$IMAGE" ]; then
  IMAGE="bench-$(basename "$ROOT" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9_.\n-' '-'):latest"
fi
if [ "$BUILD" = 1 ]; then
  echo "== building $IMAGE from $CTX ($COMMIT)" >&2
  flock -s /tmp/bench.lock docker build -q --platform linux/amd64 -t "$IMAGE" "$CTX" >&2
fi
[ -n "$REV" ] && rm -rf "$CTX"

WORK="$DATA_ROOT/bench/work.$$"
mkdir -p "$DATA_ROOT/bench"
RESULTS="$DATA_ROOT/bench/results.tsv"
[ -f "$RESULTS" ] || printf 'when\tbranch\tcommit\timage\tdataset\truns\tmedian_s\tall_s\tpeak_rss_gb\tcheck\n' > "$RESULTS"

cleanup() {
  [ -n "${CID:-}" ] && docker rm -f "$CID" >/dev/null 2>&1 || true
  if [ -d "$WORK" ]; then
    # files written by uid 1000 may not be ours; fix perms via a throwaway container
    docker run --rm --network none --user 1000:1000 -v "$WORK":/w --entrypoint chmod "${HELPER_IMAGE:-harness-py:latest}" -R a+rwX /w >/dev/null 2>&1 || true
    rm -rf "$WORK" 2>/dev/null || true
  fi
}
trap cleanup EXIT

resolve() {
  if [ -d "$1" ]; then (cd "$1" && pwd); elif [ -d "$DATA_ROOT/$1" ]; then echo "$DATA_ROOT/$1"; else echo "no dataset $1" >&2; exit 2; fi
}

# exclusive lock for the timed section
exec 9>/tmp/bench.lock
echo "== waiting for exclusive /tmp/bench.lock" >&2
flock -x 9
echo "== got lock" >&2

median() { sort -g | awk '{a[NR]=$1} END{ if(NR%2) print a[(NR+1)/2]; else printf "%.3f\n", (a[NR/2]+a[NR/2+1])/2 }'; }

for DS in "${DATASETS[@]}"; do
  DATA=$(resolve "$DS")
  NAME=$(basename "$DATA")
  TIMES=(); PEAKS=(); CHECK="no-oracle"
  for ((r=1; r<=RUNS; r++)); do
    rm -rf "$WORK" 2>/dev/null || cleanup
    mkdir -p "$WORK/out" "$WORK/scratch"; chmod 777 "$WORK/out" "$WORK/scratch"
    sync
    T0=$(date +%s.%N)
    CID=$(docker create --platform linux/amd64 \
      --cpuset-cpus 0-7 --cpus 8 --memory 24g --memory-swap 24g \
      --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
      --cap-drop ALL --security-opt no-new-privileges --pids-limit 4096 \
      --user 1000:1000 \
      -v "$DATA":/data:ro -v "$WORK/out":/output -v "$WORK/scratch":/scratch \
      -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
      -e NCPU=8 -e MEM_GB=24 \
      "$IMAGE")
    docker start "$CID" >/dev/null
    CG="/sys/fs/cgroup/system.slice/docker-$CID.scope"
    PEAK=0
    # poll the container's cgroup (cheap; no docker CLI calls) until it disappears at exit
    while [ -d "$CG" ]; do
      v=$(cat "$CG/memory.peak" 2>/dev/null || echo 0); [ "$v" -gt "$PEAK" ] 2>/dev/null && PEAK=$v
      sleep 0.02
    done
    docker wait "$CID" >/dev/null
    T1=$(date +%s.%N)
    EXIT=$(docker inspect -f '{{.State.ExitCode}}' "$CID")
    OOM=$(docker inspect -f '{{.State.OOMKilled}}' "$CID")
    S=$(docker inspect -f '{{.State.StartedAt}}' "$CID"); F=$(docker inspect -f '{{.State.FinishedAt}}' "$CID")
    CT=$(python3 -c "
from datetime import datetime
def p(s):
    s=s.rstrip('Z'); a,b=(s.split('.')+['0'])[:2]; return datetime.fromisoformat(a).timestamp()+float('0.'+b[:9])
print('%.3f'%(p('$F')-p('$S')))")
    WALL=$(awk "BEGIN{printf \"%.3f\", $T1-$T0}")
    docker logs "$CID" > "$WORK/log.txt" 2>&1 || true
    docker rm "$CID" >/dev/null; CID=""
    PGB=$(awk "BEGIN{printf \"%.2f\", $PEAK/1073741824}")
    echo "$NAME run $r: container ${CT}s (docker-run wall ${WALL}s) peak ${PGB} GB exit $EXIT oom $OOM" >&2
    if [ "$EXIT" != 0 ] || [ "$OOM" = true ]; then
      echo "---- last log lines:" >&2; tail -20 "$WORK/log.txt" >&2
      printf '%s\t%s\t%s\t%s\t%s\t%s\tFAIL\t\t%s\texit=%s oom=%s\n' "$(date -Is)" "$BRANCH" "$COMMIT" "$IMAGE" "$NAME" "$r" "$PGB" "$EXIT" "$OOM" >> "$RESULTS"
      exit 1
    fi
    TIMES+=("$CT"); PEAKS+=("$PGB")
    if [ "$r" = 1 ] && [ -d "$DATA_ROOT/oracle/$NAME/out/user_daily" ]; then
      if PYTHONPATH=$HOME/pylib python3 "$ROOT/scripts/compare.py" "$WORK/out" "$DATA_ROOT/oracle/$NAME/out" > "$WORK/cmp.txt" 2>&1; then CHECK=MATCH; else CHECK=DIFFER; cat "$WORK/cmp.txt" >&2; fi
      echo "$NAME check vs oracle: $CHECK" >&2
    fi
  done
  MED=$(printf '%s\n' "${TIMES[@]}" | median)
  MAXP=$(printf '%s\n' "${PEAKS[@]}" | sort -g | tail -1)
  ALL=$(IFS=,; echo "${TIMES[*]}")
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -Is)" "$BRANCH" "$COMMIT" "$IMAGE" "$NAME" "$RUNS" "$MED" "$ALL" "$MAXP" "$CHECK" >> "$RESULTS"
  echo "RESULT $NAME commit=$COMMIT median=${MED}s runs=[$ALL] peak_rss=${MAXP}GB check=$CHECK"
done
