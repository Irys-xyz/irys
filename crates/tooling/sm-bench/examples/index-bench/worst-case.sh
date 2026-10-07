#!/bin/bash
# Index bench worst-case partition on sdd, after the sdc group-commit ladders (r9-12) finish.
set -u
cd /home/ubuntu/loadtest
BIN=./submodule-index-bench-0c38395
D=/mnt/sm-3/loadtest/index-bench
O=index-bench-runs/0c38395-worst
mkdir -p $O
log() { echo "# $* $(date -Is)" | tee -a $O/progress.log; }
log "waiting for r9-12 to finish"
until [ -e r9-12.done ]; do sleep 60; done
log "r9-12 done: $(cat r9-12.done)"
pgrep -f "irys-sm-bench --dir|submodule-index-bench-.* --dir" > /dev/null && { log "another bench still running, stopping"; exit 1; }
[ "$(sha256sum $BIN | cut -d' ' -f1)" = bd43206467954a616f137f9dc2a9e9c9f52617cbd58599ab2bfc01eef2747548 ] || { log "binary sha mismatch"; exit 1; }
src=$(findmnt -n -o SOURCE -T /mnt/sm-3/loadtest); [ "$src" = /dev/sdd ] || { log "loadtest not on sdd ($src)"; exit 1; }
[ "$(cat /sys/block/sdd/queue/rotational)" = 1 ] || { log "sdd not rotational?"; exit 1; }
avail=$(df -B1G --output=avail /mnt/sm-3 | tail -1); [ "$avail" -ge 800 ] || { log "only ${avail}G free on sdd"; exit 1; }
{ lsblk -d -o NAME,ROTA,SIZE,MODEL,TRAN /dev/sdd; findmnt -T /mnt/sm-3/loadtest; df -hT /mnt/sm-3/loadtest; free -m; swapon --show; } > $O/disk.txt
if [ -d "$D" ]; then ls "$D" | grep -v -x -E "mdbx|rocks" && { log "unexpected files in $D"; exit 1; }; rm -rf "$D"; fi
swap0=$(awk '/SwapTotal/{t=$2} /SwapFree/{f=$2} END{print t-f}' /proc/meminfo)
iostat -dmx sdd 30 > $O/iostat.log 2>&1 & ip=$!
log "start worst-case (swap used ${swap0} kB)"
IRYS_INDEX_BENCH_LARGE=1 /usr/bin/time -v $BIN --dir "$D" --worst-case --chunks 75534400 > $O/worst.txt 2> $O/worst.err &
tp=$!; sleep 2; bp=$(pgrep -P $tp | head -1)
headchk=0
while kill -0 $tp 2>/dev/null; do
  swap=$(awk '/SwapTotal/{t=$2} /SwapFree/{f=$2} END{print t-f}' /proc/meminfo)
  memav=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
  rss=$(awk '/VmRSS/{print $2}' /proc/$bp/status 2>/dev/null); rss=${rss:-0}
  echo "$(date -Is) rss_kb=$rss memavail_kb=$memav swap_used_kb=$swap dir=$(du -s --block-size=1M $D 2>/dev/null | cut -f1)MB" >> $O/mem.log
  if [ $((swap - swap0)) -gt 1048576 ] || [ "$memav" -lt 4194304 ]; then
    log "memory guard: swap_used_kb=$swap (start $swap0) memavail_kb=$memav rss_kb=$rss — stopping (instructions: stop if swapping)"
    kill $bp; sleep 10; kill -9 $bp 2>/dev/null
  fi
  if [ $headchk = 0 ] && grep -q "full_partition_raw_data_proof_bytes" $O/worst.txt 2>/dev/null; then
    headchk=1
    for k in "mode=worst-case" "data_proof_bytes=2464" "chunks=75534400" "full_partition_raw_data_proof_bytes=186116761600"; do grep -q "$k" $O/worst.txt || log "header MISSING '$k'"; done
    log "header checked"
  fi
  sleep 30
done
wait $tp; rc=$?
kill $ip 2>/dev/null
log "end worst-case rc=$rc"
du -s --block-size=1M $D/* >> $O/progress.log 2>/dev/null
log "worst-case directory left in place for inspection (remove with rm -rf $D)"
