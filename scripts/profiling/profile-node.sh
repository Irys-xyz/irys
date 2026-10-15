#!/usr/bin/env bash
# Profile a running irys node with perf. Run on the node host.
#
#   profile-node.sh [--pid N | --pm2 NAME] [--seconds S] [--out DIR] MODE...
#
# MODE:
#   cpu      on-CPU samples (perf -F 199 -g): where the time goes
#   faults   page-fault samples: which code is touching fresh memory, i.e.
#            where anonymous memory grows (cheap stand-in for a heap profile)
#   threads  per-thread CPU (top -H) and the process memory breakdown
#   all      threads, cpu, faults
#
# Output: DIR/<mode>.data plus DIR/<mode>.txt, a summary that attributes each
# sample to its first irys frame (summarize_perf.py). Needs perf and sudo.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
pid=""; pm2name="Irys Node"; secs=10; out=""
modes=()
while [ $# -gt 0 ]; do
  case "$1" in
    --pid) pid=$2; shift 2 ;;
    --pm2) pm2name=$2; shift 2 ;;
    --seconds) secs=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    cpu|faults|threads|all) modes+=("$1"); shift ;;
    -h|--help) sed -n 2,15p "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ ${#modes[@]} -gt 0 ] || modes=(all)
if [ -z "$pid" ]; then pid=$(pm2 pid "$pm2name" 2>/dev/null || true); fi
if [ -z "$pid" ] || [ "$pid" = 0 ] || [ ! -d "/proc/$pid" ]; then
  echo "no running process (pass --pid or --pm2)" >&2; exit 1
fi
out=${out:-$HOME/irys-profile/$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$out"
[ "${modes[0]}" = all ] && modes=(threads cpu faults)

for mode in "${modes[@]}"; do
  case "$mode" in
    threads)
      {
        echo "# $(date -u -Is) pid=$pid"
        grep -E "VmRSS|RssAnon|RssFile|VmHWM|Threads" "/proc/$pid/status"
        echo
        top -H -b -n1 -p "$pid" | sed -n '7,40p'
      } > "$out/threads.txt"
      ;;
    cpu)
      sudo perf record -F 199 -g -p "$pid" -o "$out/cpu.data" -- sleep "$secs" > /dev/null 2>&1
      sudo perf script -i "$out/cpu.data" 2>/dev/null | python3 "$here/summarize_perf.py" > "$out/cpu.txt"
      ;;
    faults)
      sudo perf record -e page-faults -g -p "$pid" -o "$out/faults.data" -- sleep "$secs" > /dev/null 2>&1
      sudo perf script -i "$out/faults.data" 2>/dev/null | python3 "$here/summarize_perf.py" > "$out/faults.txt"
      ;;
  esac
  echo "$mode -> $out/$mode.txt"
done
