#!/bin/bash
# MDBX vs RocksDB read/write on the root NVMe (md2). Per-thread CPU sampled on every run; perf profile on --mid.
set -u
cd /home/ubuntu/loadtest
BIN=./submodule-index-bench-9aa12ee
BASE=/home/ubuntu/loadtest/irys-index-ssd
O=index-bench-runs/9aa12ee-ssd-cmp
mkdir -p $O $BASE
log() { echo "# $* $(date -Is)" | tee -a $O/progress.log; }
log "starting now (HDD benches are I/O-bound; load $(cut -d' ' -f1-3 /proc/loadavg))"
src=$(findmnt -n -o SOURCE -T "$BASE"); dev=$(basename "$src")
[ "$(cat /sys/block/$dev/queue/rotational)" = 0 ] || { log "$src is rotational, stopping"; exit 1; }
{ lsblk -d -o NAME,ROTA,SIZE,TRAN,MOUNTPOINTS; findmnt -T "$BASE"; df -hT "$BASE"; nproc; uptime; } > $O/disk.txt
clean() { local d=$1; [ -d "$d" ] || return 0
  ls "$d" | grep -v -x -E "mdbx|rocks" && { log "unexpected files in $d, stopping"; exit 1; }
  rm -rf "$d"; }
run() { local name=$1 dir=$2; shift 2; clean "$dir"
  log "start $name (load: $(cut -d' ' -f1-3 /proc/loadavg))"
  if [ "${LARGE:-0}" = 1 ]; then export IRYS_INDEX_BENCH_LARGE=1; else unset IRYS_INDEX_BENCH_LARGE; fi
  /usr/bin/time -v $BIN --dir "$dir" "$@" > $O/$name.txt 2> $O/$name.err &
  local tp=$!; sleep 1; local bp; bp=$(pgrep -P $tp | head -1)
  top -b -H -d 5 -p "$bp" > $O/$name.top.log 2>&1 & local topp=$!
  local pp=""
  if [ "${PERF:-0}" = 1 ]; then sudo perf record -F 49 -g -o $O/$name.perf.data -p "$bp" > $O/$name.perf.log 2>&1 & pp=$!; fi
  wait $tp; local rc=$?
  kill $topp 2>/dev/null
  if [ -n "$pp" ]; then sleep 2; sudo pkill -INT -f "perf record -F 49 -g -o $O/$name.perf.data" ; wait $pp 2>/dev/null; fi
  log "end $name rc=$rc"
  if [ -n "$pp" ]; then
    sudo perf report -i $O/$name.perf.data --stdio --no-children --sort comm --percent-limit 1 2>/dev/null | grep -v '^#' | grep -v '^$' > $O/$name.perf-threads.txt
    sudo perf report -i $O/$name.perf.data --stdio --no-children --sort comm,dso,sym --percent-limit 0.7 -g none 2>/dev/null | grep -v '^#' | grep -v '^$' | head -60 > $O/$name.perf-syms.txt
  fi
  [ $rc -eq 0 ] || exit 1
  grep -q "disk=md" $O/$name.txt || log "note: $name disk= is not md*"
  clean "$dir"; }
run filled-16384 "$BASE/filled-16384/index-bench" --chunks 16384
LARGE=1 PERF=1 run mid "$BASE/mid/index-bench" --mid
# equal batching at 1M; may hit the MDBX map size in the filled profile, so it runs last
( LARGE=1 run filled-1m "$BASE/filled-1m/index-bench" --chunks 1000000 ) || log "filled-1m failed (see filled-1m.err)"
log "SSD comparison done"
