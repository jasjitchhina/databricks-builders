#!/bin/bash
# ab.sh DATA ROUNDS BIN1 BIN2 ...: interleaved runs under the exclusive bench lock on cores 0-7; prints clean wall, user, sys per run. Dev-only numbers.
D=$1; R=$2; shift 2
flock /tmp/bench.lock bash -c '
D=$1; R=$2; shift 2
for r in $(seq $R); do for b in "$@"; do
  o=$(NCPU=8 taskset -c 0-7 $b $D 2>&1)
  echo "$(basename $b) $(echo "$o" | grep -oE "clean: [0-9.]+" ) $(echo "$o" | grep -oE "user [0-9]+ ms sys [0-9]+ ms minflt [0-9]+ maxrss [0-9]+")"
done; done' _ "$D" "$R" "$@"
