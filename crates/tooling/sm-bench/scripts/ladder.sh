#!/bin/bash
# Run a list of irys-sm-bench ladders on one disk.
#
# usage:
#   ladder.sh --config FILE
#   ladder.sh --device DEV --mount MOUNT --loadtest DIR --out-root DIR --bench PATH
#             [--max-sectors-kb N] [--restore-sectors-kb N] [--drop-cache]
#             [--trace] [--done FILE] [--prior-name NAME] [--common 'ARGS']
#             --run 'NAME BENCH-ARGS' [--run 'NAME BENCH-ARGS' ...]
#
# A flag overrides the same key in the config. Run lines from the config and
# from --run are both used, config first.
#
# Config keys (key=value), and `run` lines:
#   device, mount, loadtest, out_root, bench
#   recall_file, write_file          optional; run.sh defaults apply
#   max_sectors_kb                   value to set for the ladder
#   restore_sectors_kb               value to put back; default is the value read before the set
#   drop_cache=true|false            drop the write-region page cache before each run
#   trace=true|false                 wrap each run in blk-run.sh and blk-report.py
#   done                             done-marker path; default <out_root>/ladder.done
#   prior_name                       directory name for state that already exists
#   common                           bench args added to every run
#   run NAME [bench args...]         one ladder
#
# State (db, rocks, intervals.json, packing_params.toml, bench-prepop.json)
# is renamed into loadtest/state-<name>/ before the next run. Nothing is
# deleted. An existing state directory is an error.
#
# The loadtest path and the mount disk are checked before that rename and
# before max_sectors_kb is written. A path without a loadtest component, or a
# mount whose disk is not --device, leaves the index files and the queue alone.
#
# max_sectors_kb is restored on EXIT, INT, and TERM.
# The done marker is written when the script exits, including after a failure.
# Run under setsid and nohup so the ladder does not need a terminal.
#
# Bench failures do not stop the ladder (run.sh exits 0). A guard failure
# stops it. sudo must not prompt. --trace needs bpftrace.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

cli_device=
cli_mount=
cli_loadtest=
cli_out_root=
cli_bench=
cli_recall=
cli_write=
cli_max=
cli_restore=
cli_drop=
cli_trace=
cli_done=
cli_prior=
cli_common=
config=
declare -a run_names=()
declare -a run_args=()
declare -a cli_run_specs=()

usage() {
    echo "usage: ladder.sh --config FILE | (--device DEV --mount MOUNT --loadtest DIR --out-root DIR --bench PATH --run 'NAME ARGS' ...)" >&2
}

add_run() {
    local spec=$1
    local name args
    read -r name args <<<"$spec"
    if [ -z "$name" ]; then
        echo "run is missing a name" >&2
        exit 1
    fi
    case "$name" in
        *[!A-Za-z0-9._-]*)
            echo "run name must be a single path component: $name" >&2
            exit 1
            ;;
    esac
    run_names+=("$name")
    run_args+=("${args:-}")
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)
            config=$2
            shift 2
            ;;
        --device)
            cli_device=$2
            shift 2
            ;;
        --mount)
            cli_mount=$2
            shift 2
            ;;
        --loadtest)
            cli_loadtest=$2
            shift 2
            ;;
        --out-root)
            cli_out_root=$2
            shift 2
            ;;
        --bench)
            cli_bench=$2
            shift 2
            ;;
        --recall-file)
            cli_recall=$2
            shift 2
            ;;
        --write-file)
            cli_write=$2
            shift 2
            ;;
        --max-sectors-kb)
            cli_max=$2
            shift 2
            ;;
        --restore-sectors-kb)
            cli_restore=$2
            shift 2
            ;;
        --drop-cache)
            cli_drop=1
            shift
            ;;
        --trace)
            cli_trace=1
            shift
            ;;
        --done)
            cli_done=$2
            shift 2
            ;;
        --prior-name)
            cli_prior=$2
            shift 2
            ;;
        --common)
            cli_common=$2
            shift 2
            ;;
        --run)
            cli_run_specs+=("$2")
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

cfg_device=
cfg_mount=
cfg_loadtest=
cfg_out_root=
cfg_bench=
cfg_recall=
cfg_write=
cfg_max=
cfg_restore=
cfg_drop=0
cfg_trace=0
cfg_done=
cfg_prior=
cfg_common=

as_bool() {
    case "$1" in
        1 | true | yes | TRUE | YES) echo 1 ;;
        0 | false | no | FALSE | NO | "") echo 0 ;;
        *)
            echo "bad boolean: $1" >&2
            return 1
            ;;
    esac
}

if [ -n "$config" ]; then
    if [ ! -f "$config" ]; then
        echo "config not found: $config" >&2
        exit 1
    fi
    while IFS= read -r line || [ -n "$line" ]; do
        line=${line#"${line%%[![:space:]]*}"}
        case "$line" in
            "" | "#"*) continue ;;
        esac
        case "$line" in
            run\ *)
                add_run "${line#run }"
                continue
                ;;
        esac
        key=${line%%=*}
        value=${line#*=}
        if [ "$key" = "$line" ]; then
            echo "config line is not key=value or run: $line" >&2
            exit 1
        fi
        case "$key" in
            device) cfg_device=$value ;;
            mount) cfg_mount=$value ;;
            loadtest) cfg_loadtest=$value ;;
            out_root) cfg_out_root=$value ;;
            bench) cfg_bench=$value ;;
            recall_file) cfg_recall=$value ;;
            write_file) cfg_write=$value ;;
            max_sectors_kb) cfg_max=$value ;;
            restore_sectors_kb) cfg_restore=$value ;;
            drop_cache) cfg_drop=$(as_bool "$value") ;;
            trace) cfg_trace=$(as_bool "$value") ;;
            done) cfg_done=$value ;;
            prior_name) cfg_prior=$value ;;
            common) cfg_common=$value ;;
            *)
                echo "unknown config key: $key" >&2
                exit 1
                ;;
        esac
    done <"$config"
fi

for spec in "${cli_run_specs[@]}"; do
    add_run "$spec"
done

device=${cli_device:-$cfg_device}
mount=${cli_mount:-$cfg_mount}
loadtest=${cli_loadtest:-$cfg_loadtest}
out_root=${cli_out_root:-$cfg_out_root}
bench=${cli_bench:-$cfg_bench}
recall_file=${cli_recall:-$cfg_recall}
write_file=${cli_write:-$cfg_write}
max_sectors_kb=${cli_max:-$cfg_max}
restore_sectors_kb=${cli_restore:-$cfg_restore}
drop_cache=${cli_drop:-$cfg_drop}
trace=${cli_trace:-$cfg_trace}
done_file=${cli_done:-$cfg_done}
prior_name=${cli_prior:-${cfg_prior:-prior}}
common=${cli_common:-$cfg_common}

case "$prior_name" in
    *[!A-Za-z0-9._-]*)
        echo "prior name must be a single path component: $prior_name" >&2
        exit 1
        ;;
esac

if [ -z "$device" ] || [ -z "$mount" ] || [ -z "$loadtest" ] || [ -z "$out_root" ] || [ -z "$bench" ]; then
    echo "device, mount, loadtest, out_root, and bench are required" >&2
    usage
    exit 1
fi
if [ "${#run_names[@]}" -eq 0 ]; then
    echo "no runs" >&2
    exit 1
fi
if [ ! -d "$loadtest" ]; then
    echo "loadtest dir not found: $loadtest" >&2
    exit 1
fi
if [ ! -x "$bench" ]; then
    echo "bench binary is not executable: $bench" >&2
    exit 1
fi

case "$device" in
    *[!A-Za-z0-9]*)
        echo "device name must be alphanumeric: $device" >&2
        exit 1
        ;;
esac

recall_file=${recall_file:-$loadtest/entropy-chunks.dat}
write_file=${write_file:-$loadtest/write-region.dat}
done_file=${done_file:-$out_root/ladder.done}
mkdir -p "$(dirname "$done_file")" "$out_root"

queue=/sys/block/$device/queue
if [ ! -d "$queue" ]; then
    echo "no queue directory: $queue" >&2
    exit 1
fi

sectors_changed=0
restore_value=
restored_now=
done_written=0

write_queue() {
    local key=$1
    local value=$2
    local path=$queue/$key
    if [ "$(id -u)" -eq 0 ]; then
        printf '%s\n' "$value" >"$path"
    else
        printf '%s\n' "$value" | sudo tee "$path" >/dev/null
    fi
}

restore_sectors() {
    if [ "$sectors_changed" != 1 ]; then
        return 0
    fi
    write_queue max_sectors_kb "$restore_value" || true
    sectors_changed=0
    restored_now=$(cat "$queue/max_sectors_kb" 2>/dev/null || echo unknown)
}

mark_done() {
    local rc=$1
    if [ "$done_written" = 1 ]; then
        return 0
    fi
    done_written=1
    if [ "$rc" -eq 0 ]; then
        local now
        now=$(cat "$queue/max_sectors_kb" 2>/dev/null || echo unknown)
        echo "restored max_sectors_kb=$now $(date -Is)" >"$done_file"
    else
        echo "failed rc=$rc restored max_sectors_kb=${restored_now:-unknown} $(date -Is)" >"$done_file"
    fi
}

on_exit() {
    local rc=$?
    restore_sectors
    mark_done "$rc"
}
trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
    restore_sectors
    echo "$* restored max_sectors_kb=${restored_now:-unknown} $(date -Is)" | tee "$done_file" >&2
    done_written=1
    exit 1
}

state_names=(db rocks intervals.json packing_params.toml bench-prepop.json)

stash() {
    local name=$1
    local dest=$loadtest/state-$name
    local any=0
    local item
    for item in "${state_names[@]}"; do
        if [ -e "$loadtest/$item" ]; then
            any=1
            break
        fi
    done
    if [ "$any" -eq 0 ]; then
        return 0
    fi
    if [ -e "$dest" ]; then
        echo "state dir already exists: $dest" >&2
        return 1
    fi
    mkdir "$dest" || return 1
    for item in "${state_names[@]}"; do
        if [ -e "$loadtest/$item" ]; then
            mv "$loadtest/$item" "$dest/" || return 1
        fi
    done
}

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

# Same mount-to-disk check as blk-run.sh. The queue write below must not run
# for a disk that does not hold the loadtest directory.
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

require_loadtest "loadtest" "$loadtest"
require_loadtest "recall-file" "$recall_file"
require_loadtest "write-file" "$write_file"
require_mount_device "$(readlink -f "$loadtest")" "$device"

if [ -n "$max_sectors_kb" ]; then
    current=$(cat "$queue/max_sectors_kb")
    restore_value=${restore_sectors_kb:-$current}
    # Set the flag before the write so a failed write still restores.
    sectors_changed=1
    write_queue max_sectors_kb "$max_sectors_kb"
fi

run_one() {
    local name=$1
    local args=$2
    local out=$out_root/$name
    mkdir -p "$out"
    if [ "$drop_cache" = 1 ]; then
        dd if="$write_file" iflag=nocache count=0 status=none || return 1
    fi
    local -a common_words=()
    local -a arg_words=()
    if [ -n "$common" ]; then
        # Config and --common split on whitespace. Arguments have no spaces.
        read -r -a common_words <<<"$common"
    fi
    if [ -n "$args" ]; then
        read -r -a arg_words <<<"$args"
    fi
    local -a cmd=(
        "$here/run.sh"
        --name "$name"
        --device "$device"
        --mount "$mount"
        --loadtest "$loadtest"
        --out-root "$out_root"
        --bench "$bench"
        --recall-file "$recall_file"
        --write-file "$write_file"
        --
    )
    if [ "${#common_words[@]}" -gt 0 ]; then
        cmd+=("${common_words[@]}")
    fi
    if [ "${#arg_words[@]}" -gt 0 ]; then
        cmd+=("${arg_words[@]}")
    fi
    if [ "$trace" = 1 ]; then
        "$here/blk-run.sh" "$device" "$loadtest" "$out" >"$out/blk-run.out" 2>&1 &
        local bp=$!
        sleep 5
        if ! kill -0 "$bp" 2>/dev/null; then
            cat "$out/blk-run.out" "$out/bpftrace.err" 2>/dev/null || true
            echo "blk-run failed to start for $name" >&2
            return 1
        fi
        "${cmd[@]}" || return 1
        # TERM, not INT: a background job of a non-interactive shell ignores
        # SIGINT, so blk-run.sh would never see it. blk-run.sh traps TERM.
        kill -TERM "$bp" 2>/dev/null || true
        wait "$bp" || true
        if ! python3 "$here/blk-report.py" "$out" >"$out/blk-report.out" 2>"$out/blk-report.err"; then
            echo "blk-report failed" >>"$out/blk-report.out"
        fi
    else
        "${cmd[@]}" || return 1
    fi
}

prev=$prior_name
for i in "${!run_names[@]}"; do
    if ! stash "$prev"; then
        fail "stash $prev failed"
    fi
    if ! run_one "${run_names[$i]}" "${run_args[$i]}"; then
        fail "run ${run_names[$i]} failed"
    fi
    prev=${run_names[$i]}
done
