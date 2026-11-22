#!/usr/bin/env bash
# cache_tests.sh TARGET WORKDIR — task 62 replay-cache negative tests (verify.sh section [4]).
# TARGET as in run_target.sh (image tag or native executable). Every run must exit 0 with row-exact output;
# runs marked must-miss must also log "cache: miss" (FAST_VERBOSE=1 on every run here).
# Reference answers: the oracle for s2-1m / s3-1m / dev, and for mutated copies of dev a run of the same
# target with FAST_NO_CACHE=1 on a fresh scratch (the cache must never change what a full run computes).
# A target without the cache (no "cache:" log lines, no .fastcache after a run) -> "skip" (exit 0), unless
# CACHE_REQUIRE=1. Prints "ok"/"FAIL" lines; last line "CACHE PASS n" or "CACHE FAIL n: names"; exit 1 on fail.
set -uo pipefail
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(dirname "$H")
DATA_ROOT=${DATA_ROOT:-$HOME/data}
export PYTHONPATH=$HOME/pylib${PYTHONPATH:+:$PYTHONPATH}
export TARGET=$1; W=$2; mkdir -p "$W"
RUN="$H/run_target.sh"
A=$DATA_ROOT/s2-1m; AANS=$DATA_ROOT/oracle/s2-1m/out
B=$DATA_ROOT/s3-1m; BANS=$DATA_ROOT/oracle/s3-1m/out
DEV=$DATA_ROOT/dev; DANS=$ROOT/dev-answer
NATIVE=0; [ -f "$TARGET" ] && [ -x "$TARGET" ] && NATIVE=1
FAILS=(); NOK=0; N=0
helper() {  # helper DIR 'sh cmds on /w' : root shell on a dir (cached files belong to uid 1000 under docker)
  local d=$1; shift
  if [ "$NATIVE" = 1 ]; then (cd "$d" && sh -c "$*"); else
    docker run --rm --network none --user 0:0 -v "$d":/w -w /w --entrypoint sh "${HELPER_IMAGE:-python:3.12-slim-bookworm}" -c "$*; chmod -R a+rwX /w 2>/dev/null; true"; fi
}
nuke() { for d in "$@"; do [ -e "$d" ] || continue; rm -rf "$d" 2>/dev/null || { helper "$(dirname "$d")" "rm -rf '/w/$(basename "$d")'"; }; done; }
entries() { ls -1 "$1/.fastcache" 2>/dev/null | grep -v '\.tmp\.' | sort | tr '\n' ' '; }

# go NAME DATA SCRATCH EXPECT(hit|miss|any) ANSWER [extra TARGET_ENV] [RT_SCRATCH_MODE]
# runs once into a fresh output dir pre-seeded with a stale junk part file (the cache must clear it like a full run)
go() {
  local name=$1 data=$2 scr=$3 want=$4 ans=$5 xenv=${6:-} mode=${7:-rw} out=$W/o-$1 log=$W/log-$1 got
  N=$((N+1)); nuke "$out"; mkdir -p "$out/user_daily" "$out/campaign_daily"
  cp "$(ls "$AANS"/user_daily/*.parquet | head -1)" "$out/user_daily/part-99999-stale.parquet"   # valid parquet, foreign rows
  cp "$(ls "$AANS"/campaign_daily/*.parquet | head -1)" "$out/campaign_daily/part-99999-stale.parquet"
  local rs=(); [ -n "$scr" ] && rs=(RT_SCRATCH="$scr")
  if ! env "${rs[@]}" RT_SCRATCH_MODE="$mode" TARGET_ENV="FAST_VERBOSE=1 $xenv" "$RUN" "$data" "$out" > "$log" 2>&1; then
    echo "FAIL cache/$name: target exited nonzero"; tail -5 "$log" | sed 's/^/     /'; FAILS+=("$name"); return 1
  fi
  got=$("$H/cacheline" "$log")
  if ! python3 "$ROOT/scripts/compare.py" "$out" "$ans" > "$W/cmp-$name" 2>&1; then
    echo "FAIL cache/$name: output DIFFERS from reference ($got)"; sed 's/^/     /' "$W/cmp-$name" | head -12; FAILS+=("$name"); return 1
  fi
  case "$want:$got" in
    hit:"cache: hit"*|miss:"cache: miss"*|any:*) ;;
    *) echo "FAIL cache/$name: expected $want, log says '${got:-<no cache line>}'"; FAILS+=("$name"); return 1 ;;
  esac
  NOK=$((NOK+1)); echo "ok   cache/$name ($want: ${got:-<no cache line>}; $(grep -m1 '^user_daily' "$W/cmp-$name" | cut -d';' -f1))"
}
ref() {  # ref NAME DATA -> $W/ref-NAME: full run, cache disabled, fresh scratch (reference for mutated data)
  nuke "$W/ref-$1"
  TARGET_ENV="FAST_NO_CACHE=1" "$RUN" "$2" "$W/ref-$1" > "$W/log-ref-$1" 2>&1 || { echo "FAIL cache/ref-$1: reference run failed"; FAILS+=("ref-$1"); }
}

# ---- T1 basic miss -> hit, stale output cleared, byte-identical replay
S=$W/s1; nuke "$S"
go t1-miss "$A" "$S" any "$AANS"
[ ${#FAILS[@]} -eq 0 ] || { echo "CACHE FAIL ${#FAILS[@]}: ${FAILS[*]} (first run failed; rest not run)"; exit 1; }
if [ -z "$(entries "$S")" ] && [ -z "$("$H/cacheline" "$W/log-t1-miss")" ]; then
  if [ "${CACHE_REQUIRE:-0}" = 1 ]; then echo "FAIL cache: target has no replay cache (CACHE_REQUIRE=1)"; echo "CACHE FAIL 1: no-cache"; exit 1; fi
  echo "skip cache tests: target has no replay cache (no 'cache:' log line, no \$SCRATCH/.fastcache)"; echo "CACHE PASS 0 (no cache)"; exit 0
fi
case "$("$H/cacheline" "$W/log-t1-miss")" in "cache: miss"*) ;; *) echo "FAIL cache/t1-miss: fresh scratch but log says '$("$H/cacheline" "$W/log-t1-miss")'"; FAILS+=(t1-miss-log);; esac
[ "$(entries "$S" | wc -w)" = 1 ] && [ -f "$S/.fastcache/$(entries "$S" | tr -d ' ')/OK" ] || { echo "FAIL cache/t1-layout: expected exactly one <fp>/OK entry, got: $(ls -la "$S/.fastcache" 2>&1 | tr '\n' ' ')"; FAILS+=(t1-layout); }
go t1-hit "$A" "$S" hit "$AANS"
go t1-hit2 "$A" "$S" hit "$AANS"
diff -rq "$W/o-t1-miss" "$W/o-t1-hit" >/dev/null 2>&1 && echo "ok   cache/t1-bytes (hit output byte-identical to the full run)" || { echo "FAIL cache/t1-bytes: hit output not byte-identical"; diff -rq "$W/o-t1-miss" "$W/o-t1-hit" | head -5; FAILS+=(t1-bytes); }

# ---- T2 dataset A's scratch, then dataset B (the scorer mounts every dataset at the same /data)
go t2-A-cached "$A" "$S" hit "$AANS"
go t2-B-after-A "$B" "$S" miss "$BANS"
[ "$(entries "$S" | wc -w)" = 1 ] || { echo "FAIL cache/t2-growth: $(entries "$S" | wc -w) entries after store (old fp not removed)"; FAILS+=(t2-growth); }
go t2-A-after-B "$A" "$S" miss "$AANS"
go t2-B-again "$B" "$S" miss "$BANS"

# ---- T3 same paths, content changed (dev copy; reference = full run without cache)
D=$W/data-dev; nuke "$D"; cp -a "$DEV" "$D"
S=$W/s3; nuke "$S"
go t3-populate "$D" "$S" miss "$DANS"
BIG=$(ls -S "$D"/events/*.csv | head -1); SZ=$(stat -c %s "$BIG")
mutate() {  # mutate FILE OFFSET: replace the first digit at/after OFFSET by another digit; keep size + mtime
  python3 - "$1" "$2" <<'EOF'
import os, sys
p, off = sys.argv[1], int(sys.argv[2]); st = os.stat(p)
with open(p, 'r+b') as f:
    buf = f.read(); j = next(k for k in list(range(off, len(buf))) + list(range(0, off)) if 48 <= buf[k] <= 57)
    f.seek(j); f.write(bytes([48 + (buf[j] - 48 + 1) % 10]))   # raises (test skipped) if the file has no digit
os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))
EOF
}
mutate "$BIG" $((SZ / 2)); ref t3-mid "$D"   # middle of the biggest file: outside the head/tail hash, same size+mtime
cmp -s <(cd "$W/ref-t3-mid" && cat user_daily/*.parquet campaign_daily/*.parquet) <(cd "$W/o-t3-populate" && cat user_daily/*.parquet campaign_daily/*.parquet) && echo "warn cache/t3-mid: mutation did not change the output (test weaker)"
go t3-mid "$D" "$S" miss "$W/ref-t3-mid"
mutate "$BIG" 100; ref t3-head "$D"; go t3-head "$D" "$S" miss "$W/ref-t3-head"
mutate "$BIG" $((SZ - 300)); ref t3-tail "$D"; go t3-tail "$D" "$S" miss "$W/ref-t3-tail"
SM=$(ls -S $(find "$D"/events -name "*.csv" -size +8k) | tail -1)   # smallest non-header-only file
if mutate "$SM" $(( $(stat -c %s "$SM") / 2 )); then ref t3-small "$D"; go t3-small "$D" "$S" miss "$W/ref-t3-small"; else echo "FAIL cache/t3-small: could not mutate $SM"; FAILS+=(t3-small-setup); fi
cp -p "$SM" "$D/events/part-77777.csv"; ref t3-addfile "$D"; go t3-addfile "$D" "$S" miss "$W/ref-t3-addfile"
rm -f "$D/events/part-77777.csv" "$SM"; ref t3-rmfile "$D"; go t3-rmfile "$D" "$S" miss "$W/ref-t3-rmfile"
# swap two same-size files' contents? -> rename-replace a file with a modified copy, mtime restored (new inode)
F=$(ls "$D"/clicks/*.csv | head -1); cp -p "$F" "$F.new"; mutate "$F.new" $(( $(stat -c %s "$F") / 2 )); touch -r "$F" "$F.new"; mv "$F.new" "$F"
ref t3-replace "$D"; go t3-replace "$D" "$S" miss "$W/ref-t3-replace"
LAST=$W/ref-t3-replace
MOFF=$(grep -bo '[0-9][0-9]T' "$D/manifest.json" 2>/dev/null | tail -1 | cut -d: -f1)   # window_end day, 2nd digit
if [ -n "$MOFF" ] && mutate "$D/manifest.json" $((MOFF + 1)) 2>/dev/null; then ref t3-manifest "$D"; go t3-manifest "$D" "$S" miss "$W/ref-t3-manifest"; LAST=$W/ref-t3-manifest
else echo "skip cache/t3-manifest: no digit in manifest.json"; fi
go t3-unchanged-hit "$D" "$S" hit "$LAST"   # and the unchanged (mutated) dataset now hits

# ---- T4 damaged cache entries (each run re-stores a good entry for the next subtest)
S=$W/s4; nuke "$S"; go t4-populate "$A" "$S" miss "$AANS"
fp() { entries "$S" | tr -d ' '; }
helper "$S" "f=\$(find .fastcache/$(fp) -type f ! -name OK | sort | head -1); truncate -s -1 \"\$f\""; go t4-truncated "$A" "$S" miss "$AANS"
helper "$S" "f=\$(find .fastcache/$(fp) -type f ! -name OK | sort | tail -1); truncate -s +7 \"\$f\""; go t4-grown "$A" "$S" miss "$AANS"
helper "$S" "f=\$(find .fastcache/$(fp) -type f ! -name OK | sort | head -1); rm -f \"\$f\""; go t4-file-missing "$A" "$S" miss "$AANS"
helper "$S" "rm -f .fastcache/$(fp)/OK"; go t4-ok-missing "$A" "$S" miss "$AANS"
helper "$S" ": > .fastcache/$(fp)/OK"; go t4-ok-empty "$A" "$S" miss "$AANS"
helper "$S" "head -c 300 /dev/urandom > .fastcache/$(fp)/OK"; go t4-ok-garbage "$A" "$S" miss "$AANS"
helper "$S" "d=.fastcache/$(fp); mkdir -p \$d/user_daily \$d/campaign_daily; cp \$(find \$d -name '*.parquet' | head -1) \$d/user_daily/part-88888.parquet; cp \$(find \$d -name '*.parquet' | head -1) \$d/campaign_daily/part-88888.parquet; cp \$(find \$d -name '*.parquet' | head -1) \$d/part-88888.parquet"
go t4-unlisted-extra "$A" "$S" any "$AANS"
helper "$S" "mkdir -p .fastcache/$(fp).tmp.1 .fastcache/$(fp).tmp.999999; cp -r .fastcache/$(fp)/. .fastcache/$(fp).tmp.1/; chmod -R a+rwX .fastcache"
go t4-stale-tmp "$A" "$S" any "$AANS"
helper "$S" "rm -rf .fastcache; : > .fastcache; chmod a+rw .fastcache"; go t4-fastcache-is-file "$A" "$S" miss "$AANS"
helper "$S" "rm -rf .fastcache; mkdir .fastcache; mkdir .fastcache/$(printf 'ff%.0s' $(seq 8)); chmod -R a+rwX .fastcache"; go t4-bogus-fp-dir "$A" "$S" miss "$AANS"

# ---- T5 read-only scratch (empty, then holding a valid entry)
S=$W/s5; nuke "$S"; mkdir -p "$S"
go t5-ro-empty "$A" "$S" miss "$AANS" "" ro
go t5-populate "$A" "$S" miss "$AANS"
go t5-ro-populated "$A" "$S" any "$AANS" "" ro
go t5-ro-other-dataset "$B" "$S" miss "$BANS" "" ro

# ---- T6 scratch missing / SCRATCH_DIR unset
go t6-missing "$A" "" miss "$AANS" "" missing
go t6-unset "$A" "" miss "$AANS" "" unset

# ---- T7 full scratch (tiny tmpfs volume that persists across runs; docker only)
if [ "$NATIVE" = 0 ]; then
  for sz in 16k 300k 900k; do
    V=cachetest-$$-$sz
    docker volume create --driver local --opt type=tmpfs --opt device=tmpfs --opt o="size=$sz,uid=1000,gid=1000,mode=0755" "$V" >/dev/null
    go "t7-full-$sz-1" "$A" "" miss "$AANS" "" "vol:$V"
    go "t7-full-$sz-2" "$A" "" any "$AANS" "" "vol:$V"
    go "t7-full-$sz-B" "$B" "" miss "$BANS" "" "vol:$V"
    docker volume rm "$V" >/dev/null
  done
else echo "skip cache/t7-full: native target (needs a tmpfs volume)"; fi

# ---- T8 two fingerprints present
S1=$W/s8a; S2=$W/s8b; nuke "$S1" "$S2"
go t8-popA "$A" "$S1" miss "$AANS"; go t8-popB "$B" "$S2" miss "$BANS"
cp -r "$S1/.fastcache/." "$S2/.fastcache/" && chmod -R a+rwX "$S2" && [ "$(entries "$S2" | wc -w)" = 2 ] || { echo "FAIL cache/t8-setup: could not make 2 entries"; FAILS+=(t8-setup); }
go t8-A-with-2 "$A" "$S2" hit "$AANS"
cp -r "$S1/.fastcache/." "$S2/.fastcache/" 2>/dev/null; chmod -R a+rwX "$S2" 2>/dev/null
go t8-B-with-2 "$B" "$S2" hit "$BANS"
[ "$(entries "$S2" | wc -w)" -le 1 ] || echo "warn cache/t8: $(entries "$S2" | wc -w) entries left after a hit (stores prune, hits need not)"

# ---- T9 FAST_NO_CACHE=1 disables read and write
S=$W/s9; nuke "$S"; go t9-populate "$A" "$S" miss "$AANS"
BEFORE=$(find "$S" -printf '%P %s %T@\n' | sort | md5sum)
go t9-disabled-A "$A" "$S" miss "$AANS" FAST_NO_CACHE=1
case "$("$H/cacheline" "$W/log-t9-disabled-A")" in "cache: hit"*) echo "FAIL cache/t9: FAST_NO_CACHE=1 run logged a hit"; FAILS+=(t9-log);; esac
go t9-disabled-B "$B" "$S" miss "$BANS" FAST_NO_CACHE=1
[ "$(find "$S" -printf '%P %s %T@\n' | sort | md5sum)" = "$BEFORE" ] && echo "ok   cache/t9-nowrite (scratch untouched)" || { echo "FAIL cache/t9-nowrite: FAST_NO_CACHE=1 modified scratch"; FAILS+=(t9-nowrite); }

nuke "$W"/s* "$W"/o-* "$W"/ref-* "$D"
if [ ${#FAILS[@]} -eq 0 ]; then echo "CACHE PASS $NOK/$N runs"; exit 0; fi
echo "CACHE FAIL ${#FAILS[@]}: ${FAILS[*]}"; exit 1
