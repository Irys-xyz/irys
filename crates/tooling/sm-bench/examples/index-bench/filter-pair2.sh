#!/bin/bash
# Step 2c: cold pair, partitioned first, random-read flush, 100k Gets. Binary 1519a32c5. Run 13 dirs, no re-copy, no rewrite.
set -u
cd /home/ubuntu/loadtest
BIN=./submodule-index-bench-1519a32
O=index-bench-runs/1519a32-filter-pair-cold2
mkdir -p $O
log() { echo "# $* $(date -Is)" | tee -a $O/progress.log; }
die() { log "STOP: $*"; exit 1; }
[ "$(sha256sum $BIN | cut -d' ' -f1)" = ba044167c46b58bd1d3c0f2c8bd1c0b356a7715340c07d0259d6825b6fd8f8ab ] || die "binary sha"
pgrep -f "submodule-index-bench-.* --dir|irys-sm-bench --dir" > /dev/null && die "another bench running"
for name in partition-filters baseline; do
  d="/mnt/sm-3/loadtest/rocks-filter-$name/index-bench/rocks"
  ssts=$(find "$d" -name '*.sst' -printf '%f %s\n'); echo "== $name"$'\n'"$ssts" >> $O/progress.log
  echo "$ssts" | grep -q "^000022.sst " || die "$name missing 000022.sst"
  echo "$ssts" | grep -q "^000009.sst " && die "$name still has 000009.sst"
done
log "both copies confirmed (000022.sst, no 000009.sst)"
mdb=/mnt/sm-3/loadtest/index-bench/mdbx/mdbx.dat
test -f "$mdb" || die "no mdbx.dat"
blocks=$(( $(stat -c %s "$mdb") / 65536 )); [ "$blocks" -gt 1 ] || die "blocks=$blocks"
log "random flush: 50000 x 64KiB direct reads over $blocks blocks"
t0=$(date +%s)
for _ in $(seq 1 50000); do
  off=$(( (RANDOM * 32768 + RANDOM) % blocks ))
  dd if="$mdb" of=/dev/null bs=65536 count=1 skip="$off" iflag=direct status=none
done
log "random flush done in $(( $(date +%s) - t0 )) s"
run() { local name=$1; shift
  log "start $name"
  local s0=$(date +%s)
  /usr/bin/time -v $BIN "$@" > $O/$name.txt 2> $O/$name.err &
  local tp=$!; sleep 1; local bp; bp=$(pgrep -P $tp | head -1); local maxrss=0
  while kill -0 $tp 2>/dev/null; do
    rss=$(awk '/VmRSS/{print $2}' /proc/$bp/status 2>/dev/null); rss=${rss:-0}
    [ "$rss" -gt "$maxrss" ] && maxrss=$rss
    if [ "$rss" -gt 20971520 ]; then log "$name RSS ${rss}kB > 20GB, stopping"; kill $bp; fi
    sleep 5
  done
  wait $tp; local rc=$?; log "end $name rc=$rc wall_s=$(( $(date +%s) - s0 )) max_rss_kb=$maxrss"; [ $rc -eq 0 ] || die "$name rc=$rc"; }
for name in partition-filters baseline; do
  run "cold2-$name" --dir "/mnt/sm-3/loadtest/rocks-filter-$name/index-bench" --profile filter-reuse --rocks "$name" --reads 100000
  t=$O/cold2-$name.txt
  grep -q "reads=100000" $t || log "CHECK FAIL $name: header lacks reads=100000"
  grep -q "rewrite_ms" $t && log "CHECK FAIL $name: rewrite_ms present"
  ao=$(grep -E "^ *sst when=after_open" $t)
  [ "$(echo "$ao" | grep -c .)" = 1 ] && echo "$ao" | grep -q "name=000022.sst level=6" && echo "$ao" | grep -q "start=0 end=41943039" || log "CHECK FAIL $name: after_open: $ao"
  grep -E "sst_cover" $t | grep -q "files=000022.sst@6$" || log "CHECK FAIL $name: sst_cover: $(grep sst_cover $t)"
  grep -E "rocks_bg shape=offset_get when=before" $t | grep -q "compactions=0 " || log "CHECK FAIL $name: compactions before shape"
  grep -E "read shape=offset_get" $t >> $O/progress.log
done
log "done"
