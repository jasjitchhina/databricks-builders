#!/bin/bash
# oraclecheck.sh SET...  : diff outputs + sessions.csv dump vs ~/data/oracle/<SET>
here=$(cd "$(dirname "$0")" && pwd)
BIN=${BIN:-$here/target/release/fast}; PQ=${PQ:-$here/../fast-tools/target/release/pq2csv}
UC=user_id,day,sessions,events,revenue_cents,top_category
CC=campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions
rc=0
for s in "$@"; do
  tmp=$(mktemp -d); O=~/data/oracle/$s
  DUMP_DIR=$tmp/dump FAST_TIMING=1 "$BIN" ~/data/$s "$tmp/out" || { echo "$s CRASH"; rc=1; continue; }
  for t in user_daily campaign_daily; do
    c=$UC; [ $t = campaign_daily ] && c=$CC
    "$PQ" "$O/out/$t" $c | LC_ALL=C sort > $tmp/exp; "$PQ" "$tmp/out/$t" $c | LC_ALL=C sort > $tmp/act
    if cmp -s $tmp/exp $tmp/act; then echo "$s $t MATCH $(wc -l <$tmp/exp)"; else rc=1; echo "$s $t DIFFER"; diff $tmp/exp $tmp/act | head -6; fi
  done
  if [ -f $O/dump/sessions.csv ]; then
    if cmp -s <(LC_ALL=C sort $O/dump/sessions.csv) <(LC_ALL=C sort $tmp/dump/sessions.csv); then echo "$s sessions MATCH"; else rc=1; echo "$s sessions DIFFER"; diff <(LC_ALL=C sort $O/dump/sessions.csv) <(LC_ALL=C sort $tmp/dump/sessions.csv) | head -6; fi
  fi
  rm -rf $tmp
done
exit $rc
