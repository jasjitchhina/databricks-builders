#!/bin/bash
# check_all.sh: diff prepdump vs oracle stage dumps on fixtures + 1M sets. Needs FX_DUMPS (oracle dumps per fixture) dir.
set -u
HERE=$(cd "$(dirname "$0")/.." && pwd)
FX=${FX_DUMPS:?}
W=$(mktemp -d)
fail=0
for f in $HERE/../fixtures/*/; do n=$(basename $f)
  $HERE/target/release/prepdump $f/input $W/$n 2>/dev/null
  python3 $HERE/tools/cmp_oracle.py $f/input $FX/$n/od $W/$n >$W/$n.log 2>&1 || { echo "FAIL fixture $n"; cat $W/$n.log; fail=1; }
done
for s in ${SETS:-dev s2-1m s3-1m s4-1m s5-1m s15-1m-feb29 s62-1m-feb29}; do
  $HERE/target/release/prepdump ~/data/$s $W/$s 2>/dev/null
  python3 $HERE/tools/cmp_oracle.py ~/data/$s ~/data/oracle/$s/dump $W/$s >$W/$s.log 2>&1 && echo "ok $s" || { echo "FAIL $s"; cat $W/$s.log; fail=1; }
done
rm -rf $W
[ $fail = 0 ] && echo ALL OK || echo SOME FAILED
exit $fail
