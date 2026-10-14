#!/bin/bash
# Pre-write a file with O_DIRECT so XFS extents are written, not unwritten.
#
# An unwritten extent reads back as zeros and does not touch the disk.
# The log has one line per segment: index offset_bytes len_bytes seconds MiB/s.
# Segment time comes from /proc/uptime (monotonic). The log also gets wall-clock
# start and done lines.
#
# usage:
#   fill.sh --file PATH --log PATH [--segment-bytes N] [--block SIZE]
#           [--from INDEX] [--stop-file PATH] [--pid-file PATH]
#
# --from INDEX resumes at that segment. The log is appended.
# Creating --stop-file makes the filler exit 0 after the current segment.
# Default stop file is ./STOP. Default segment is 64 GiB. Default block is 64M.
# The file must already exist. This script does not allocate it.
# The path must contain a `loadtest` component, and so must `readlink -f`
# of the file. A symlink out of `loadtest` is refused before any write.

set -euo pipefail

file=
log=
segment_bytes=$((64 * 1024 * 1024 * 1024))
block=64M
start_i=0
stop_file=./STOP
pid_file=

usage() {
    echo "usage: fill.sh --file PATH --log PATH [--segment-bytes N] [--block SIZE] [--from INDEX] [--stop-file PATH] [--pid-file PATH]" >&2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --file)
            file=$2
            shift 2
            ;;
        --log)
            log=$2
            shift 2
            ;;
        --segment-bytes)
            segment_bytes=$2
            shift 2
            ;;
        --block)
            block=$2
            shift 2
            ;;
        --from)
            start_i=$2
            shift 2
            ;;
        --stop-file)
            stop_file=$2
            shift 2
            ;;
        --pid-file)
            pid_file=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage
            exit 1
            ;;
    esac
done

if [ -z "$file" ] || [ -z "$log" ]; then
    echo "--file and --log are required" >&2
    usage
    exit 1
fi
if [ ! -f "$file" ]; then
    echo "file not found: $file" >&2
    exit 1
fi
if [ "$segment_bytes" -le 0 ]; then
    echo "--segment-bytes must be greater than 0" >&2
    exit 1
fi

has_loadtest() {
    case "/$1/" in
        */loadtest/*) return 0 ;;
        *) return 1 ;;
    esac
}

require_loadtest() {
    local label=$1
    local path=$2
    if ! has_loadtest "$path"; then
        echo "$label $path has no loadtest path component; refusing" >&2
        exit 1
    fi
    if [ -e "$path" ]; then
        local canon
        canon=$(readlink -f "$path")
        if ! has_loadtest "$canon"; then
            echo "$label resolves to $canon, which has no loadtest path component; refusing" >&2
            exit 1
        fi
    fi
}

require_loadtest "file" "$file"

cleanup() {
    if [ -n "$pid_file" ]; then
        rm -f "$pid_file"
    fi
}
trap cleanup EXIT

if [ -n "$pid_file" ]; then
    echo $$ >"$pid_file"
fi

mono() {
    awk '{printf "%.6f\n", $1}' /proc/uptime
}

size=$(stat -c %s "$file")
n=$(((size + segment_bytes - 1) / segment_bytes))
mkdir -p "$(dirname "$log")"
echo "# start $(date -Is) size=$size segments=$n from=$start_i segment_bytes=$segment_bytes" >>"$log"

i=$start_i
while [ "$i" -lt "$n" ]; do
    if [ -e "$stop_file" ]; then
        echo "# stopped at $i $(date -Is)" >>"$log"
        exit 0
    fi
    off=$((i * segment_bytes))
    len=$segment_bytes
    if [ $((size - off)) -lt "$len" ]; then
        len=$((size - off))
    fi
    t0=$(mono)
    if ! dd if=/dev/zero of="$file" bs="$block" iflag=count_bytes oflag=direct,seek_bytes conv=notrunc \
        seek="$off" count="$len" status=none; then
        echo "# dd failed at $i $(date -Is)" >>"$log"
        exit 1
    fi
    t1=$(mono)
    awk -v i="$i" -v o="$off" -v l="$len" -v a="$t0" -v b="$t1" \
        'BEGIN{d=b-a; if (d <= 0) d = 0.001; printf "%d %d %d %.1f %.1f\n", i, o, l, d, l/d/1048576}' >>"$log"
    i=$((i + 1))
done

echo "# done $(date -Is)" >>"$log"
