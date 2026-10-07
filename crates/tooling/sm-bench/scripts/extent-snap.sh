#!/bin/sh
# Snapshot physical extents of every regular file under <dir>.
#
# usage: extent-snap.sh <mountpoint> <dir> <out>
#
# Writes <out>/extents-<monotonic_ns>.txt
# Does not read file bytes. Uses filefrag -v (FIEMAP). If that returns no
# extent rows, tries xfs_bmap -v.
#
# Sectors are 512-byte sectors of the underlying disk. filefrag numbers are
# filesystem blocks. xfs_bmap -v numbers are already 512-byte sectors.
# /sys/class/block/<dev>/start is added so a partition lines up with the
# parent disk. On a whole-disk filesystem the start is 0.
#
# Header:
#   # monotonic_ns=N realtime_ns=N dev=NAME part_start=N fs_block=N
# Rows (end is exclusive):
#   <start_sector> <end_sector> <path-relative-to-dir>
#
# Run this about every 30 s during a bench so new index files are captured.
# Deleted files are not listed. blk-report.py keeps their last mapping until
# a later snapshot reuses the sectors.

set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: extent-snap.sh <mountpoint> <dir> <out>" >&2
    exit 1
fi

mountpoint=$1
dir=$2
out=$3

if [ ! -d "$mountpoint" ]; then
    echo "mountpoint is not a directory: $mountpoint" >&2
    exit 1
fi
if [ ! -d "$dir" ]; then
    echo "dir is not a directory: $dir" >&2
    exit 1
fi

dir=$(CDPATH= cd -- "$dir" && pwd)
mkdir -p "$out"

if command -v findmnt >/dev/null 2>&1; then
    src=$(findmnt -n -o SOURCE -T "$mountpoint" || true)
else
    src=$(awk -v p="$mountpoint" '
        BEGIN { best = 0; src = "" }
        {
            mp = $2
            gsub(/\\040/, " ", mp)
            n = length(mp)
            if (index(p "/", mp "/") == 1 && n > best) {
                best = n
                src = $1
            }
        }
        END { print src }
    ' /proc/mounts)
fi

if [ -z "${src:-}" ]; then
    echo "no mount source for $mountpoint" >&2
    exit 1
fi

# findmnt appends a btrfs/overlay subvolume in brackets.
src=${src%%[*}
if [ ! -e "$src" ]; then
    echo "mount source is not a device node: $src" >&2
    exit 1
fi
src=$(readlink -f "$src") || {
    echo "cannot resolve mount source $src" >&2
    exit 1
}

maj_hex=$(stat -c '%t' "$src")
min_hex=$(stat -c '%T' "$src")
sysfs="/sys/dev/block/$((0x$maj_hex)):$((0x$min_hex))"
if [ ! -e "$sysfs" ]; then
    echo "no sysfs node for $src ($sysfs)" >&2
    exit 1
fi
name=$(basename "$(readlink -f "$sysfs")")

part_start=0
if [ -r "$sysfs/start" ]; then
    part_start=$(tr -d '[:space:]' < "$sysfs/start")
fi
case "$part_start" in
    '' | *[!0-9]*)
        echo "partition start is not an integer: $part_start" >&2
        exit 1
        ;;
esac

fs_block=$(stat -f -c '%S' "$dir")

clocks=$(python3 -c 'import time; print(time.clock_gettime_ns(time.CLOCK_MONOTONIC), time.clock_gettime_ns(time.CLOCK_REALTIME))')
mono=${clocks%% *}
real=${clocks#* }

list=$(mktemp)
body=$(mktemp)
trap 'rm -f "$list" "$body"' EXIT

find "$dir" -xdev -type f -print0 >"$list"

python3 - "$dir" "$fs_block" "$part_start" "$list" >"$body" <<'PY'
import os
import re
import subprocess
import sys

root, fs_block_s, part_s, list_path = sys.argv[1:5]
fs_block = int(fs_block_s)
part_start = int(part_s)
default_scale = fs_block // 512 if fs_block >= 512 and fs_block % 512 == 0 else 1

frag_re = re.compile(
    r"^\s*\d+:\s+\d+\.\.\s*\d+:\s+(\d+)\.\.\s*(\d+):\s+\d+:"
)
block_re = re.compile(r"blocks of (\d+) bytes")
# xfs_bmap -v: "  0: [0..7]: 192..199 ..." — 512-byte sectors, inclusive.
bmap_re = re.compile(r"^\s*\d+:\s+\[[\d.]+\.\.[\d.]+\]:\s+(\d+)\.\.(\d+)\b")


def run(cmd):
    try:
        proc = subprocess.run(
            cmd, check=False, capture_output=True, text=True, timeout=60
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as error:
        print(f"{cmd[0]}: {error}", file=sys.stderr)
        return ""
    return proc.stdout + "\n" + proc.stderr


def from_filefrag(text):
    scale = default_scale
    found = block_re.search(text)
    if found:
        block = int(found.group(1))
        if block < 512 or block % 512 != 0:
            return []
        scale = block // 512
    rows = []
    for line in text.splitlines():
        match = frag_re.match(line)
        if match is None:
            continue
        first = int(match.group(1))
        last = int(match.group(2))
        if last < first:
            continue
        rows.append((first * scale + part_start, (last + 1) * scale + part_start))
    return rows


def from_bmap(text):
    rows = []
    for line in text.splitlines():
        match = bmap_re.match(line)
        if match is None:
            continue
        first = int(match.group(1))
        last = int(match.group(2))
        if last < first:
            continue
        rows.append((first + part_start, last + 1 + part_start))
    return rows


with open(list_path, "rb") as handle:
    names = handle.read().split(b"\0")

for item in names:
    if not item:
        continue
    path = item.decode("utf-8", "surrogateescape")
    rel = os.path.relpath(path, root)
    rows = from_filefrag(run(["filefrag", "-v", path]))
    if not rows:
        rows = from_bmap(run(["xfs_bmap", "-v", path]))
    if not rows:
        try:
            size = os.path.getsize(path)
        except OSError:
            size = 0
        if size > 0:
            print(f"no extents: {rel}", file=sys.stderr)
        continue
    for start, end in rows:
        print(f"{start} {end} {rel}")
PY

dest="$out/extents-$mono.txt"
{
    printf '# monotonic_ns=%s realtime_ns=%s dev=%s part_start=%s fs_block=%s\n' \
        "$mono" "$real" "$name" "$part_start" "$fs_block"
    cat "$body"
} >"$dest"

echo "$dest"
