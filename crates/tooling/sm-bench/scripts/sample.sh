#!/bin/sh
# Log disk and memory counters beside an irys-sm-bench run.
#
# usage: sample.sh <dev> <out-dir>
#
# <dev> is a /sys/block name such as sdc. The name must be alphanumeric.
# The script writes <out-dir>/iostat.log and <out-dir>/meminfo-delta.log.
# It stops when iostat exits. INT and TERM kill iostat.
#
# TODO: bpftrace per-file split of the recall file and the write file.

set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: sample.sh <dev> <out-dir>" >&2
    exit 1
fi

dev=$1
out=$2

case "$dev" in
    *[!A-Za-z0-9]*)
        echo "device name must be alphanumeric: $dev" >&2
        exit 1
        ;;
esac

if ! command -v iostat >/dev/null 2>&1; then
    echo "iostat is not installed (package sysstat)" >&2
    exit 1
fi

mkdir -p "$out"

pid=
cleanup() {
    if [ -n "${pid:-}" ]; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        pid=
    fi
}
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
trap cleanup EXIT

iostat -dmx "$dev" 1 >"$out/iostat.log" 2>&1 &
pid=$!

prev=$(mktemp)
cp /proc/meminfo "$prev"
: >"$out/meminfo-delta.log"

while kill -0 "$pid" 2>/dev/null; do
    sleep 1
    kill -0 "$pid" 2>/dev/null || break
    now=$(date +%s)
    awk -v ts="$now" '
        NR == FNR { prev[$1] = $2 + 0; next }
        {
            key = $1
            val = $2 + 0
            old = (key in prev) ? prev[key] : 0
            printf "%s %s %d\n", ts, key, val - old
        }
    ' "$prev" /proc/meminfo >>"$out/meminfo-delta.log"
    cp /proc/meminfo "$prev"
done

rm -f "$prev"
wait "$pid" 2>/dev/null || true
pid=
