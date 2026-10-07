#!/bin/bash
# submodule-index-bench 5f0e9ad: --mid --group-commit on the root NVMe (Run 6 SSD).
set -u
cd /home/ubuntu/loadtest
BIN=./submodule-index-bench-5f0e9ad
SSD_ROOT=/home/ubuntu/loadtest
O=index-bench-runs/5f0e9ad-ssd
mkdir -p $O
log() { echo "# $* $(date -Is)" | tee -a $O/progress.log; }
src=$(findmnt -n -o SOURCE -T "$SSD_ROOT"); dev=$(basename "$src")
[ "$(cat /sys/block/$dev/queue/rotational)" = 0 ] || { log "$src is rotational, stopping"; exit 1; }
findmnt -T "$SSD_ROOT" > $O/disk.txt
dir="$SSD_ROOT/irys-index-ssd/index-bench"
if [ -d "$dir" ]; then ls "$dir" | grep -v -x -E "mdbx|rocks" && { log "unexpected files in $dir"; exit 1; }; rm -rf "$dir"; fi
log "start mid-group (load $(cut -d' ' -f1-3 /proc/loadavg))"
IRYS_INDEX_BENCH_LARGE=1 /usr/bin/time -v $BIN --dir "$dir" --mid --group-commit > $O/mid-group.txt 2> $O/mid-group.err &
tp=$!; sleep 1; bp=$(pgrep -P $tp | head -1)
top -b -H -d 5 -p "$bp" > $O/mid-group.top.log 2>&1 & topp=$!
wait $tp; rc=$?; kill $topp 2>/dev/null
log "end mid-group rc=$rc"
for k in "group_commit=1" "txs_per_commit=8 path_batch=2048"; do grep -q "$k" $O/mid-group.txt || log "MISSING '$k'"; done
[ "$(grep -c "txs_per_commit=8 path_batch=2048" $O/mid-group.txt)" -ge 2 ] || log "MISSING: both engine lines need txs_per_commit=8 path_batch=2048"
if [ -d "$dir" ]; then ls "$dir" | grep -v -x -E "mdbx|rocks" || rm -rf "$dir"; fi
log "done"
