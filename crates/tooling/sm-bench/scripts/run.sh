#!/bin/bash
# Run irys-sm-bench with the sampler beside it.
#
# usage:
#   run.sh --name NAME --device DEV --mount MOUNT --loadtest DIR --out-root DIR
#          [--bench PATH] [--recall-file PATH] [--write-file PATH]
#          [--min-free-gib N] [-- bench args...]
#
# The same names exist as environment variables: NAME, DEVICE, MOUNT,
# LOADTEST, OUT_ROOT, BENCH, RECALL_FILE, WRITE_FILE, MIN_FREE_GIB.
# A flag wins over the environment.
#
# Guards, all of them refuse the run:
#   pm2 is installed and the app "Irys Node" is online, or its jlist cannot be read
#   a missing pm2 binary is allowed
#   the loadtest disk has less than MIN_FREE_GIB free (default 500), counted in bytes
#   --mount is not the mount that holds the loadtest directory
#   loadtest, the recall file, or the write file has no `loadtest` path component
#   on the given path, or on the deepest existing ancestor plus the missing tail
#   --device is not the disk that holds the loadtest directory
# The queue settings are written to <out>/sysfs.txt before the bench starts.
# <out>/run.pid is the bench process, not this shell. The bench runs in its
# own process group. TERM and INT stop that group. The EXIT trap removes
# run.pid after that process is gone, so a later teardown cannot signal a
# reused pid. <out>/sample.pid is the sampler, written before the bench
# starts. The EXIT trap removes it after the sampler is gone.
#
# The bench's exit code is appended to <out>/bench.out. This script exits 0
# after a bench failure so a ladder can continue. A guard failure exits 1.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

name=${NAME:-}
device=${DEVICE:-}
mount=${MOUNT:-}
loadtest=${LOADTEST:-}
out_root=${OUT_ROOT:-}
bench=${BENCH:-}
recall_file=${RECALL_FILE:-}
write_file=${WRITE_FILE:-}
min_free_gib=${MIN_FREE_GIB:-500}
bench_args=()

usage() {
    echo "usage: run.sh --name NAME --device DEV --mount MOUNT --loadtest DIR --out-root DIR [--bench PATH] [--recall-file PATH] [--write-file PATH] [--min-free-gib N] [-- bench args...]" >&2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --name)
            name=$2
            shift 2
            ;;
        --device)
            device=$2
            shift 2
            ;;
        --mount)
            mount=$2
            shift 2
            ;;
        --loadtest)
            loadtest=$2
            shift 2
            ;;
        --out-root)
            out_root=$2
            shift 2
            ;;
        --bench)
            bench=$2
            shift 2
            ;;
        --recall-file)
            recall_file=$2
            shift 2
            ;;
        --write-file)
            write_file=$2
            shift 2
            ;;
        --min-free-gib)
            min_free_gib=$2
            shift 2
            ;;
        --)
            shift
            bench_args=("$@")
            break
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

if [ -z "$name" ] || [ -z "$device" ] || [ -z "$mount" ] || [ -z "$loadtest" ] || [ -z "$out_root" ]; then
    echo "name, device, mount, loadtest, and out-root are required" >&2
    usage
    exit 1
fi

# A mapper name such as dm-0 contains a hyphen. Empty, slash, and "." / ".."
# stay refused. The queue path below is quoted.
case "$device" in
    "" | . | .. | *[!A-Za-z0-9-]*)
        echo "device name must be alphanumeric or '-': $device" >&2
        exit 1
        ;;
esac

case "$name" in
    . | .. | *[!A-Za-z0-9._-]*)
        echo "run name must be a single path component: $name" >&2
        exit 1
        ;;
esac

if [ -z "$bench" ]; then
    bench=target/release/irys-sm-bench
fi
if [ ! -x "$bench" ]; then
    echo "bench binary is not executable: $bench" >&2
    exit 1
fi

recall_file=${recall_file:-$loadtest/entropy-chunks.dat}
write_file=${write_file:-$loadtest/write-region.dat}

has_loadtest() {
    case "/$1/" in
        */loadtest/*) return 0 ;;
        *) return 1 ;;
    esac
}

# Canonicalize the deepest existing ancestor, then apply the missing tail.
# `..` in that tail is applied, so loadtest/../../outside does not keep the component.
resolve_path() {
    local path=$1
    local -a missing=()
    local base parent
    while [ ! -e "$path" ] && [ ! -L "$path" ]; do
        base=$(basename -- "$path")
        parent=$(dirname -- "$path")
        if [ "$parent" = "$path" ]; then
            echo "cannot resolve $1" >&2
            return 1
        fi
        missing+=("$base")
        if [ -z "$parent" ]; then
            path=.
            break
        fi
        path=$parent
    done
    local canon part
    local i
    canon=$(readlink -f "$path") || return 1
    for ((i = ${#missing[@]} - 1; i >= 0; i--)); do
        part=${missing[$i]}
        if [ "$part" = "." ]; then
            continue
        fi
        if [ "$part" = ".." ]; then
            canon=$(dirname -- "$canon")
            continue
        fi
        canon=$canon/$part
    done
    printf '%s\n' "$canon"
}

require_loadtest() {
    local label=$1
    local path=$2
    if ! has_loadtest "$path"; then
        echo "$label $path has no loadtest path component; refusing" >&2
        exit 1
    fi
    local resolved
    resolved=$(resolve_path "$path") || exit 1
    if ! has_loadtest "$resolved"; then
        echo "$label resolves to $resolved, which has no loadtest path component; refusing" >&2
        exit 1
    fi
}

# Same mount-to-disk check as ladder.sh. Sampling and the sysfs snapshot must
# use the disk that holds the loadtest directory.
require_mount_device() {
    local canon=$1
    local dev=$2
    local mnt_src mnt
    if command -v findmnt >/dev/null 2>&1; then
        mnt_src=$(findmnt -n -o SOURCE -T "$canon" || true)
        mnt=$(findmnt -n -o TARGET -T "$canon" || true)
    else
        mnt_src=$(df -P "$canon" | awk 'NR==2 { print $1 }')
        mnt=$(df -P "$canon" | awk 'NR==2 { print $6 }')
    fi
    if [ -z "${mnt_src:-}" ] || [ -z "${mnt:-}" ]; then
        echo "no mount for $canon" >&2
        exit 1
    fi
    # findmnt appends a subvolume in brackets. The lab disk does not.
    mnt_src=${mnt_src%%[*}
    mnt_src=$(readlink -f "$mnt_src") || {
        echo "cannot resolve mount source for $canon" >&2
        exit 1
    }
    local mnt_name parent
    mnt_name=${mnt_src##*/}
    if [ -e "/sys/class/block/$mnt_name/partition" ]; then
        parent=$(basename "$(readlink -f "/sys/class/block/$mnt_name/..")")
    else
        parent=$mnt_name
    fi
    if [ "$parent" != "$dev" ]; then
        echo "refusing: $canon is on $mnt_name, not $dev" >&2
        exit 1
    fi
}

mount_target() {
    local path=$1
    local target
    if command -v findmnt >/dev/null 2>&1; then
        target=$(findmnt -n -o TARGET -T "$path" || true)
    else
        target=$(df -P "$path" | awk 'NR==2 { print $6 }')
    fi
    if [ -z "${target:-}" ]; then
        echo "no mount for $path" >&2
        exit 1
    fi
    readlink -f "$target"
}

require_loadtest "loadtest" "$loadtest"
require_loadtest "recall-file" "$recall_file"
require_loadtest "write-file" "$write_file"

if command -v pm2 >/dev/null 2>&1; then
    jlist=$(pm2 jlist 2>/dev/null || true)
    pm_status=0
    printf '%s' "$jlist" | python3 -c '
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    sys.exit(2)
try:
    apps = json.loads(raw)
except json.JSONDecodeError:
    sys.exit(2)
if not isinstance(apps, list):
    sys.exit(2)
online = False
for app in apps:
    if not isinstance(app, dict):
        continue
    env = app.get("pm2_env") or {}
    if app.get("name") == "Irys Node" and env.get("status") == "online":
        online = True
        break
sys.exit(0 if online else 1)
' || pm_status=$?
    if [ "$pm_status" -eq 0 ]; then
        echo "Irys Node is online; refusing" >&2
        exit 1
    fi
    if [ "$pm_status" -ne 1 ]; then
        echo "pm2 jlist was not readable; refusing" >&2
        exit 1
    fi
fi

queue=/sys/block/$device/queue
if [ ! -d "$queue" ]; then
    echo "no queue directory: $queue" >&2
    exit 1
fi

resolved_loadtest=$(resolve_path "$loadtest") || exit 1
mount_at=$resolved_loadtest
while [ ! -e "$mount_at" ] && [ ! -L "$mount_at" ]; do
    mount_at=$(dirname -- "$mount_at")
done
require_mount_device "$mount_at" "$device"

loadtest_mount=$(mount_target "$mount_at")
given_mount=$(mount_target "$mount")
if [ "$loadtest_mount" != "$given_mount" ]; then
    echo "refusing: --mount $mount is $given_mount, not the loadtest mount $loadtest_mount" >&2
    exit 1
fi

# df -B1G rounds to the nearest GiB, so a volume just under the floor can pass.
# Measure the loadtest path. --mount is only the operator's name for that mount.
avail_bytes=$(df -B1 --output=avail "$mount_at" | tail -n 1)
avail_bytes=${avail_bytes//[[:space:]]/}
min_free_bytes=$((min_free_gib * 1024 * 1024 * 1024))
if [ "$avail_bytes" -lt "$min_free_bytes" ]; then
    echo "$mount_at has ${avail_bytes} bytes free, below ${min_free_gib} GiB; refusing" >&2
    exit 1
fi

out=$out_root/$name
mkdir -p "$out"
raw_sched=$(cat "$queue/scheduler")
active_sched=$(awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\[/) { print $i; exit } }' "$queue/scheduler")
if [ -z "$active_sched" ]; then
    active_sched=$raw_sched
fi
{
    echo "max_sectors_kb=$(cat "$queue/max_sectors_kb")"
    echo "read_ahead_kb=$(cat "$queue/read_ahead_kb")"
    echo "nr_requests=$(cat "$queue/nr_requests")"
    # Brackets mark the active scheduler. teardown.sh strips them.
    # The raw line can contain every scheduler name, which teardown would join.
    echo "scheduler=$active_sched"
    echo "# scheduler_raw=$raw_sched"
} >"$out/sysfs.txt"

sp=
bp=
# A TERM trap does not run during a foreground command. Background the bench
# and wait, so TERM stops it before this shell exits. Monitor mode gives the
# bench its own process group; it is turned off again so job notices stay quiet.
cleanup() {
    trap - EXIT TERM INT
    if [ -n "${bp:-}" ]; then
        kill -TERM -- "-$bp" 2>/dev/null || kill -TERM "$bp" 2>/dev/null || true
        wait "$bp" 2>/dev/null || true
        bp=
    fi
    if [ -n "${sp:-}" ]; then
        kill -TERM "$sp" 2>/dev/null || true
        wait "$sp" 2>/dev/null || true
        sp=
    fi
    if [ -n "${out:-}" ]; then
        rm -f "$out/run.pid" "$out/sample.pid"
    fi
}
trap cleanup EXIT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 130' INT

set -m
"$here/sample.sh" "$device" "$out" &
sp=$!
# Record the sampler before the bench. A kill of this shell skips the trap
# above, and teardown.sh uses this pid to stop sample.sh and iostat.
printf '%s\n' "$sp" >"$out/sample.pid"
set +e
"$bench" \
    --dir "$loadtest" \
    --recall-file "$recall_file" \
    --write-file "$write_file" \
    --out "$out" \
    "${bench_args[@]}" >"$out/bench.out" 2>&1 &
bp=$!
set +m
printf '%s\n' "$bp" >"$out/run.pid"
wait "$bp"
rc=$?
set -e
bp=
echo "exit=$rc" >>"$out/bench.out"
echo "# done $(date -Is)" >>"$out/bench.out"
exit 0
