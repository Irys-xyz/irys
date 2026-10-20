#!/usr/bin/env python3
"""Copy a short prefix of a blk-run directory for a blk-report diff.

The slice keeps the first N issued trace lines (and the completes in that
prefix), plus the extent snapshots that bracket those event times. device.txt,
steps.jsonl, and busy.log are copied whole when present. Do not commit the
slice.

    python3 mk-blk-slice.py SRC DEST --issues 20000
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path


def snapshot_time(path: Path) -> int | None:
    with path.open(encoding="utf-8", errors="replace") as handle:
        for raw in handle:
            line = raw.strip()
            if not line:
                continue
            if not line.startswith("#"):
                return None
            for token in line[1:].split():
                if token.startswith("monotonic_ns="):
                    try:
                        return int(token.split("=", 1)[1])
                    except ValueError:
                        return None
            return None
    return None


def write_trace_prefix(src: Path, dest: Path, issues: int) -> tuple[int, int, int]:
    """Write the trace through the Nth issue. Return (issued, first_ns, last_ns)."""
    issued = 0
    first = None
    last = None
    with src.open(encoding="utf-8", errors="replace") as inp, dest.open(
        "w", encoding="utf-8"
    ) as out:
        for raw in inp:
            out.write(raw)
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) < 2 or parts[1] not in ("I", "C"):
                continue
            try:
                mono = int(parts[0])
            except ValueError:
                continue
            if first is None:
                first = mono
            last = mono
            if parts[1] == "I":
                issued += 1
                if issued >= issues:
                    break
    if first is None or last is None:
        raise SystemExit(f"no trace events in {src}")
    return issued, first, last


def bracket_snapshots(src: Path, first: int, last: int) -> list[Path]:
    """Snapshots a nearest-map lookup can hit for an event in [first, last]."""
    timed = []
    for path in src.glob("extents-*.txt"):
        when = snapshot_time(path)
        if when is None:
            continue
        timed.append((when, path.name, path))
    timed.sort(key=lambda item: (item[0], item[1]))
    if not timed:
        return []
    low = 0
    for index, (when, _name, _path) in enumerate(timed):
        if when <= first:
            low = index
        else:
            break
    high = len(timed) - 1
    for index, (when, _name, _path) in enumerate(timed):
        if when >= last:
            high = index
            break
    return [item[2] for item in timed[low : high + 1]]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Copy a blk-report input prefix.")
    parser.add_argument("src", type=Path)
    parser.add_argument("dest", type=Path)
    parser.add_argument("--issues", type=int, default=20_000)
    args = parser.parse_args(argv)
    if args.issues < 1:
        print("--issues must be at least 1", file=sys.stderr)
        return 2
    src = args.src
    trace = src / "trace.log"
    if not trace.is_file():
        print(f"trace not found: {trace}", file=sys.stderr)
        return 2
    dest = args.dest
    try:
        dest.mkdir()
    except FileExistsError:
        print(f"dest exists: {dest}", file=sys.stderr)
        return 2
    issued, first, last = write_trace_prefix(trace, dest / "trace.log", args.issues)
    for name in ("device.txt", "steps.jsonl", "busy.log"):
        path = src / name
        if path.is_file():
            shutil.copyfile(path, dest / name)
    snaps = bracket_snapshots(src, first, last)
    for path in snaps:
        shutil.copyfile(path, dest / path.name)
    print(
        f"issued {issued}  mono {first}..{last}  snapshots {len(snaps)}  -> {dest}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
