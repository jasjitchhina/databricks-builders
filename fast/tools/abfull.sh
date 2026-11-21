#!/bin/bash
# abfull.sh DATA ROUNDS BIN1 BIN2 ...: interleaved end-to-end runs of the `fast` binary (DATA OUT), exclusive bench lock, cores 0-7 (or $CPUS). Prints process wall (ms). Dev-only numbers.
D=$1; R=$2; shift 2
CPUS=${CPUS:-0-7}
OUT=$(mktemp -d)
flock /tmp/bench.lock bash -c '
D=$1; R=$2; OUT=$3; CPUS=$4; shift 4
for r in $(seq $R); do for b in "$@"; do
  rm -rf $OUT/o; mkdir -p $OUT/o
  s=$(date +%s%N); NCPU=8 taskset -c $CPUS $b $D $OUT/o 2>/dev/null; e=$(date +%s%N)
  echo "$(basename $b) wall $(( (e-s)/1000000 )) ms"
done; done' _ "$D" "$R" "$OUT" "$CPUS" "$@"
rm -rf $OUT
