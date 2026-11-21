#!/bin/bash
# oracle/phase_sweep.sh BIN DATASET REPS "LABEL:CPUSET:NCPU[:BIN]" ...
# In-process FAST_TIMING phase medians per config (NOT bench.sh; for relative comparisons only).
BIN=$1; DS=$2; REPS=$3; shift 3
O=$(mktemp -d)
for i in $(seq 1 "$REPS"); do
  for cfg in "$@"; do
    IFS=: read -r lab cs n bin <<<"$cfg"; bin=${bin:-$BIN}
    rm -rf "$O/out"  # fresh output dir every run: overwriting old parquet files adds ~150ms to engine
    FAST_VERBOSE=1 FAST_TIMING=1 NCPU=$n taskset -c "$cs" "$bin" "$HOME/data/$DS" "$O/out" 2>&1 | awk -v lab="$lab" '
      /\[clean\] map\+chunk/ {mc=$(NF-1)+0}
      /\[clean\] parse / {gsub(/[()]/,""); p=$3; pt=$(NF-1)}
      /\[clean\] dedup / {d=$3}
      /\[clean\] parts / {pa=$3}
      /cpu: events/ {ce=$4; cc=$7}
      /\[dedup\] cpu/ {di=$4; dr=$7; dp=$10}
      /^load / {ld=$2; en=$4}
      /^main total/ {mt=$3}
      END {printf "%s parse=%s dedup=%s parts=%s engine=%s main=%s cpu_ev=%s cpu_cl=%s dd_ins=%s dd_rew=%s dd_ps=%s\n", lab,p,d,pa,en,mt,ce,cc,di,dr,dp}'
  done
done | python3 -c "
import sys,re,statistics as st
from collections import defaultdict
D=defaultdict(lambda: defaultdict(list)); order=[]
for l in sys.stdin:
    f=l.split(); lab=f[0]
    if lab not in order: order.append(lab)
    for kv in f[1:]:
        k,v=kv.split('=')
        v=re.sub(r'[^0-9.]','',v)
        if v: D[lab][k].append(float(v)*(1000 if k=='main' and float(v)<50 else 1))
keys=['parse','dedup','parts','engine','main','cpu_ev','cpu_cl','dd_ins','dd_rew','dd_ps']
print('config  '+' '.join(f'{k:>8}' for k in keys))
for lab in order:
    print(f'{lab:7} '+' '.join(f'{st.median(D[lab][k]):8.0f}' if D[lab][k] else '       -' for k in keys))
"
rm -rf "$O"
