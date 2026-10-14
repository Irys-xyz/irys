#!/bin/sh
# Stop irys-sm-bench and restore /sys/block/<dev>/queue settings.
#
# usage: teardown.sh <dev> <sysfs-orig-file>
#
# <dev> is a /sys/block name such as sdc. The name must be alphanumeric.
# <sysfs-orig-file> is `key=value` lines. The keys are:
#   max_sectors_kb
#   read_ahead_kb
#   nr_requests
#   scheduler
# A scheduler value may be wrapped in brackets, as in `[mq-deadline]`.
# The script writes the name without brackets.
#
# This script does not delete bench files. The operator deletes those by hand.
# It does not use sudo. A permission error on sysfs is left as the shell error.
#
# The pid in run.pid next to the sysfs file is signaled. Other processes
# with the bench's name are left alone. A missing run.pid signals nothing.

set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: teardown.sh <dev> <sysfs-orig-file>" >&2
    exit 1
fi

dev=$1
orig=$2

case "$dev" in
    *[!A-Za-z0-9]*)
        echo "device name must be alphanumeric: $dev" >&2
        exit 1
        ;;
esac

if [ ! -f "$orig" ]; then
    echo "sysfs orig file not found: $orig" >&2
    exit 1
fi

queue="/sys/block/$dev/queue"
if [ ! -d "$queue" ]; then
    echo "no queue directory: $queue" >&2
    exit 1
fi

# run.sh records its pid beside the sysfs snapshot. Signal that process only.
pid_file=$(dirname "$orig")/run.pid
if [ -f "$pid_file" ]; then
    pid=$(head -n 1 "$pid_file" | tr -d '[:space:]')
    case "$pid" in
        '' | *[!0-9]*)
            echo "run.pid is not a pid: $pid" >&2
            ;;
        *)
            if [ -d "/proc/$pid" ]; then
                kill -TERM "$pid" 2>/dev/null || true
            fi
            ;;
    esac
fi

while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
        '' | '#'*)
            continue
            ;;
    esac
    key=${line%%=*}
    value=${line#*=}
    case "$key" in
        max_sectors_kb | read_ahead_kb | nr_requests | scheduler)
            ;;
        *)
            echo "unknown sysfs key: $key" >&2
            exit 1
            ;;
    esac
    value=$(printf '%s' "$value" | tr -d '[]' | tr -d '[:space:]')
    if [ -z "$value" ]; then
        echo "empty value for $key" >&2
        exit 1
    fi
    printf '%s\n' "$value" >"$queue/$key"
    echo "restored $key=$value"
done <"$orig"
