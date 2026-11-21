#!/usr/bin/env bash
# nothp_ab.sh GITREV [GITREV2 ...] — THP on vs THP=never (prctl wrapper) A/B via bench.sh, both cpusets.
# For each rev: builds bench-rev-<sha> (bench.sh -r), wraps it as nothp-<sha> (harness/floor/nothp, PR_SET_THP_DISABLE
# inherited across exec), then benches {thp,nothp} x {0-7, 0-3,16-19} on $SETS (default s7-20m) with RUNS (default 3).
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(dirname "$H"); SETS=${SETS:-s7-20m}; RUNS=${RUNS:-3}
W=$(mktemp -d "$HOME/data/scratch/nothp.XXXX"); trap 'rm -rf "$W"' EXIT
gcc -O2 -static -o "$W/nothp" "$H/floor/nothp.c"
for rev in "$@"; do
  sha=$(git -C "$ROOT" rev-parse --short "$rev^{commit}")
  "$ROOT/bench.sh" -r "$sha" -n 1 dev >/dev/null 2>&1 || { echo "build/bench of $sha failed"; exit 1; }
  printf 'FROM bench-rev-%s:latest\nCOPY nothp /app/nothp\nCMD ["/app/nothp","/app/fast"]\n' "$sha" > "$W/Dockerfile"
  flock -s /tmp/bench.lock docker build -q --cpuset-cpus 8-15,24-31 -t "nothp-$sha:latest" "$W" >/dev/null
  for cs in 0-7 0-3,16-19; do for img in bench-rev-$sha nothp-$sha; do
    BENCH_CPUSET=$cs "$ROOT/bench.sh" -i "$img:latest" -n "$RUNS" $SETS 2>&1 | grep RESULT | sed "s/^RESULT /$img cpus=$cs /; s/ commit=image//" | cut -c1-150
  done; done
done
