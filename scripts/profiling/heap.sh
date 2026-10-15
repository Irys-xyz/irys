#!/usr/bin/env bash
# jemalloc heap profiling for an irys node built with `--features jemalloc-prof`.
#
# 1. Start the node with sampling on. This samples one allocation per ~1 MiB
#    (lg_prof_sample:20) and writes a dump every 8 GiB allocated (lg_prof_interval:33):
#      mkdir -p ~/irys-heap
#      export _RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:20,lg_prof_interval:33,prof_prefix:$HOME/irys-heap/jeprof
#    Under pm2: `_RJEM_MALLOC_CONF=... pm2 restart "Irys Node" --update-env`.
# 2. heap.sh list                 newest dumps
# 3. heap.sh report BINARY HEAP [--base OLDER_HEAP] [--svg OUT.svg]
#    Live memory by allocation site. --base subtracts an older dump so only
#    growth is shown. Needs jeprof (`apt install libjemalloc-dev`).
set -euo pipefail
dir=$HOME/irys-heap
cmd=${1:-}; shift || true
case "$cmd" in
  list)
    ls -1t "$dir"/jeprof.*.heap 2>/dev/null | head -5 \
      || echo "no heap dumps in $dir (built with jemalloc-prof? _RJEM_MALLOC_CONF set?)"
    ;;
  report)
    bin=$1; heap=$2; shift 2
    base=(); svg=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --base) base=("--base=$2"); shift 2 ;;
        --svg) svg=$2; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
      esac
    done
    command -v jeprof > /dev/null || { echo "jeprof not found: apt install libjemalloc-dev" >&2; exit 1; }
    jeprof --text --lines "${base[@]}" "$bin" "$heap" 2>/dev/null | head -40
    if [ -n "$svg" ]; then
      jeprof --svg "${base[@]}" "$bin" "$heap" > "$svg" 2>/dev/null && echo "svg: $svg"
    fi
    ;;
  *) sed -n 2,13p "$0"; exit 2 ;;
esac
