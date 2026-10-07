#!/usr/bin/env python3
"""Per-window disk time from `iostat -dmx`.

Busy time is `%util` (percent of a second, times 10, in milliseconds).
Transfer time is bytes divided by a zone rate. The remainder is positioning:
seek, rotation, command overhead, and flush wait.

The first sample for the device is the since-boot average and is dropped.
Columns match `iostat -dmx` with the flush field at index 19 and `%util` last.
Recall bytes use `--recall-rate`. Other reads and all writes use `--region-rate`.

usage:
  disktime.py IOSTAT.log --device DEV [--window NAME:START:END ...]
      [--recall-rate MIB/S] [--region-rate MIB/S] [--recall-mib MIB]
  disktime.py IOSTAT.log --device DEV --scan
"""

from __future__ import annotations

import sys

# iostat -dmx field indexes. %util is the last field, not a fixed index.
FIELD_INDEX = {
    "rs": 1,
    "rmb": 2,
    "r_await": 5,
    "rsz": 6,
    "ws": 7,
    "wmb": 8,
    "w_await": 11,
    "wsz": 12,
    "fs": 19,
}

DEFAULT_RECALL_RATE = 200.0
DEFAULT_REGION_RATE = 146.0
DEFAULT_RECALL_MIB = 100.0


class IostatFormatError(ValueError):
    pass


def parse_iostat(lines, device):
    """Device rows after the since-boot average. `lines` are text lines."""
    rows = []
    for line in lines:
        parts = line.split()
        if not parts or parts[0] != device:
            continue
        if len(parts) <= FIELD_INDEX["fs"]:
            raise IostatFormatError(
                f"{device} line has {len(parts)} fields; iostat -dmx with a flush column is required"
            )
        row = {}
        for name, index in FIELD_INDEX.items():
            row[name] = float(parts[index])
        row["util"] = float(parts[-1])
        rows.append(row)
    if rows:
        rows = rows[1:]
    return rows


def load_iostat(path, device):
    with open(path, encoding="utf-8") as handle:
        return parse_iostat(handle, device)


def _mean(rows, key):
    return sum(row[key] for row in rows) / len(rows)


def window_time(rows, recall_rate, region_rate, recall_mib):
    """Milliseconds per second for one window of iostat rows.

    `recall_rate` and `region_rate` are MiB/s. `recall_mib` is the recall
    traffic assumed in each second, capped by the measured read rate.
    """
    if not rows:
        raise ValueError("window is empty")
    if recall_rate <= 0 or region_rate <= 0:
        raise ValueError("zone rates must be greater than 0")
    avg = {key: _mean(rows, key) for key in rows[0]}
    busy_ms = avg["util"] * 10
    recall_mib_used = min(recall_mib, avg["rmb"])
    recall_ms = recall_mib_used / recall_rate * 1000
    other_read_mib = max(0.0, avg["rmb"] - recall_mib)
    other_read_ms = other_read_mib / region_rate * 1000
    write_ms = avg["wmb"] / region_rate * 1000
    positioning_ms = busy_ms - recall_ms - other_read_ms - write_ms
    return {
        "n": len(rows),
        "busy_ms": busy_ms,
        "recall_ms": recall_ms,
        "recall_mib": recall_mib_used,
        "other_read_ms": other_read_ms,
        "other_read_mib": other_read_mib,
        "write_ms": write_ms,
        "write_mib": avg["wmb"],
        "positioning_ms": positioning_ms,
        "idle_ms": 1000 - busy_ms,
        "rs": avg["rs"],
        "rsz": avg["rsz"],
        "ws": avg["ws"],
        "wsz": avg["wsz"],
        "fs": avg["fs"],
        "rmb": avg["rmb"],
        "util": avg["util"],
    }


def format_window(name, stats):
    return (
        f"{name:10s} n={stats['n']:3d}s busy={stats['busy_ms']:5.0f}ms"
        f" | recall xfer {stats['recall_ms']:4.0f}ms ({stats['recall_mib']:.0f} MiB)"
        f" | entropy/index read xfer {stats['other_read_ms']:4.0f}ms ({stats['other_read_mib']:.1f} MiB)"
        f" | write xfer {stats['write_ms']:4.0f}ms ({stats['write_mib']:.1f} MiB)"
        f" | positioning+other {stats['positioning_ms']:4.0f}ms"
        f" | idle {stats['idle_ms']:4.0f}ms"
        f" || r/s {stats['rs']:.0f} (avg {stats['rsz']:.0f} KiB)"
        f" w/s {stats['ws']:.0f} (avg {stats['wsz']:.0f} KiB)"
        f" flush/s {stats['fs']:.1f}"
    )


def format_scan(rows, bucket=60):
    lines = []
    for start in range(0, len(rows), bucket):
        window = rows[start : start + bucket]
        write_mib = _mean(window, "wmb")
        read_mib = _mean(window, "rmb")
        util = _mean(window, "util")
        lines.append(
            f"t={start:5d}s wMB/s={write_mib:5.1f} rMB/s={read_mib:6.1f} util={util:5.1f}"
        )
    return lines


def parse_window(text):
    name, start, end = text.split(":")
    return name, int(start), int(end)


def main(argv):
    args = list(argv[1:])
    if not args or args[0].startswith("-"):
        print(
            "usage: disktime.py IOSTAT.log --device DEV [--window NAME:START:END ...] "
            "[--recall-rate N] [--region-rate N] [--recall-mib N] [--scan]",
            file=sys.stderr,
        )
        return 2
    path = args[0]
    device = None
    windows = []
    recall_rate = DEFAULT_RECALL_RATE
    region_rate = DEFAULT_REGION_RATE
    recall_mib = DEFAULT_RECALL_MIB
    scan = False
    index = 1
    while index < len(args):
        flag = args[index]
        if flag == "--scan":
            scan = True
            index += 1
            continue
        if index + 1 >= len(args):
            print(f"missing value for {flag}", file=sys.stderr)
            return 2
        value = args[index + 1]
        if flag == "--device":
            device = value
        elif flag == "--window":
            windows.append(parse_window(value))
        elif flag == "--recall-rate":
            recall_rate = float(value)
        elif flag == "--region-rate":
            region_rate = float(value)
        elif flag == "--recall-mib":
            recall_mib = float(value)
        else:
            print(f"unknown argument: {flag}", file=sys.stderr)
            return 2
        index += 2
    if not device:
        print("--device is required", file=sys.stderr)
        return 2
    rows = load_iostat(path, device)
    if scan:
        for line in format_scan(rows):
            print(line)
        return 0
    if not windows:
        print("pass --window NAME:START:END or --scan", file=sys.stderr)
        return 2
    for name, start, end in windows:
        print(format_window(name, window_time(rows[start:end], recall_rate, region_rate, recall_mib)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
