#!/usr/bin/env bash
# soak.sh — continuous random-seed regression of main (task 36). Runs forever; safe to restart (state in ~/data/soak).
#   nohup setsid harness/soak.sh > ~/data/soak/soak.out 2>&1 &
# Per seed (from 1000 up, shared counter, $WORKERS in parallel): generate a ~1M dataset with randomized
# --events/--clicks, run main's oracle (ground truth) and main's fast binary (native, NCPU=8, OUT wiped first),
# compare with scripts/compare.py; every 10th seed also h1's independent xcheck vs oracle.
# Task 62: scratch modes rotate per seed (FAST_NO_CACHE / fresh / persistent per-worker scratch + replay), see one_seed.
# Failures: dataset kept at ~/data/soak/fail-<seed>, note inserted into team.db at once.
# When main moves: build main's Docker image (the scorer binary, PGO) and run that binary natively, then a no-THP + cold bench of main
# via bench.sh (exclusive lock, CPUs 0-7), interleaved with the previous landing; note only if either regresses >5%.
# Everything except bench.sh runs on CPUs $SOAK_CPUS (default 12-15,28-31).
set -uo pipefail
H=$(cd "$(dirname "$0")" && pwd); ROOT=${SOAK_ROOT:-$(dirname "$H")}
# run from a private copy so editing harness/soak.sh never corrupts the running loop
if [ -z "${SOAK_COPY:-}" ]; then
  mkdir -p "$HOME/data/soak"; cp "$0" "$HOME/data/soak/run.sh"
  SOAK_COPY=1 SOAK_ROOT="$ROOT" SOAK_H="$H" exec bash "$HOME/data/soak/run.sh" "$@"
fi
H=${SOAK_H:-$H}
D=$HOME/data/soak; mkdir -p "$D"
DB=/home/jessechhina/databricks-builders/team.db
CPUS=${SOAK_CPUS:-12-15,28-31}; WORKERS=${WORKERS:-3}
GEN=$HOME/data/bin/generate
export PYTHONPATH=$HOME/pylib
log() { echo "$(date +%T) $*" | tee -a "$D/soak.log"; }
note() { sqlite3 "$DB" "insert into notes(agent,task_id,msg) values('h2',36,'$(echo "$*" | sed "s/'/''/g")');"; }

next_seed() {  # atomic shared counter
  ( flock -x 8; s=$(cat "$D/next_seed" 2>/dev/null || echo 1000); echo $((s+1)) > "$D/next_seed"; echo "$s" ) 8>"$D/.seed.lock"
}

build_main() {  # -> $D/bin/<sha>/{fast,oracle/}; fast = the SCORER binary (/app/fast from the Dockerfile
  # image, i.e. PGO + pinned toolchain), not a native cargo build
  local sha=$1 src=$D/src-$1 cid
  [ -x "$D/bin/$sha/fast" ] && return 0
  rm -rf "$src"; mkdir -p "$src" "$D/bin/$sha"
  git -C "$ROOT" archive "$sha" | tar -x -C "$src"
  if ! "$ROOT/bench.sh" -r "$sha" -n 1 dev >"$D/build-$sha.log" 2>&1 || ! cid=$(docker create "bench-rev-$sha:latest" 2>>"$D/build-$sha.log"); then
    log "BUILD FAIL $sha"; note "SOAK: docker build of main $sha FAILED: see $D/build-$sha.log"; return 1
  fi
  docker cp "$cid:/app/fast" "$D/bin/$sha/fast" >/dev/null; docker rm "$cid" >/dev/null
  cp -r "$src/oracle" "$src/oracle.sh" "$D/bin/$sha/"
  rm -rf "$src"
}

bench_landing() {  # no-THP + cold bench of main $1 vs the previous landing, INTERLEAVED in the same window
  # (stored baselines drift with machine load: a 10% false alarm came from comparing against a quieter hour)
  local sha=$1 psha="" a b c d
  [ -f "$D/landing.last" ] && read -r psha _ < "$D/landing.last"
  med() { "$ROOT/bench.sh" "$@" 2>/dev/null | grep RESULT | grep -oE 'median=[0-9.]+' | cut -d= -f2; }
  "$H/mknothp.sh" "$sha" >/dev/null 2>&1 || true   # builds bench-rev-/nothp- images
  [ -n "$psha" ] && [ "$psha" != "$sha" ] && { "$H/mknothp.sh" "$psha" >/dev/null 2>&1 || true; }
  if [ -n "$psha" ] && [ "$psha" != "$sha" ]; then
    local an=0 bn=0 ac=0 bc=0
    for round in 1 2; do
      a=$(med -i "nothp-$psha:latest" -n 3 s7-20m); b=$(med -i "nothp-$sha:latest" -n 3 s7-20m)
      c=$(med -r "$psha" --no-build --cold -n 1 s7-20m); d=$(med -r "$sha" --no-build --cold -n 1 s7-20m)
      an=$(awk -v x="$an" -v y="${a:-0}" 'BEGIN{print x+y}'); bn=$(awk -v x="$bn" -v y="${b:-0}" 'BEGIN{print x+y}')
      ac=$(awk -v x="$ac" -v y="${c:-0}" 'BEGIN{print x+y}'); bc=$(awk -v x="$bc" -v y="${d:-0}" 'BEGIN{print x+y}')
    done
    log "landing $sha vs $psha (interleaved, sums of 2 medians): noTHP $bn vs $an; cold $bc vs $ac"
    awk -v a="$an" -v b="$bn" -v c="$ac" -v d="$bc" 'BEGIN{exit !((a>0 && b>a*1.05) || (c>0 && d>c*1.05))}' && \
      note "SOAK REGRESSION >5% main $sha vs $psha, benched INTERLEAVED in the same window (2 rounds): no-THP 20M cpus 0-7 sum of medians ${bn}s vs ${an}s; cold 20M ${bc}s vs ${ac}s."
  else
    log "landing $sha: baseline set"
  fi
  echo "$sha" > "$D/landing.last"
}

one_seed() {  # one_seed SEED SHA WORKER
  local seed=$1 sha=$2 wk=$3 R=$D/w-$1 ev cl rc=0
  local h; h=$(printf '%s' "$seed" | cksum | cut -d' ' -f1)
  ev=$(( 600000 + h % 900001 ))                         # 0.6M..1.5M events, hashed per seed
  cl=$(( ev / (2 + (h / 900001) % 7) ))                 # clicks = events/2 .. events/8
  rm -rf "$R"; mkdir -p "$R"
  taskset -c "$CPUS" "$GEN" --seed "$seed" --events "$ev" --clicks "$cl" --output "$R/data" >/dev/null 2>&1 || { log "seed $seed: generate failed"; return; }
  taskset -c "$CPUS" "$D/bin/$sha/oracle.sh" "$R/data" "$R/oracle" > "$R/oracle.log" 2>&1 || { log "seed $seed: ORACLE CRASH"; note "SOAK: oracle crashed on seed $seed ($ev/$cl); data kept at $D/fail-$seed"; mv "$R" "$D/fail-$seed"; return; }
  # task 62 replay cache. seed%4==0: FAST_NO_CACHE=1, fresh scratch; seed%4==2: fresh scratch (cache on, store only);
  # odd seeds: the worker's PERSISTENT scratch (holds the previous seed's = another dataset's entry: must not be
  # replayed), then a REPLAY run on that same scratch into an output dir seeded with stale parquet (must be
  # row-exact and byte-identical to the first run).
  local mode scr="$R/scr" cenv=""
  case $((seed % 4)) in 0) mode=nocache; cenv="FAST_NO_CACHE=1";; 2) mode=fresh;; *) mode=replay; scr="$D/wscr-$wk";; esac
  mkdir -p "$scr"
  runcmp() {  # runcmp LABEL OUTDIR [ENV..] -> 0 ok; on failure: note, keep data, return 1
    local lab=$1 out=$2; shift 2; rc=0
    env "$@" DATA_DIR="$R/data" OUTPUT_DIR="$out" SCRATCH_DIR="$scr" NCPU=8 MEM_GB=24 taskset -c "$CPUS" "$D/bin/$sha/fast" > "$R/fast-$lab.log" 2>&1 || rc=$?
    if [ $rc != 0 ]; then
      log "seed $seed: FAST EXIT $rc [$lab]"; note "SOAK FAILURE: main $sha fast exited $rc on seed $seed (--events $ev --clicks $cl) [$lab run, scratch mode $mode]. Data+logs kept at $D/fail-$seed"; cp -r "$scr" "$R/scratch-copy" 2>/dev/null; mv "$R" "$D/fail-$seed"; return 1
    fi
    if ! python3 "$ROOT/scripts/compare.py" "$out" "$R/oracle" > "$R/cmp-$lab.txt" 2>&1; then
      log "seed $seed: DIFFER [$lab]"; note "SOAK FAILURE: main $sha DIFFERS from oracle on seed $seed (--events $ev --clicks $cl) [$lab run, scratch mode $mode, $("$H/cacheline" "$R/fast-$lab.log")]: $(grep -E '^(user|campaign)_daily' "$R/cmp-$lab.txt" | tr '\n' ' '). Data+outputs kept at $D/fail-$seed"; cp -r "$scr" "$R/scratch-copy" 2>/dev/null; mv "$R" "$D/fail-$seed"; return 1
    fi
  }
  rm -rf "$R/out"; mkdir -p "$R/out"
  runcmp first "$R/out" FAST_VERBOSE=1 $cenv || return
  local cache1 cache2=""; cache1=$("$H/cacheline" "$R/fast-first.log" | cut -c1-60)
  if [ "$mode" = replay ]; then
    case "$cache1" in "cache: hit"*) log "seed $seed: REPLAYED ANOTHER DATASET"; note "SOAK FAILURE: main $sha replayed a cache entry on seed $seed whose scratch held a DIFFERENT dataset (previous seed of worker $wk) - output still matched the oracle by luck? Kept at $D/fail-$seed"; mv "$R" "$D/fail-$seed"; return;; esac
    mkdir -p "$R/out2/user_daily" "$R/out2/campaign_daily"
    for d in user_daily campaign_daily; do f=$(ls "$R/out/$d"/*.parquet 2>/dev/null | head -1); [ -n "$f" ] && cp "$f" "$R/out2/$d/part-99999-stale.parquet"; done
    runcmp replay "$R/out2" FAST_VERBOSE=1 || return
    cache2=$("$H/cacheline" "$R/fast-replay.log" | cut -c1-60)
    if [ "${cache2#cache: hit}" != "$cache2" ] && ! diff -rq "$R/out" "$R/out2" > "$R/rdiff.txt" 2>&1; then
      log "seed $seed: REPLAY NOT BYTE-IDENTICAL"; note "SOAK FAILURE: main $sha replay output on seed $seed is row-exact but not byte-identical to the full run ($cache2). Kept at $D/fail-$seed"; mv "$R" "$D/fail-$seed"; return
    fi
  fi
  local xc=""
  if [ $((seed % 10)) = 0 ]; then
    if taskset -c "$CPUS" timeout 900 python3 "$D/bin/$sha/oracle/xcheck.py" "$R/data" "$R/xc" > "$R/xc.log" 2>&1 && python3 "$ROOT/scripts/compare.py" "$R/xc" "$R/oracle" > "$R/xcmp.txt" 2>&1; then xc=" xcheck=MATCH"
    else xc=" xcheck=DIFFER"; note "SOAK: xcheck vs oracle DIFFERS (or failed) on seed $seed ($ev/$cl): oracle and xcheck disagree - investigate before trusting oracle. Kept at $D/fail-xc-$seed"; cp -r "$R" "$D/fail-xc-$seed"; fi
  fi
  log "seed $seed ok ($ev ev / $cl cl, main $sha, scratch $mode: ${cache1#cache: }${cache2:+ -> ${cache2#cache: }})$xc"
  rm -rf "$R"
}

worker() {  # worker ID
  while true; do
    local sha; sha=$(cat "$D/current_sha")
    one_seed "$(next_seed)" "$sha" "$1"
  done
}

log "soak start pid $$ workers=$WORKERS cpus=$CPUS"
sha=$(git -C "$ROOT" rev-parse --short main); build_main "$sha" && echo "$sha" > "$D/current_sha"
for ((w=0; w<WORKERS; w++)); do worker "$w" & done
[ -f "$D/landing.last" ] || bench_landing "$sha"   # baseline for regression checks
while true; do  # watch main
  sleep 60
  new=$(git -C "$ROOT" rev-parse --short main)
  if [ "$new" != "$(cat "$D/current_sha")" ]; then
    log "main moved -> $new"
    if build_main "$new"; then echo "$new" > "$D/current_sha"; bench_landing "$new"; fi
  fi
done
