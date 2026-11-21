#!/bin/bash
# envtest.sh IMAGE: run docker env/robustness matrix on s2-1m, compare each vs oracle
S=${ENVTEST_DIR:-$(mktemp -d)}
IMG=$1; D=$HOME/data/s2-1m; O=$HOME/data/oracle/s2-1m/out
W=$S/envw
run() { # name, setup-fn, docker args...
  local name=$1; shift; local setup=$1; shift
  docker run --rm --network none --user 0:0 -v $W:/w --entrypoint /bin/true $IMG >/dev/null 2>&1
  rm -rf $W 2>/dev/null; docker run --rm -v $S:/s alpine:3 rm -rf /s/envw >/dev/null 2>&1
  mkdir -p $W/out $W/scratch; chmod 777 $W/out $W/scratch
  $setup
  docker run --rm --cpuset-cpus 8-15 --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
    --cap-drop ALL --security-opt no-new-privileges --user 1000:1000 \
    -v $D:/data:ro -v $W/out:/output -v $W/scratch:/scratch "$@" $IMG > $W/stdout 2> $W/stderr
  rc=$?
  res=$(PYTHONPATH=$HOME/pylib python3 /home/jessechhina/wt/h4/scripts/compare.py $W/out $O 2>&1 | tail -1)
  echo "$name: rc=$rc $res stdout=$(wc -c < $W/stdout)B stderr=$(tail -c 150 $W/stderr | tr '\n' ' ')"
}
none() { :; }
stale() { mkdir -p $W/out/user_daily $W/out/campaign_daily; cp $HOME/data/oracle/s2-1m/out/user_daily/*.parquet $W/out/user_daily/part-99.parquet; cp $HOME/data/oracle/s2-1m/out/campaign_daily/*.parquet $W/out/campaign_daily/part-7.parquet; echo x > $W/out/user_daily/_SUCCESS; chmod -R 777 $W/out; }
emptydirs() { mkdir -p $W/out/user_daily $W/out/campaign_daily; chmod -R 777 $W/out; }
roscratch() { chmod 555 $W/scratch; }
E="-e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch -e MEM_GB=24"
run baseline none $E -e NCPU=8 --cpus 8 --memory 24g
run ncpu_unset none $E --cpus 8
run ncpu_garbage none $E -e NCPU=abc
run ncpu_zero none $E -e NCPU=0
run ncpu_spaces none $E -e "NCPU= 8 "
run ncpu_huge none $E -e NCPU=4096 --pids-limit 4096
run ncpu_neg none $E -e NCPU=-3
run quota2_unset none $E --cpus 2
run quota1.5_ncpu8 none $E -e NCPU=8 --cpus 1.5
run stale_output stale $E -e NCPU=8
run outdirs_exist emptydirs $E -e NCPU=8
run empty_env none -e DATA_DIR= -e OUTPUT_DIR= -e NCPU= -e HOME= -e SCRATCH_DIR=
run no_env none
run ro_scratch roscratch $E -e NCPU=8
run mem3g_pids256 none $E -e NCPU=8 --memory 3g --memory-swap 3g --pids-limit 256
