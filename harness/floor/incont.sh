# incont.sh "ENV..." : run floor-alloc with scorer flags on cpuset 0-7 (under exclusive lock), print ms from MARK to FinishedAt
for kv in $1; do E="$E -e $kv"; done
CID=$(docker create --cpuset-cpus 0-7 --cpus 8 --memory 24g --memory-swap 24g --network none --read-only --user 1000:1000 $E floor-alloc:latest)
docker start -a $CID 2> log.txt >/dev/null
M=$(grep MARK log.txt | awk '{print $2}'); F=$(docker inspect -f '{{.State.FinishedAt}}' $CID); S=$(docker inspect -f '{{.State.StartedAt}}' $CID); docker rm $CID >/dev/null
python3 -c "
from datetime import datetime
def p(s):
    s=s.rstrip('Z'); a,b=s.split('.'); return datetime.fromisoformat(a+'+00:00').timestamp()+float('0.'+b[:9])
m=float('$M'); print('$1: start->MARK %.0f ms, MARK->exit %.0f ms'%((m-p('$S'))*1000,(p('$F')-m)*1000))"
