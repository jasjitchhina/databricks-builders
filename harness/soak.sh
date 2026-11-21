#!/usr/bin/env bash
# soak.sh — continuous random-seed regression of main (task 36). Runs forever; safe to restart (state in ~/data/soak).
#   nohup setsid harness/soak.sh > ~/data/soak/soak.out 2>&1 &
# Per seed (from 1000 up, shared counter, $WORKERS in parallel): generate a ~1M dataset with randomized
# --events/--clicks, run main's oracle (ground truth) and main's fast binary (native, NCPU=8, OUT wiped first),
# compare with scripts/compare.py; every 10th seed also h1's independent xcheck vs oracle.
# Failures: dataset kept at ~/data/soak/fail-<seed>, note inserted into team.db at once.
# When main moves: rebuild binary from `git archive main` (offline, vendored), then a no-THP + cold bench of main
# via bench.sh (exclusive lock, CPUs 0-7); a note is posted only if either regresses >5% vs the previous landing.
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

build_main() {  # -> $D/bin/<sha>/{fast,oracle/}
  local sha=$1 src=$D/src-$1
  [ -x "$D/bin/$sha/fast" ] && return 0
  rm -rf "$src"; mkdir -p "$src" "$D/bin/$sha"
  git -C "$ROOT" archive "$sha" | tar -x -C "$src"
  if ! (cd "$src/fast" && taskset -c "$CPUS" cargo build --release --offline --locked --bin fast -q) >"$D/build-$sha.log" 2>&1; then
    log "BUILD FAIL $sha"; note "SOAK: build of main $sha FAILED natively (cargo --offline --locked): see $D/build-$sha.log"; return 1
  fi
  cp "$src/fast/target/release/fast" "$D/bin/$sha/fast"
  cp -r "$src/oracle" "$src/oracle.sh" "$D/bin/$sha/"
  rm -rf "$src"
}

bench_landing() {  # no-THP + cold bench of main $1 (20M), compare with previous landing
  local sha=$1 out prev
  out=$("$H/nothp_ab.sh" "$sha" 2>/dev/null | grep -E '^nothp-.* cpus=0-7 ' | grep -oE 'median=[0-9.]+' | cut -d= -f2)
  local cold; cold=$("$ROOT/bench.sh" -r "$sha" --no-build --cold -n 3 s7-20m 2>/dev/null | grep RESULT | grep -oE 'median=[0-9.]+' | cut -d= -f2)
  log "landing $sha: noTHP 20M ${out:-?}s cold 20M ${cold:-?}s"
  if [ -f "$D/landing.last" ]; then
    read -r psha pnothp pcold < "$D/landing.last"
    awk -v a="${out:-0}" -v b="$pnothp" -v c="${cold:-0}" -v d="$pcold" 'BEGIN{exit !((b>0 && a>b*1.05) || (d>0 && c>d*1.05))}' && \
      note "SOAK REGRESSION >5% main $sha vs $psha: no-THP 20M (cpus 0-7) ${out}s vs ${pnothp}s; cold 20M ${cold}s vs ${pcold}s (bench.sh medians of 3)."
  fi
  echo "$sha ${out:-0} ${cold:-0}" > "$D/landing.last"
}

one_seed() {  # one_seed SEED SHA
  local seed=$1 sha=$2 R=$D/w-$1 ev cl rc=0
  local h; h=$(printf '%s' "$seed" | cksum | cut -d' ' -f1)
  ev=$(( 600000 + h % 900001 ))                         # 0.6M..1.5M events, hashed per seed
  cl=$(( ev / (2 + (h / 900001) % 7) ))                 # clicks = events/2 .. events/8
  rm -rf "$R"; mkdir -p "$R"
  taskset -c "$CPUS" "$GEN" --seed "$seed" --events "$ev" --clicks "$cl" --output "$R/data" >/dev/null 2>&1 || { log "seed $seed: generate failed"; return; }
  taskset -c "$CPUS" "$D/bin/$sha/oracle.sh" "$R/data" "$R/oracle" > "$R/oracle.log" 2>&1 || { log "seed $seed: ORACLE CRASH"; note "SOAK: oracle crashed on seed $seed ($ev/$cl); data kept at $D/fail-$seed"; mv "$R" "$D/fail-$seed"; return; }
  rm -rf "$R/out"; mkdir -p "$R/out" "$R/scr"
  DATA_DIR="$R/data" OUTPUT_DIR="$R/out" SCRATCH_DIR="$R/scr" NCPU=8 MEM_GB=24 taskset -c "$CPUS" "$D/bin/$sha/fast" > "$R/fast.log" 2>&1 || rc=$?
  if [ $rc != 0 ]; then
    log "seed $seed: FAST EXIT $rc"; note "SOAK FAILURE: main $sha fast exited $rc on seed $seed (--events $ev --clicks $cl). Data+logs kept at $D/fail-$seed"; mv "$R" "$D/fail-$seed"; return
  fi
  if ! python3 "$ROOT/scripts/compare.py" "$R/out" "$R/oracle" > "$R/cmp.txt" 2>&1; then
    log "seed $seed: DIFFER"; note "SOAK FAILURE: main $sha DIFFERS from oracle on seed $seed (--events $ev --clicks $cl): $(grep -E '^(user|campaign)_daily' "$R/cmp.txt" | tr '\n' ' '). Data+outputs kept at $D/fail-$seed"; mv "$R" "$D/fail-$seed"; return
  fi
  local xc=""
  if [ $((seed % 10)) = 0 ]; then
    if taskset -c "$CPUS" timeout 900 python3 "$D/bin/$sha/oracle/xcheck.py" "$R/data" "$R/xc" > "$R/xc.log" 2>&1 && python3 "$ROOT/scripts/compare.py" "$R/xc" "$R/oracle" > "$R/xcmp.txt" 2>&1; then xc=" xcheck=MATCH"
    else xc=" xcheck=DIFFER"; note "SOAK: xcheck vs oracle DIFFERS (or failed) on seed $seed ($ev/$cl): oracle and xcheck disagree - investigate before trusting oracle. Kept at $D/fail-xc-$seed"; cp -r "$R" "$D/fail-xc-$seed"; fi
  fi
  log "seed $seed ok ($ev ev / $cl cl, main $sha)$xc"
  rm -rf "$R"
}

worker() {
  while true; do
    local sha; sha=$(cat "$D/current_sha")
    one_seed "$(next_seed)" "$sha"
  done
}

log "soak start pid $$ workers=$WORKERS cpus=$CPUS"
sha=$(git -C "$ROOT" rev-parse --short main); build_main "$sha" && echo "$sha" > "$D/current_sha"
for ((w=0; w<WORKERS; w++)); do worker & done
[ -f "$D/landing.last" ] || bench_landing "$sha"   # baseline for regression checks
while true; do  # watch main
  sleep 60
  new=$(git -C "$ROOT" rev-parse --short main)
  if [ "$new" != "$(cat "$D/current_sha")" ]; then
    log "main moved -> $new"
    if build_main "$new"; then echo "$new" > "$D/current_sha"; bench_landing "$new"; fi
  fi
done
