#!/bin/bash
# prof.sh BIN DATA [runs]: dev-profiling run on the bench cores under the EXCLUSIVE bench lock (never overlaps bench.sh). Internal timers only; not an official timing.
BIN=$1; D=$2; R=${3:-3}
flock /tmp/bench.lock bash -c "for i in \$(seq $R); do FAST_VERBOSE=1 NCPU=8 taskset -c 0-7 $BIN $D 2>&1 | grep -vE '^events ' | sed 's/\[clean\] *//; s/(total *[0-9.]* ms)//' | tr -s ' ' | tr '\n' '|'; echo; done"
