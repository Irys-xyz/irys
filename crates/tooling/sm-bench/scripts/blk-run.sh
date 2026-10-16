#!/bin/bash
# Trace one disk while irys-sm-bench runs.
#
# usage: blk-run.sh <dev> <loadtest-dir> <out-dir>
#
# <dev> is a whole-disk name such as sdc, or /dev/sdc. Not a partition.
# <loadtest-dir> must contain a path component named loadtest, before and
# after symlink resolution. The script reads extent metadata only.
# <out-dir> should be on a different filesystem from <dev>.
#
# Starts bpftrace (via sudo, unless already root), snapshots extents about
# every 30 s, and samples io_ticks every 1 s. INT and TERM stop the trace,
# take one last extent snapshot, and exit.
#
# Writes, under <out-dir>:
#   trace.log     bpftrace lines
#   bpftrace.err  bpftrace diagnostics
#   device.txt    name, dev_t, sector count
#   busy.log      monotonic_ns realtime_ns io_ticks_ms
#   extents-*.txt from extent-snap.sh

set -eu

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh
source "$here/lib.sh"

if [ "$#" -ne 3 ]; then
    echo "usage: blk-run.sh <dev> <loadtest-dir> <out-dir>" >&2
    exit 1
fi

dev=$1
loadtest=$2
out=$3
dev=${dev#/dev/}

# A mapper name such as dm-0 contains a hyphen. Empty, slash, and "." / ".."
# stay refused.
require_device_name "$dev"

if ! has_loadtest "$loadtest"; then
    echo "refusing: $loadtest has no loadtest path component" >&2
    exit 1
fi
if [ ! -d "$loadtest" ]; then
    echo "loadtest dir not found: $loadtest" >&2
    exit 1
fi
canon=$(CDPATH= cd -- "$loadtest" && pwd -P)
if ! has_loadtest "$canon"; then
    echo "refusing: canonical path $canon has no loadtest path component" >&2
    exit 1
fi

if [ ! -b "/dev/$dev" ]; then
    echo "not a block device: /dev/$dev" >&2
    exit 1
fi
if [ -e "/sys/class/block/$dev/partition" ]; then
    echo "pass the whole disk, not the partition $dev" >&2
    exit 1
fi

require_mount_device "$canon" "$dev"
# The snapshot needs the mount path as findmnt printed it, not the canonical path.
if command -v findmnt >/dev/null 2>&1; then
    mnt=$(findmnt -n -o TARGET -T "$canon" || true)
else
    mnt=$(df -P "$canon" | awk 'NR==2 { print $6 }')
fi
if [ -z "${mnt:-}" ]; then
    echo "no mount for $canon" >&2
    exit 1
fi

bt=$here/blk-trace.bt
snap=$here/extent-snap.sh

# The snapshot's own process group, so TERM reaches filefrag and not only the shell.
start_snap() {
    set -m
    "$snap" "$mnt" "$canon" "$out" &
    snap_pid=$!
    set +m
}

stop_snap() {
    if [ -z "${snap_pid:-}" ]; then
        return 0
    fi
    local pid=$snap_pid
    snap_pid=
    if ! kill -TERM -- "-$pid" 2>/dev/null; then
        local kids
        kids=$(ps -o pid= --ppid "$pid" 2>/dev/null || true)
        if [ -n "$kids" ]; then
            # shellcheck disable=SC2086
            kill -TERM $kids 2>/dev/null || true
        fi
        kill -TERM "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}

if [ ! -f "$bt" ] || [ ! -f "$snap" ]; then
    echo "missing blk-trace.bt or extent-snap.sh next to blk-run.sh" >&2
    exit 1
fi

maj_hex=$(stat -c '%t' "/dev/$dev")
min_hex=$(stat -c '%T' "/dev/$dev")
# Linux dev_t: minor in the low 20 bits.
dev_t=$(( (0x$maj_hex << 20) | 0x$min_hex ))
sectors=$(tr -d '[:space:]' < "/sys/class/block/$dev/size")

mkdir -p "$out"
cat >"$out/device.txt" <<EOF
name=$dev
dev_t=$dev_t
sectors=$sectors
sector_bytes=512
EOF

bpid=
spid=
sleep_pid=
snap_pid=
cleaned=0

cleanup() {
    status=$1
    if [ "$cleaned" -eq 1 ]; then
        exit "$status"
    fi
    cleaned=1
    trap '' INT TERM EXIT
    # A foreground sleep or snapshot would hold this trap until it returned.
    # Those commands are waited in the background, so kill them here.
    if [ -n "$sleep_pid" ]; then
        kill -TERM "$sleep_pid" 2>/dev/null || true
        wait "$sleep_pid" 2>/dev/null || true
        sleep_pid=
    fi
    stop_snap
    if [ -n "$bpid" ]; then
        kill -INT "$bpid" 2>/dev/null || true
        child=$(ps -o pid= --ppid "$bpid" 2>/dev/null || true)
        if [ -n "$child" ]; then
            # shellcheck disable=SC2086
            kill -INT $child 2>/dev/null || true
        fi
        wait "$bpid" 2>/dev/null || true
        bpid=
    fi
    if [ -n "${mnt:-}" ]; then
        "$snap" "$mnt" "$canon" "$out" >/dev/null || true
    fi
    if [ -n "$spid" ]; then
        kill -TERM "$spid" 2>/dev/null || true
        wait "$spid" 2>/dev/null || true
        spid=
    fi
    exit "$status"
}

trap 'cleanup 130' INT
trap 'cleanup 143' TERM
trap 'cleanup $?' EXIT

if [ "$(id -u)" -eq 0 ]; then
    if command -v stdbuf >/dev/null 2>&1; then
        stdbuf -oL bpftrace "$bt" "$dev_t" >"$out/trace.log" 2>"$out/bpftrace.err" &
    else
        bpftrace "$bt" "$dev_t" >"$out/trace.log" 2>"$out/bpftrace.err" &
    fi
else
    sudo -v
    if command -v stdbuf >/dev/null 2>&1; then
        sudo stdbuf -oL bpftrace "$bt" "$dev_t" >"$out/trace.log" 2>"$out/bpftrace.err" &
    else
        sudo bpftrace "$bt" "$dev_t" >"$out/trace.log" 2>"$out/bpftrace.err" &
    fi
fi
bpid=$!

sleep 1 &
sleep_pid=$!
wait "$sleep_pid" || true
sleep_pid=
if ! kill -0 "$bpid" 2>/dev/null; then
    echo "bpftrace exited at start" >&2
    if [ -f "$out/bpftrace.err" ]; then
        cat "$out/bpftrace.err" >&2 || true
    fi
    exit 1
fi

python3 - "$dev" "$out/busy.log" <<'PY' &
import signal
import sys
import time

dev = sys.argv[1]
path = sys.argv[2]
stat_path = f"/sys/block/{dev}/stat"


def stop(_signum, _frame):
    sys.exit(0)


signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)

with open(path, "a", encoding="utf-8") as out:
    if out.tell() == 0:
        out.write("# monotonic_ns realtime_ns io_ticks_ms\n")
    while True:
        with open(stat_path, encoding="utf-8") as handle:
            fields = handle.read().split()
        if len(fields) < 10:
            print("short /sys/block stat", file=sys.stderr)
            sys.exit(1)
        # Field 10 is io_ticks (ms). iostat %util is its delta.
        ticks = fields[9]
        mono = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
        real = time.clock_gettime_ns(time.CLOCK_REALTIME)
        out.write(f"{mono} {real} {ticks}\n")
        out.flush()
        time.sleep(1)
PY
spid=$!

start_snap
wait "$snap_pid" || echo "extent-snap failed" >&2
snap_pid=

while kill -0 "$bpid" 2>/dev/null; do
    sleep 30 &
    sleep_pid=$!
    wait "$sleep_pid" || true
    sleep_pid=
    if ! kill -0 "$bpid" 2>/dev/null; then
        break
    fi
    start_snap
    wait "$snap_pid" || echo "extent-snap failed" >&2
    snap_pid=
done
