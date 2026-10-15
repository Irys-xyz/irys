#!/usr/bin/env python3
"""Summarise `perf script` output for an irys node.

Each sample is attributed to its innermost frame from an irys crate, so time
or page faults inside std, tokio, rocksdb or btree code are charged to the
irys function that called them. Prints the top functions and, with
--folded FILE, writes folded stacks for inferno-flamegraph / flamegraph.pl.

    sudo perf script -i cpu.data | summarize_perf.py [--top 25] [--folded out.folded]
"""
import argparse
import collections
import re
import sys

HASH = re.compile(r"(::)?h[0-9a-f]{16}$|17h[0-9a-f]{16}E$")
OFFSET = re.compile(r"\+0x[0-9a-f]+$")
IRYS = re.compile(r"irys_[a-z_]+")


def clean(symbol: str) -> str:
    symbol = OFFSET.sub("", symbol)
    return HASH.sub("", symbol)


def samples(stream):
    frames = []
    for line in stream:
        line = line.rstrip("\n")
        if not line.strip():
            if frames:
                yield frames
            frames = []
            continue
        if line[:1].isspace():
            parts = line.split(None, 2)
            if len(parts) >= 2:
                frames.append(clean(parts[1]))
    if frames:
        yield frames


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--folded")
    args = ap.parse_args()
    owners = collections.Counter()
    folded = collections.Counter()
    total = 0
    for frames in samples(sys.stdin):
        total += 1
        owner = next((f for f in frames if IRYS.search(f)), frames[0] if frames else "?")
        owners[owner] += 1
        if args.folded:
            folded[";".join(reversed(frames))] += 1
    print(f"samples: {total}")
    for symbol, count in owners.most_common(args.top):
        print(f"{100.0 * count / max(total, 1):6.2f}% {count:8d}  {symbol}")
    if args.folded:
        with open(args.folded, "w") as out:
            for stack, count in folded.items():
                out.write(f"{stack} {count}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
