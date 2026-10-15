#!/bin/sh
# Stop irys-sm-bench and restore /sys/block/<dev>/queue settings.
#
# usage: teardown.sh <dev> <sysfs-orig-file>
#
# <dev> is a /sys/block name such as sdc or dm-0. A hyphen is allowed.
# Empty, slash, and "." / ".." are refused.
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
# The pid in run.pid next to the sysfs file is signaled when its comm is
# irys-sm-bench. When that pid is a process group leader, the whole group is
# signaled so a child does not keep writing. The queue is restored only after
# that pid is gone (a zombie is gone: it is not doing I/O). A missing, empty,
# or non-numeric run.pid exits before any queue write. A live pid whose comm
# is not irys-sm-bench exits the same way. Other processes are left alone.
# A numeric pid that is already gone has finished I/O, so the queue is restored.
#
# sample.pid next to run.pid is optional. After the bench check succeeds, a
# numeric pid whose command line contains sample.sh is signaled. A group
# leader is signaled with the group so iostat stops too. The queue write
# waits until that group is idle. A missing, empty, or non-numeric sample.pid
# does not change the exit status. A live pid that is not sample.sh is left
# alone.

set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: teardown.sh <dev> <sysfs-orig-file>" >&2
    exit 1
fi

dev=$1
orig=$2

case "$dev" in
    "" | . | .. | *[!A-Za-z0-9-]*)
        echo "device name must be alphanumeric or '-': $dev" >&2
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

# run.sh records the bench pid beside the sysfs snapshot. Signal that
# process only, then wait until it is gone before the queue write below.
proc_busy() {
    if [ ! -d "/proc/$1" ]; then
        return 1
    fi
    state=$(awk '/^State:/{print $2}' "/proc/$1/status" 2>/dev/null || true)
    if [ -z "$state" ] || [ "$state" = "Z" ]; then
        return 1
    fi
    return 0
}

# 0 when any non-zombie task still has this process group.
group_busy() {
    want=$1
    ps -eo pgid=,stat= 2>/dev/null | awk -v want="$want" '
        $1 == want && $2 !~ /^Z/ { found = 1 }
        END { exit (found ? 0 : 1) }
    '
}

wait_until_idle() {
    pid=$1
    pgid=$2
    i=0
    while proc_busy "$pid"; do
        i=$((i + 1))
        if [ "$i" -ge 50 ]; then
            if [ -n "$pgid" ] && [ "$pgid" = "$pid" ]; then
                kill -KILL "-$pgid" 2>/dev/null || true
            else
                kill -KILL "$pid" 2>/dev/null || true
            fi
            i=0
            while proc_busy "$pid"; do
                i=$((i + 1))
                if [ "$i" -ge 20 ]; then
                    echo "run pid $pid did not exit; not restoring the queue" >&2
                    exit 1
                fi
                sleep 0.1
            done
            return 0
        fi
        sleep 0.1
    done
}

pid_file=$(dirname "$orig")/run.pid
if [ ! -f "$pid_file" ]; then
    echo "run.pid is missing: $pid_file" >&2
    exit 1
fi
pid=$(head -n 1 "$pid_file" | tr -d '[:space:]')
case "$pid" in
    '' | *[!0-9]*)
        echo "run.pid is not a pid: $pid" >&2
        exit 1
        ;;
esac
if [ -d "/proc/$pid" ]; then
    comm=$(tr -d '[:space:]' < "/proc/$pid/comm" 2>/dev/null || true)
    if [ "$comm" != "irys-sm-bench" ]; then
        echo "run pid $pid is ${comm:-unknown}, not irys-sm-bench; not restoring the queue" >&2
        exit 1
    fi
    if proc_busy "$pid"; then
        pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d '[:space:]' || true)
        if [ -n "$pgid" ] && [ "$pgid" = "$pid" ]; then
            kill -TERM "-$pgid" 2>/dev/null || true
        else
            kill -TERM "$pid" 2>/dev/null || true
        fi
        wait_until_idle "$pid" "$pgid"
    fi
fi

# The bench check above has succeeded. Stop the sampler, if one was recorded,
# before the queue write. A bad sample.pid must not block that write.
sample_file=$(dirname "$orig")/sample.pid
if [ -f "$sample_file" ]; then
    sample_pid=$(head -n 1 "$sample_file" | tr -d '[:space:]')
    case "$sample_pid" in
        '' | *[!0-9]*)
            ;;
        *)
            if [ -d "/proc/$sample_pid" ]; then
                cmd=$(tr '\0' ' ' < "/proc/$sample_pid/cmdline" 2>/dev/null || true)
                case "$cmd" in
                    *sample.sh*)
                        spgid=$(ps -o pgid= -p "$sample_pid" 2>/dev/null | tr -d '[:space:]' || true)
                        if [ -n "$spgid" ] && [ "$spgid" = "$sample_pid" ]; then
                            kill -TERM "-$spgid" 2>/dev/null || true
                            i=0
                            while group_busy "$spgid"; do
                                i=$((i + 1))
                                if [ "$i" -ge 30 ]; then
                                    kill -KILL "-$spgid" 2>/dev/null || true
                                    break
                                fi
                                sleep 0.1
                            done
                        else
                            kill -TERM "$sample_pid" 2>/dev/null || true
                            i=0
                            while proc_busy "$sample_pid"; do
                                i=$((i + 1))
                                if [ "$i" -ge 30 ]; then
                                    kill -KILL "$sample_pid" 2>/dev/null || true
                                    break
                                fi
                                sleep 0.1
                            done
                        fi
                        ;;
                esac
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
