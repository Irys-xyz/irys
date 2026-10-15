#!/usr/bin/env bash
# Log a node's memory every INTERVAL seconds; the first time anonymous memory
# passes THRESHOLD_MB, capture threads + faults (+ a jemalloc heap dump when the
# node runs with heap profiling). Run under nohup/setsid on the node host.
#
#   memwatch.sh [--pm2 NAME] [--interval 20] [--threshold-mb 20480] [--log FILE]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
pm2name="Irys Node"; interval=20; threshold=20480; log=$HOME/irys-memwatch.log
while [ $# -gt 0 ]; do
  case "$1" in
    --pm2) pm2name=$2; shift 2 ;;
    --interval) interval=$2; shift 2 ;;
    --threshold-mb) threshold=$2; shift 2 ;;
    --log) log=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
fired_for=""
while true; do
  pid=$(pm2 pid "$pm2name" 2>/dev/null || true)
  if [ -n "$pid" ] && [ "$pid" != 0 ] && [ -r "/proc/$pid/status" ]; then
    anon=$(awk '/RssAnon/{print int($2/1024)}' "/proc/$pid/status")
    file=$(awk '/RssFile/{print int($2/1024)}' "/proc/$pid/status")
    echo "$(date -u +%FT%TZ) pid=$pid anon_mb=$anon file_mb=$file up=$(ps -o etime= -p "$pid" | tr -d ' ')" >> "$log"
    if [ "$anon" -gt "$threshold" ] && [ "$fired_for" != "$pid" ]; then
      fired_for=$pid
      "$here/profile-node.sh" --pid "$pid" --seconds 5 threads faults >> "$log" 2>&1 || true
      "$here/heap.sh" list >> "$log" 2>&1 || true
    fi
  fi
  sleep "$interval"
done
