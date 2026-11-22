#!/usr/bin/env bash
# run_target.sh DATA_DIR OUTPUT_DIR — runs $TARGET on one dataset.
#   TARGET=/path/to/executable  -> runs it natively with DATA_DIR/OUTPUT_DIR/SCRATCH_DIR env (+ args DATA OUT)
#   TARGET=image:tag            -> docker run with the scorer's flags (8 cpu, 24g, ro root, uid 1000, no net)
# Takes the SHARED /tmp/bench.lock for the duration of the run.
# Optional env (task 62 replay-cache tests):
#   TARGET_ENV="K=V ..."  extra env for the target
#   RT_SCRATCH=DIR        use DIR as scratch and KEEP it (caller owns it); default: a fresh temp dir, deleted after
#   RT_SCRATCH_MODE=rw (default) | ro (scratch mounted/chmod read-only) | missing (SCRATCH_DIR points to a
#                   nonexistent path) | unset (no SCRATCH_DIR at all) | vol:NAME (docker volume NAME at /scratch,
#                   e.g. a tiny tmpfs volume = full scratch; docker targets only)
set -euo pipefail
DATA=$(cd "$1" && pwd)
mkdir -p "$2"; OUT=$(cd "$2" && pwd)
SCR_ROOT=${SCRATCH_ROOT:-$HOME/data/scratch}
mkdir -p "$SCR_ROOT"
MODE=${RT_SCRATCH_MODE:-rw}
if [ -n "${RT_SCRATCH:-}" ]; then mkdir -p "$RT_SCRATCH"; SCRATCH=$(cd "$RT_SCRATCH" && pwd); OWN=0
else SCRATCH=$(mktemp -d "$SCR_ROOT/run.XXXXXX"); OWN=1; fi
read -r -a TENV <<< "${TARGET_ENV:-}"
if [ -x "$TARGET" ] && [ -f "$TARGET" ]; then
  cleanup_n() { [ "$MODE" = ro ] && chmod -R u+w "$SCRATCH" 2>/dev/null; [ "$OWN" = 1 ] && rm -rf "$SCRATCH"; true; }
  trap cleanup_n EXIT
  SENV=(SCRATCH_DIR="$SCRATCH")
  case $MODE in
    rw) ;;
    ro) chmod -R a-w "$SCRATCH" ;;
    missing) SENV=(SCRATCH_DIR="$SCRATCH/does-not-exist") ;;
    unset) SENV=() ;;
    *) echo "run_target: RT_SCRATCH_MODE=$MODE not supported for native targets" >&2; exit 2 ;;
  esac
  env -u SCRATCH_DIR DATA_DIR="$DATA" OUTPUT_DIR="$OUT" "${SENV[@]}" HOME="$SCRATCH" NCPU=${NCPU:-8} MEM_GB=${MEM_GB:-24} "${TENV[@]}" \
    flock -s /tmp/bench.lock "$TARGET" "$DATA" "$OUT"
else
  chmod 777 "$OUT" "$SCRATCH"; chmod -R a+rwX "$OUT" 2>/dev/null || true   # pre-seeded subdirs must be writable by uid 1000
  release() {
    # always hand foreign-owned (uid 1000) files back so the caller can inspect/modify a kept scratch
    if [ -n "$(find "$OUT" "$SCRATCH" ! -user "$(id -u)" -print -quit 2>/dev/null)" ]; then
      docker run --rm --network none --user 0:0 -v "$OUT":/output -v "$SCRATCH":/scratch \
        --entrypoint chmod "${HELPER_IMAGE:-python:3.12-slim-bookworm}" -R a+rwX /output /scratch >/dev/null 2>&1 || true
    fi
    [ "$OWN" = 1 ] && rm -rf "$SCRATCH" 2>/dev/null; true
  }
  trap release EXIT
  EENV=(); for kv in "${TENV[@]}"; do EENV+=(-e "$kv"); done
  SMNT=(-v "$SCRATCH":/scratch); SENV=(-e SCRATCH_DIR=/scratch)
  case $MODE in
    rw) ;;
    ro) SMNT=(-v "$SCRATCH":/scratch:ro) ;;
    missing) SMNT=(); SENV=(-e SCRATCH_DIR=/scratch/does-not-exist) ;;
    unset) SMNT=(); SENV=() ;;
    vol:*) SMNT=(-v "${MODE#vol:}":/scratch) ;;
    *) echo "run_target: unknown RT_SCRATCH_MODE=$MODE" >&2; exit 2 ;;
  esac
  flock -s /tmp/bench.lock docker run --rm --platform linux/amd64 --cpuset-cpus "${VERIFY_CPUS:-8-15,24-31}" \
    --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
    --cap-drop ALL --security-opt no-new-privileges --pids-limit 4096 \
    --cpus 8 --memory 24g --memory-swap 24g --user 1000:1000 \
    -v "$DATA":/data:ro -v "$OUT":/output "${SMNT[@]}" \
    -e DATA_DIR=/data -e OUTPUT_DIR=/output "${SENV[@]}" -e HOME=/scratch \
    -e NCPU=8 -e MEM_GB=24 "${EENV[@]}" "$TARGET"
fi
