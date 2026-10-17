#!/usr/bin/env python3
"""Per-step summary of an irys-sm-bench samples.jsonl.

Recall percentiles are percentiles of the per-second maxima. The bench table
uses every recall sample, so the two p99 numbers can differ.

usage: ladder.py SAMPLES.jsonl [SAMPLES.jsonl ...]
"""

from __future__ import annotations

import json
import sys


def percentile(values, fraction):
    """Floor-rank percentile. `fraction` is in [0, 1]. Empty input is 0."""
    if not values:
        return 0
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(len(ordered) * fraction))
    return ordered[index]


def load_samples(path):
    rows = []
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


def step_names(rows):
    names = []
    for row in rows:
        name = row["step"]
        if name not in names:
            names.append(name)
    return names


def summarize_step(rows, step):
    """One step. Rates are mean bytes per ingest sample, in MiB/s."""
    recall = [
        row
        for row in rows
        if row["step"] == step and row["driver"] == "recall" and row["ops"] > 0
    ]
    ingest = [row for row in rows if row["step"] == step and row["driver"] == "ingest"]
    seconds = len(ingest)
    divisor = seconds or 1
    enqueued = sum(row["enqueued"] for row in ingest) / divisor / 2**20
    durable = sum(row["durable"] for row in ingest) / divisor / 2**20
    skips = sum(
        row["skips"] for row in rows if row["step"] == step and row["driver"] == "recall"
    )
    latency_ms = [row["lat_max_us"] / 1000 for row in recall]
    start_ms = [row["start_delay_max_us"] / 1000 for row in recall]
    limits = {}
    for row in ingest:
        phases = row.get("phases") or {}
        label = phases.get("limit")
        limits[label] = limits.get(label, 0) + 1
    return {
        "step": step,
        "secs": seconds,
        "enq_mib_s": enqueued,
        "dur_mib_s": durable,
        "skips": skips,
        "recall_p99_ms": percentile(latency_ms, 0.99),
        "start_p99_ms": percentile(start_ms, 0.99),
        "limit": limits,
    }


def format_step(summary):
    return (
        f"  {summary['step']:<10} secs={summary['secs']:3d}"
        f" enq={summary['enq_mib_s']:5.1f} dur={summary['dur_mib_s']:5.1f} MiB/s"
        f" skips={summary['skips']}"
        f" recall_p99={summary['recall_p99_ms']:.0f}ms"
        f" start_p99={summary['start_p99_ms']:.0f}ms"
        f" limit={summary['limit']}"
    )


def format_run(path, rows):
    lines = [f"=== {path}"]
    for step in step_names(rows):
        lines.append(format_step(summarize_step(rows, step)))
    return "\n".join(lines)


def main(argv):
    if len(argv) < 2:
        print("usage: ladder.py SAMPLES.jsonl [SAMPLES.jsonl ...]", file=sys.stderr)
        return 2
    exit_code = 0
    for path in argv[1:]:
        try:
            rows = load_samples(path)
        except FileNotFoundError:
            print(f"missing {path}", file=sys.stderr)
            exit_code = 1
            continue
        print(format_run(path, rows))
    return exit_code


if __name__ == "__main__":
    sys.exit(main(sys.argv))
