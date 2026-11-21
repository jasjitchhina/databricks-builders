#!/usr/bin/env bash
# pgo.sh [GITREV] — regenerate fast/pgo/fast.profdata (task 52) for the source at GITREV (default HEAD).
# The instrumented build runs in the SAME pinned rust:1.100.0 image and at the SAME path (/src/fast) as the
# Dockerfile build, so symbol names / crate hashes match the profile-use build exactly. Training runs the
# instrumented binary on a mix of datasets (warm, NCPU=8, also the 512-bucket and wide-u128 paths), then
# (FAST_NO_ZAP=1: exit via exit(0) so the profile runtime writes its file; the _exit path skips it) merges with the host llvm-profdata of the matching LLVM (rustup llvm-tools of rustc 1.100.0).
# A stale/mismatched profile only costs speed: PGO never changes semantics (still run ./verify.sh).
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(dirname "$H")
REV=${1:-HEAD}; SHA=$(git -C "$ROOT" rev-parse --short "$REV^{commit}")
IMG=$(sed -n 's/^FROM \(rust:[^ ]*\) AS build/\1/p' "$ROOT/Dockerfile")
PROFDATA=$(ls ~/.rustup/toolchains/*/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata | head -1)
SETS=${PGO_SETS:-"s7-20m s11-60m s2-1m adv-synth-s2 adv-perturb-s21 adv-shape-longids adv-shape-manyfiles dev"}
CPUS=${PGO_CPUS:-8-15,24-31}
W=$(mktemp -d "$HOME/data/scratch/pgo.XXXX"); trap 'rm -rf "$W"' EXIT
mkdir -p "$W/src" "$W/raw" "$W/out"
git -C "$ROOT" archive "$SHA" fast | tar -x -C "$W/src"
echo "pgo: instrumented build of $SHA in $IMG" >&2
flock -s /tmp/bench.lock docker run --rm --network none --cpuset-cpus "$CPUS" --user "$(id -u):$(id -g)" \
  -e CARGO_HOME=/tmp/cargo -v "$W/src:/src" -w /src/fast "$IMG" \
  bash -c 'RUSTFLAGS="-C target-cpu=x86-64-v3 -Cprofile-generate=/pgo-raw" cargo build --release --locked --offline --bin fast -q && cp target/release/fast /src/fast-instr'
for ds in $SETS; do
  for envv in "" ${PGO_EXTRA_ENV-FAST_NB_BITS=9}; do
    [ -n "$envv" ] && [ "$ds" != s7-20m ] && continue   # extra-env variant only on one set
    rm -rf "$W/out"/*
    echo "pgo: train $ds ${envv:-}" >&2
    flock -s /tmp/bench.lock docker run --rm --network none --cpuset-cpus "$CPUS" --user "$(id -u):$(id -g)" \
      -v "$W/src:/src:ro" -v "$HOME/data/$ds:/data:ro" -v "$W/out:/output" -v "$W/raw:/pgo-raw" \
      -e LLVM_PROFILE_FILE=/pgo-raw/fast-%p-%m.profraw -e NCPU=8 -e FAST_NO_ZAP=1 ${envv:+-e "$envv"} \
      "$IMG" /src/fast-instr /data /output >/dev/null
  done
done
mkdir -p "$ROOT/fast/pgo"
"$PROFDATA" merge -o "$ROOT/fast/pgo/fast.profdata" "$W"/raw/*.profraw
echo "$SHA $(date -u +%FT%TZ) sets: $SETS" > "$ROOT/fast/pgo/SOURCE"
ls -la "$ROOT/fast/pgo/" >&2
