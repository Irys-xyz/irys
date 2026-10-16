#!/usr/bin/env python3
"""Per-minute breakdown of one irys-sm-bench samples.jsonl.

A step can meet its mean and still fall after the buffers fill. The minute
lines show that. Minute 1 is sample seconds (0, 60], minute 2 is (60, 120],
matching the bench's 1-based `sec` field.

usage: minutes.py SAMPLES.jsonl [STEP ...] [--minute-secs N]
"""

from __future__ import annotations

import json
import sys

if __package__:
    from analysis.ladder import load_samples, step_names
else:
    from ladder import load_samples, step_names


def minute_bounds(minute, minute_secs):
    """`minute` is 0-based. Returns the exclusive-inclusive (lo, hi] window."""
    return minute * minute_secs, (minute + 1) * minute_secs


def minute_stats(rows, step, minute, minute_secs=60):
    """None when the minute has no ingest samples."""
    lo, hi = minute_bounds(minute, minute_secs)
    ingest = [
        row
        for row in rows
        if row["step"] == step
        and row["driver"] == "ingest"
        and lo < row["sec"] <= hi
    ]
    if not ingest:
        return None
    recall = [
        row
        for row in rows
        if row["step"] == step
        and row["driver"] == "recall"
        and row["ops"] > 0
        and lo < row["sec"] <= hi
    ]
    enqueued = sum(row["enqueued"] for row in ingest) / len(ingest) / 2**20
    durable = sum(row["durable"] for row in ingest) / len(ingest) / 2**20
    start_max_ms = max((row["start_delay_max_us"] for row in recall), default=0) / 1000
    latency_max_ms = max((row["lat_max_us"] for row in recall), default=0) / 1000
    pending_max = max(row["pending_chunks"] for row in ingest)
    return {
        "minute": minute + 1,
        "secs": len(ingest),
        "enq_mib_s": enqueued,
        "dur_mib_s": durable,
        "recall_max_ms": latency_max_ms,
        "start_max_ms": start_max_ms,
        "pending_max": pending_max,
    }


def format_minute(stats):
    return (
        f"  min{stats['minute']:2d}"
        f" enq={stats['enq_mib_s']:5.1f} dur={stats['dur_mib_s']:5.1f}"
        f" recall_max={stats['recall_max_ms']:5.0f}ms"
        f" start_max={stats['start_max_ms']:5.0f}ms"
        f" pending_max={stats['pending_max']}"
    )


def iter_minutes(rows, step, minute_secs):
    """Yield every minute through the last ingest sample of `step`.

    An empty minute is `{"minute": n, "gap": True}`. Stopping at the first
    gap would hide every later minute of the step.
    """
    last_sec = 0
    for row in rows:
        if row.get("step") != step or row.get("driver") != "ingest":
            continue
        sec = row["sec"]
        if sec > last_sec:
            last_sec = sec
    if last_sec <= 0:
        return
    last_minute = (last_sec - 1) // minute_secs
    for minute in range(last_minute + 1):
        stats = minute_stats(rows, step, minute, minute_secs)
        if stats is None:
            yield {"minute": minute + 1, "gap": True}
        else:
            yield stats


def main(argv):
    args = list(argv[1:])
    minute_secs = 60
    if "--minute-secs" in args:
        index = args.index("--minute-secs")
        try:
            minute_secs = int(args[index + 1])
        except (IndexError, ValueError):
            print("usage: minutes.py SAMPLES.jsonl [STEP ...] [--minute-secs N]", file=sys.stderr)
            return 2
        if minute_secs <= 0:
            print("--minute-secs must be greater than 0", file=sys.stderr)
            return 2
        del args[index : index + 2]
    if not args:
        print("usage: minutes.py SAMPLES.jsonl [STEP ...] [--minute-secs N]", file=sys.stderr)
        return 2
    rows = load_samples(args[0])
    steps = args[1:] or step_names(rows)
    for step in steps:
        print("==", args[0], step)
        any_row = False
        for stats in iter_minutes(rows, step, minute_secs):
            any_row = True
            if stats.get("gap"):
                print(f"  min{stats['minute']:2d} (no samples)")
            else:
                print(format_minute(stats))
        if not any_row:
            print("  (no samples)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
