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
# The file, a symlink log, and --pid-file use the shared loadtest guard:
# the given path and the resolved path each contain a loadtest component,
# and the named parent is the parent after resolution. A symlink into
# another directory is refused before any write, including when that
# directory also contains loadtest. --pid-file is created with O_EXCL, and
# an existing symlink is not removed. TERM and INT stop the dd process group.
# The log's nearest loadtest directory must be the same directory as the
# named path's loadtest directory. A link that stays under that directory is
# allowed. A link into another loadtest directory is refused before mkdir
# or append. Parent equality would also refuse the same-tree link.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"

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

# Nearest directory whose own name is loadtest. The path is absolute.
loadtest_ancestor() {
    local dir=$1
    local next
    while [ "$dir" != "/" ] && [ "$dir" != "." ]; do
        if [ "$(basename -- "$dir")" = "loadtest" ]; then
            printf '%s\n' "$dir"
            return 0
        fi
        next=$(dirname -- "$dir")
        if [ "$next" = "$dir" ]; then
            break
        fi
        dir=$next
    done
    return 1
}

# mkdir and append follow a symlink ancestor. The named path and the resolved
# path must share one loadtest directory. A link into a second loadtest tree
# is refused before either write. A link that stays under the named tree is
# allowed, including when the log file's parent is that link.
require_same_loadtest_dir() {
    local log_path=$1
    if ! has_loadtest "$log_path"; then
        echo "log $log_path has no loadtest path component; refusing" >&2
        exit 1
    fi
    local resolved named_abs named_root resolved_root named_canon resolved_canon
    resolved=$(resolve_path "$log_path") || exit 1
    if ! has_loadtest "$resolved"; then
        echo "log resolves to $resolved, which has no loadtest path component; refusing" >&2
        exit 1
    fi
    named_abs=$(lexical_absolute "$log_path") || exit 1
    named_root=$(loadtest_ancestor "$named_abs") || {
        echo "log $log_path has no loadtest directory; refusing" >&2
        exit 1
    }
    resolved_root=$(loadtest_ancestor "$resolved") || {
        echo "log resolves to $resolved, which has no loadtest directory; refusing" >&2
        exit 1
    }
    named_canon=$(readlink -f "$named_root") || exit 1
    resolved_canon=$(readlink -f "$resolved_root") || exit 1
    if [ "$named_canon" != "$resolved_canon" ]; then
        echo "refusing log $log_path: its loadtest directory resolves to $resolved_canon, not $named_canon" >&2
        exit 1
    fi
}

require_loadtest "file" "$file"

pid_created=0
dd_pid=
# A TERM trap does not run during a foreground dd. Background dd and wait,
# so TERM stops that process group instead of leaving it running.
cleanup() {
    trap - EXIT TERM INT
    if [ -n "${dd_pid:-}" ]; then
        kill -TERM -- "-$dd_pid" 2>/dev/null || kill -TERM "$dd_pid" 2>/dev/null || true
        wait "$dd_pid" 2>/dev/null || true
        dd_pid=
    fi
    if [ "$pid_created" = 1 ] && [ ! -L "$pid_file" ]; then
        rm -f "$pid_file"
    fi
}
trap cleanup EXIT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 130' INT

if [ -L "$log" ]; then
    require_loadtest "log" "$log"
fi
require_same_loadtest_dir "$log"

if [ -n "$pid_file" ]; then
    require_loadtest "pid-file" "$pid_file"
    pid_parent=$(dirname -- "$pid_file")
    if [ -e "$pid_parent" ] || [ -L "$pid_parent" ]; then
        pid_parent_canon=$(readlink -f "$pid_parent") || exit 1
        if ! has_loadtest "$pid_parent_canon"; then
            echo "pid-file parent resolves to $pid_parent_canon, which has no loadtest path component; refusing" >&2
            exit 1
        fi
    fi
    # System dd, O_EXCL: an existing path (including a symlink) is not followed.
    if ! printf '%s\n' "$$" | command -p dd of="$pid_file" conv=excl status=none; then
        echo "refusing to create pid file: $pid_file" >&2
        exit 1
    fi
    pid_created=1
fi

mono() {
    awk '{printf "%.6f\n", $1}' /proc/uptime
}

# -L follows a symlink. The path-string length is not the file dd opens.
size=$(stat -Lc %s "$file")
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
    set -m
    set +e
    dd if=/dev/zero of="$file" bs="$block" iflag=count_bytes oflag=direct,seek_bytes conv=notrunc \
        seek="$off" count="$len" status=none &
    dd_pid=$!
    set +m
    wait "$dd_pid"
    dd_rc=$?
    set -e
    dd_pid=
    if [ "$dd_rc" -ne 0 ]; then
        echo "# dd failed at $i $(date -Is)" >>"$log"
        exit 1
    fi
    t1=$(mono)
    awk -v i="$i" -v o="$off" -v l="$len" -v a="$t0" -v b="$t1" \
        'BEGIN{d=b-a; if (d <= 0) d = 0.001; printf "%d %d %d %.1f %.1f\n", i, o, l, d, l/d/1048576}' >>"$log"
    i=$((i + 1))
done

echo "# done $(date -Is)" >>"$log"
