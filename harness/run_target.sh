#!/usr/bin/env bash
# run_target.sh DATA_DIR OUTPUT_DIR — runs $TARGET on one dataset.
#   TARGET=/path/to/executable  -> runs it natively with DATA_DIR/OUTPUT_DIR/SCRATCH_DIR env (+ args DATA OUT)
#   TARGET=image:tag            -> docker run with the scorer's flags (8 cpu, 24g, ro root, uid 1000, no net)
# Takes the SHARED /tmp/bench.lock for the duration of the run.
set -euo pipefail
DATA=$(cd "$1" && pwd)
mkdir -p "$2"; OUT=$(cd "$2" && pwd)
SCR_ROOT=${SCRATCH_ROOT:-$HOME/data/scratch}
mkdir -p "$SCR_ROOT"
SCRATCH=$(mktemp -d "$SCR_ROOT/run.XXXXXX")
if [ -x "$TARGET" ] && [ -f "$TARGET" ]; then
  trap 'rm -rf "$SCRATCH"' EXIT
  DATA_DIR="$DATA" OUTPUT_DIR="$OUT" SCRATCH_DIR="$SCRATCH" HOME="$SCRATCH" NCPU=${NCPU:-8} MEM_GB=${MEM_GB:-24} \
    flock -s /tmp/bench.lock "$TARGET" "$DATA" "$OUT"
else
  chmod 777 "$OUT" "$SCRATCH"
  release() {
    if [ -n "$(find "$OUT" "$SCRATCH" ! -user "$(id -u)" -print -quit 2>/dev/null)" ]; then
      docker run --rm --network none --user 1000:1000 -v "$OUT":/output -v "$SCRATCH":/scratch \
        --entrypoint chmod "$TARGET" -R a+rwX /output /scratch >/dev/null 2>&1 || true
    fi
    rm -rf "$SCRATCH" 2>/dev/null || true
  }
  trap release EXIT
  flock -s /tmp/bench.lock docker run --rm --platform linux/amd64 \
    --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
    --cap-drop ALL --security-opt no-new-privileges --pids-limit 4096 \
    --cpus 8 --memory 24g --memory-swap 24g --user 1000:1000 \
    -v "$DATA":/data:ro -v "$OUT":/output -v "$SCRATCH":/scratch \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
    -e NCPU=8 -e MEM_GB=24 "$TARGET"
fi
