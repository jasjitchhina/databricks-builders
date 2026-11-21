#!/usr/bin/env bash
# mknothp.sh GITREV -> builds bench-rev-<sha> (bench.sh -r) and nothp-<sha> (THP disabled via prctl wrapper) images
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); sha=$(git -C $ROOT rev-parse --short "$1^{commit}")
W=$(mktemp -d ~/data/scratch/nothp.XXXX); trap 'rm -rf "$W"' EXIT
gcc -O2 -static -o "$W/nothp" $ROOT/harness/floor/nothp.c
$ROOT/bench.sh -r "$sha" -n 1 dev >/dev/null 2>&1
printf 'FROM bench-rev-%s:latest\nCOPY nothp /app/nothp\nCMD ["/app/nothp","/app/fast"]\n' "$sha" > "$W/Dockerfile"
flock -s /tmp/bench.lock docker build -q --cpuset-cpus 8-15,24-31 -t "nothp-$sha:latest" "$W" >/dev/null
echo "nothp-$sha:latest"
