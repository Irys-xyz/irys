#!/bin/bash
# Step 2b: cold pair, both rewritten (sdd). Binary 1519a32c5. Source untouched.
set -u
cd /home/ubuntu/loadtest
BIN=./submodule-index-bench-1519a32
O=index-bench-runs/1519a32-filter-pair
mkdir -p $O
log() { echo "# $* $(date -Is)" | tee -a $O/progress.log; }
die() { log "STOP: $*"; exit 1; }
run() { local name=$1; shift
  log "start $name"
  /usr/bin/time -v $BIN "$@" > $O/$name.txt 2> $O/$name.err &
  local tp=$!; sleep 1; local bp; bp=$(pgrep -P $tp | head -1); local maxrss=0
  while kill -0 $tp 2>/dev/null; do
    rss=$(awk '/VmRSS/{print $2}' /proc/$bp/status 2>/dev/null); rss=${rss:-0}
    [ "$rss" -gt "$maxrss" ] && maxrss=$rss
    if [ "$rss" -gt 20971520 ]; then log "$name RSS ${rss}kB > 20GB, stopping"; kill $bp; fi
    sleep 2
  done
  wait $tp; local rc=$?; log "end $name rc=$rc max_rss_kb=$maxrss"; [ $rc -eq 0 ] || die "$name rc=$rc"; }
pgrep -f "submodule-index-bench-.* --dir|irys-sm-bench --dir" > /dev/null && die "another bench running"
src=/mnt/sm-3/loadtest/index-bench/rocks
test -f "$src/CURRENT" && test -f "$src/000009.sst" && test -f "$src/SCHEMA" || die "source incomplete"
printf "irys-submodule-rocks 1\n" | cmp -s - "$src/SCHEMA" || die "source SCHEMA not v1"
for name in baseline partition-filters; do
  dest="/mnt/sm-3/loadtest/rocks-filter-$name/index-bench"
  rm -rf "$dest"; mkdir -p "$dest"; cp -a "$src" "$dest/rocks"
done
log "fresh copies made"
declare -A FILE
for name in baseline partition-filters; do
  run "rewrite-$name" --dir "/mnt/sm-3/loadtest/rocks-filter-$name/index-bench" --profile filter-reuse --rocks "$name" --rewrite-filters
  t=$O/rewrite-$name.txt
  rw=$(grep -o "rewrite_ms=[0-9]*" $t | head -1 | cut -d= -f2); [ -n "$rw" ] && [ "$rw" -ge 1000 ] || die "$name rewrite_ms=$rw not seconds"
  want=full; [ $name = partition-filters ] && want=partitioned
  grep -q "filters=$want" $t || die "$name not filters=$want"
  ar=$(grep -E "^ *sst when=after_rewrite" $t)
  [ "$(echo "$ar" | grep -c .)" = 1 ] || die "$name after_rewrite is not exactly one file: $ar"
  echo "$ar" | grep -q "level=6" && echo "$ar" | grep -q "entries=41943040" && echo "$ar" | grep -q "start=0 " && echo "$ar" | grep -q "end=41943039" || die "$name after_rewrite wrong: $ar"
  echo "$ar" | grep -q "000009.sst" && die "$name 000009.sst still present"
  f=$(echo "$ar" | grep -o "name=[0-9]*\.sst" | cut -d= -f2); FILE[$name]=$f
  grep -E "sst_cover" $t | grep -q "files=$f@" || die "$name sst_cover does not name $f"
  log "rewrite-$name checks pass: $ar"
done
pgrep -f "submodule-index-bench-.* --dir|irys-sm-bench --dir" > /dev/null && die "bench still running before cache flush"
mdb=$(find /mnt/sm-3/loadtest/index-bench/mdbx -type f -printf '%s %p\n' | sort -n | tail -1 | cut -d' ' -f2-)
test -n "$mdb" || die "no mdbx file"
log "cache flush from $mdb"
dd if="$mdb" of=/dev/null bs=1M count=4096 iflag=direct 2>> $O/progress.log
log "cache flush done"
for name in baseline partition-filters; do
  run "cold-$name" --dir "/mnt/sm-3/loadtest/rocks-filter-$name/index-bench" --profile filter-reuse --rocks "$name"
  t=$O/cold-$name.txt; f=${FILE[$name]}
  grep -q "rewrite_ms" $t && die "$name cold run has rewrite_ms"
  ao=$(grep -E "^ *sst when=after_open" $t)
  [ "$(echo "$ao" | grep -c .)" = 1 ] && echo "$ao" | grep -q "name=$f " && echo "$ao" | grep -q "level=6" && echo "$ao" | grep -q "start=0 " && echo "$ao" | grep -q "end=41943039" || die "$name after_open differs from rewrite file $f: $ao"
  grep -E "sst_cover" $t | grep -q "files=$f@" || die "$name cold sst_cover does not name $f"
  grep -E "rocks_bg shape=offset_get when=before" $t | grep -q "compactions=0 " || die "$name compactions running before shape"
  log "cold-$name checks pass"
  grep -E "read shape=offset_get" $t | tee -a $O/progress.log > /dev/null
done
log "done"
