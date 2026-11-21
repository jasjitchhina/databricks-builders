#!/bin/bash
# profsum.sh BIN SAMPLES [N]: resolve sampled offsets to (inlined) function + line, print the top N lines and top functions.
BIN=$1; S=$2; N=${3:-40}
sort $S | uniq -c | sort -rn > $S.cnt
TOT=$(awk '{s+=$1} END {print s}' $S.cnt)
awk '{print $2}' $S.cnt | head -3000 | addr2line -e $BIN -f -C -i -a 2>/dev/null > $S.sym
python3 - "$S.cnt" "$S.sym" "$TOT" "$N" <<'PY'
import sys, collections, re
cnt, sym, tot, n = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
counts = {}
for l in open(cnt):
    c, a = l.split(); counts[int(a,16)] = int(c)
# addr2line -a output: 0xADDR then pairs (func, file:line) for each inline level (innermost first)
lines = open(sym).read().split('\n')
cur=None; frames={}
i=0
while i < len(lines):
    l=lines[i]
    if l.startswith('0x'):
        cur=int(l,16); frames[cur]=[]; i+=1; continue
    if cur is not None and i+1 < len(lines):
        frames[cur].append((l, lines[i+1])); i+=2; continue
    i+=1
byline=collections.Counter(); byfunc=collections.Counter(); byouter=collections.Counter()
for a,fr in frames.items():
    c=counts.get(a,0)
    if not fr: continue
    f,loc = fr[0]
    byline[(f[:60], loc.split('/')[-1])]+=c
    byfunc[f[:80]]+=c
    # first frame whose file is in fast/src
    for f2,loc2 in fr:
        if 'fast/src' in loc2 and 'clean.rs' in loc2:
            byouter[loc2.split('/')[-1]]+=c; break
print(f"total samples {tot}")
print("== top inner functions"); [print(f"{c*100/tot:5.1f}% {f}") for f,c in byfunc.most_common(n//2)]
print("== top inner lines"); [print(f"{c*100/tot:5.1f}% {f} {l}") for (f,l),c in byline.most_common(n)]
print("== top clean.rs call sites"); [print(f"{c*100/tot:5.1f}% {l}") for l,c in byouter.most_common(n//2)]
PY
