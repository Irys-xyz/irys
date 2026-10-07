#!/usr/bin/env python3
"""Split a blk-run.sh trace into per-file disk time.

Zone rate is linear in LBA fraction: 266 MiB/s at the first sector, 130 MiB/s
at the last. Sections 103 and 104 of the disk-lane notebook measured about
266 MiB/s on the outer tracks of this HDD. The write region in allocation
group 18 (groups 0-20) filled at about 146 MiB/s, which is this line there.
Override with --zone-outer and --zone-inner.

Service time is complete - max(issue, previous completion), in completion
order. The actuator is serial, so the sum is busy time. Positioning is
service minus bytes / zone_rate(start LBA). A zero-length flush does not
move the seek cursor. Its positioning time is the whole service time: the
drive is draining its write cache.
"""

from __future__ import annotations

import argparse
import bisect
import csv
import json
import sys
from pathlib import Path

OUTER_MIB_S = 266.0
INNER_MIB_S = 130.0
SECTOR_BYTES = 512
GIB = 1024**3
MIB = 1024**2

CATEGORIES = (
    "recall",
    "entropy_read",
    "packed_write",
    "index_wal",
    "index_sst",
    "index_blob",
    "index_other",
    "flush",
    "metadata",
)

ZONE_NOTE = (
    "Linear by LBA fraction, 266 MiB/s at the first sector to 130 MiB/s "
    "at the last. Notebook sections 103 and 104: outer sequential fill "
    "about 266 MiB/s; the AG 18 write region filled at about 146 MiB/s."
)


class Extent:
    def __init__(self, start: int, end: int, path: str) -> None:
        self.start = start
        self.end = end
        self.path = path


class Issued:
    def __init__(self, mono: int, sector: int, nr: int, rwbs: str, comm: str, pid: int) -> None:
        self.mono = mono
        self.sector = sector
        self.nr = nr
        self.rwbs = rwbs
        self.comm = comm
        self.pid = pid

    def clone(self, sector: int | None = None, nr: int | None = None) -> Issued:
        return Issued(
            self.mono,
            self.sector if sector is None else sector,
            self.nr if nr is None else nr,
            self.rwbs,
            self.comm,
            self.pid,
        )


class Request:
    def __init__(self, issued: Issued, complete_ns: int, seq: int) -> None:
        self.issue_ns = issued.mono
        self.complete_ns = complete_ns
        self.sector = issued.sector
        self.nr = issued.nr
        self.rwbs = issued.rwbs
        self.comm = issued.comm
        self.pid = issued.pid
        self.seq = seq
        self.category = "metadata"
        self.service_ns = 0
        self.transfer_ns = 0
        self.positioning_ns = 0
        self.seek_sectors: int | None = None
        self.prev_category: str | None = None
        self.clamped = False


def label(category: str) -> str:
    if category == "metadata":
        return "metadata/other"
    return category


def op_letter(rwbs: str) -> str:
    for char in rwbs.upper():
        if char in "RWD":
            return char
    return ""


def same_op(left: str, right: str) -> bool:
    a = op_letter(left)
    b = op_letter(right)
    return a == "" or b == "" or a == b


def category_for_path(path: str, rwbs: str) -> str:
    name = path.rsplit("/", 1)[-1]
    flags = rwbs.upper()
    if name == "entropy-chunks.dat":
        if "R" in flags and "W" not in flags:
            return "recall"
        return "metadata"
    if name == "write-region.dat":
        if "W" in flags:
            return "packed_write"
        if "R" in flags:
            return "entropy_read"
        return "metadata"
    if name.endswith(".log") and name != "LOG" and not name.startswith("LOG."):
        return "index_wal"
    if name.endswith(".sst"):
        return "index_sst"
    if name.endswith(".blob"):
        return "index_blob"
    if (
        name.startswith("MANIFEST")
        or name.startswith("OPTIONS")
        or name.startswith("LOG.")
        or name in {"LOG", "CURRENT", "IDENTITY", "mdbx.dat", "mdbx.lck"}
    ):
        return "index_other"
    return "metadata"


def parse_trace(text: str) -> tuple[list[tuple], int | None, int]:
    """Return (events, dev_t, skipped lines).

    An event is (kind, mono, sector, nr, rwbs, comm, pid).
    """
    events = []
    dev_t = None
    skipped = 0
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if line.startswith("#"):
            for token in line[1:].split():
                if token.startswith("dev_t="):
                    try:
                        dev_t = int(token.split("=", 1)[1])
                    except ValueError:
                        pass
            continue
        parts = line.split()
        if len(parts) < 5 or parts[1] not in ("I", "C"):
            skipped += 1
            continue
        try:
            mono = int(parts[0])
            sector = int(parts[2])
            nr = int(parts[3])
        except ValueError:
            skipped += 1
            continue
        rwbs = parts[4]
        comm = ""
        pid = 0
        if parts[1] == "I" and len(parts) >= 7:
            comm = parts[5]
            try:
                pid = int(parts[6])
            except ValueError:
                pid = 0
        events.append((parts[1], mono, sector, nr, rwbs, comm, pid))
    return events, dev_t, skipped


def pair_requests(events: list[tuple]) -> tuple[list[Request], int, int]:
    """Match issue to complete. Merges grow an outstanding request.

    A completion may be a prefix or a middle slice of the issued span.
    Zero-length requests (flushes) match FIFO at the same sector.
    """
    outstanding: list[Issued] = []
    done: list[Request] = []
    unmatched_completes = 0
    seq = 0
    for kind, mono, sector, nr, rwbs, comm, pid in events:
        if kind == "I":
            fresh = Issued(mono, sector, nr, rwbs, comm, pid)
            replaced = False
            if nr > 0:
                for index, old in enumerate(outstanding):
                    if (
                        old.sector == sector
                        and old.rwbs == rwbs
                        and nr > old.nr
                        and old.nr > 0
                    ):
                        outstanding[index] = fresh
                        replaced = True
                        break
            if not replaced:
                outstanding.append(fresh)
            continue
        matched: Issued | None = None
        if nr == 0:
            for index, old in enumerate(outstanding):
                if old.nr == 0 and old.sector == sector and same_op(old.rwbs, rwbs):
                    matched = outstanding.pop(index)
                    break
        else:
            for index, old in enumerate(outstanding):
                if old.nr == nr and old.sector == sector and old.nr > 0 and same_op(old.rwbs, rwbs):
                    matched = outstanding.pop(index)
                    break
            if matched is None:
                for index, old in enumerate(outstanding):
                    if old.nr <= 0 or not same_op(old.rwbs, rwbs):
                        continue
                    end = sector + nr
                    old_end = old.sector + old.nr
                    if old.sector <= sector and end <= old_end:
                        matched = old.clone(sector=sector, nr=nr)
                        pieces: list[Issued] = []
                        if old.sector < sector:
                            pieces.append(old.clone(sector=old.sector, nr=sector - old.sector))
                        if end < old_end:
                            pieces.append(old.clone(sector=end, nr=old_end - end))
                        outstanding.pop(index)
                        for offset, piece in enumerate(pieces):
                            outstanding.insert(index + offset, piece)
                        break
        if matched is None:
            unmatched_completes += 1
            continue
        done.append(Request(matched, mono, seq))
        seq += 1
    return done, len(outstanding), unmatched_completes


def subtract(extents: list[Extent], start: int, end: int) -> list[Extent]:
    out: list[Extent] = []
    for ext in extents:
        if ext.end <= start or ext.start >= end:
            out.append(Extent(ext.start, ext.end, ext.path))
            continue
        if ext.start < start:
            out.append(Extent(ext.start, start, ext.path))
        if ext.end > end:
            out.append(Extent(end, ext.end, ext.path))
    return out


def coalesce(extents: list[Extent]) -> list[Extent]:
    if not extents:
        return []
    ordered = sorted(extents, key=lambda ext: (ext.start, ext.end, ext.path))
    out = [ordered[0]]
    for ext in ordered[1:]:
        last = out[-1]
        if ext.path == last.path and ext.start <= last.end:
            if ext.end > last.end:
                last.end = ext.end
        else:
            out.append(ext)
    return out


def evolve(prev: list[Extent], rows: list[tuple[int, int, str]]) -> list[Extent]:
    """Files present in this snapshot replace their old extents.

    A file that is absent keeps its last extents until a present file
    takes those sectors.
    """
    present = {path for _, _, path in rows}
    kept = [Extent(ext.start, ext.end, ext.path) for ext in prev if ext.path not in present]
    for start, end, path in rows:
        if end <= start:
            continue
        kept = subtract(kept, start, end)
        kept.append(Extent(start, end, path))
    return coalesce(kept)


def build_maps(snapshots: list[tuple[int, list[tuple[int, int, str]]]]) -> list[tuple[int, list[Extent]]]:
    current: list[Extent] = []
    maps = []
    for when, rows in snapshots:
        current = evolve(current, rows)
        maps.append((when, current))
    return maps


def nearest_map(maps: list[tuple[int, list[Extent]]], when: int) -> list[Extent]:
    if not maps:
        return []
    times = [item[0] for item in maps]
    index = bisect.bisect_left(times, when)
    if index <= 0:
        return maps[0][1]
    if index >= len(times):
        return maps[-1][1]
    if when - times[index - 1] <= times[index] - when:
        return maps[index - 1][1]
    return maps[index][1]


def overlap_counts(extents: list[Extent], start: int, end: int) -> dict[str, int]:
    counts: dict[str, int] = {}
    starts = [ext.start for ext in extents]
    index = bisect.bisect_right(starts, start) - 1
    if index < 0:
        index = 0
    while index < len(extents) and extents[index].start < end:
        ext = extents[index]
        lo = start if start > ext.start else ext.start
        hi = end if end < ext.end else ext.end
        if hi > lo:
            counts[ext.path] = counts.get(ext.path, 0) + (hi - lo)
        index += 1
    return counts


def classify(req: Request, extents: list[Extent]) -> str:
    flags = req.rwbs.upper()
    if req.nr == 0 and "F" in flags:
        return "flush"
    if "D" in flags and "R" not in flags and "W" not in flags:
        return "metadata"
    if req.nr <= 0:
        return "metadata"
    counts = overlap_counts(extents, req.sector, req.sector + req.nr)
    if not counts:
        return "metadata"
    path = max(counts, key=lambda item: counts[item])
    return category_for_path(path, req.rwbs)


def rate_bytes_per_s(sector: int, device_sectors: int, outer: float, inner: float) -> int:
    if device_sectors <= 0:
        fraction = 0.0
    else:
        fraction = sector / float(device_sectors)
        if fraction < 0.0:
            fraction = 0.0
        elif fraction > 1.0:
            fraction = 1.0
    mib = outer + (inner - outer) * fraction
    return int(round(mib * MIB))


def transfer_ns(nbytes: int, bytes_per_s: int) -> int:
    if nbytes <= 0 or bytes_per_s <= 0:
        return 0
    return (nbytes * 1_000_000_000) // bytes_per_s


def attribute(
    requests: list[Request], device_sectors: int, outer: float, inner: float
) -> None:
    ordered = sorted(requests, key=lambda req: (req.complete_ns, req.seq))
    prev_complete: int | None = None
    prev_end: int | None = None
    prev_category: str | None = None
    for req in ordered:
        if prev_complete is None:
            service = req.complete_ns - req.issue_ns
        else:
            anchor = req.issue_ns if req.issue_ns > prev_complete else prev_complete
            service = req.complete_ns - anchor
        if service < 0:
            service = 0
        nbytes = req.nr * SECTOR_BYTES
        rate = rate_bytes_per_s(req.sector, device_sectors, outer, inner)
        xfer = transfer_ns(nbytes, rate)
        if xfer > service:
            req.clamped = True
            positioning = 0
        else:
            positioning = service - xfer
        seek = None
        if req.nr > 0 and prev_end is not None:
            seek = abs(req.sector - prev_end)
        req.service_ns = service
        req.transfer_ns = xfer
        req.positioning_ns = positioning
        req.seek_sectors = seek
        req.prev_category = prev_category
        prev_complete = req.complete_ns
        prev_category = req.category
        if req.nr > 0:
            prev_end = req.sector + req.nr


def percentile(values: list[int], pct: int) -> int | None:
    """Nearest rank. rank = ceil(pct/100 * n) - 1. Same rule as the bench."""
    if not values or pct <= 0:
        return None
    ordered = sorted(values)
    n = len(ordered)
    rank = (pct * n + 99) // 100 - 1
    if rank < 0:
        rank = 0
    if rank >= n:
        rank = n - 1
    return ordered[rank]


def to_gib(sectors: int | float | None) -> float | None:
    if sectors is None:
        return None
    return float(sectors) * SECTOR_BYTES / GIB


def parse_steps(text: str) -> list[dict]:
    steps: dict[int, dict] = {}
    order: list[int] = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        obj = json.loads(line)
        if obj.get("kind") == "run":
            continue
        if obj.get("kind") != "step":
            continue
        index = int(obj["index"])
        if index not in steps:
            steps[index] = {
                "index": index,
                "name": obj.get("name", str(index)),
            }
            order.append(index)
        phase = obj["phase"]
        steps[index][phase] = int(obj["monotonic_ns"])
        steps[index][phase + "_realtime"] = int(obj["realtime_ns"])
    out = []
    for index in order:
        step = steps[index]
        if "start" not in step or "end" not in step:
            continue
        if step["end"] < step["start"]:
            continue
        out.append(step)
    return out


def parse_extent_file(text: str) -> tuple[int | None, list[tuple[int, int, str]]]:
    when = None
    rows = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if line.startswith("#"):
            for token in line[1:].split():
                if token.startswith("monotonic_ns="):
                    try:
                        when = int(token.split("=", 1)[1])
                    except ValueError:
                        pass
            continue
        parts = line.split()
        if len(parts) < 3:
            continue
        try:
            start = int(parts[0])
            end = int(parts[1])
        except ValueError:
            continue
        path = " ".join(parts[2:])
        rows.append((start, end, path))
    return when, rows


def parse_busy(text: str) -> list[tuple[int, int]]:
    rows = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) < 3:
            continue
        try:
            rows.append((int(parts[0]), int(parts[2])))
        except ValueError:
            continue
    return rows


def parse_device(text: str) -> dict[str, str]:
    info = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        info[key.strip()] = value.strip()
    return info


def busy_ms(samples: list[tuple[int, int]], start: int, end: int) -> int | None:
    """io_ticks delta using the last sample at or before each bound."""
    before_start = None
    before_end = None
    for mono, ticks in samples:
        if mono <= start:
            before_start = ticks
        if mono <= end:
            before_end = ticks
    if before_start is None or before_end is None:
        return None
    delta = before_end - before_start
    if delta < 0:
        return None
    return delta


def per_second(value: float, seconds: float) -> float:
    if seconds <= 0:
        return 0.0
    return value / seconds


def summarize_step(step: dict, requests: list[Request], samples: list[tuple[int, int]]) -> dict:
    seconds = (step["end"] - step["start"]) / 1_000_000_000
    buckets = {
        cat: {
            "requests": 0,
            "bytes": 0,
            "service_ns": 0,
            "positioning_ns": 0,
            "seeks": [],
        }
        for cat in CATEGORIES
    }
    transitions: dict[tuple[str, str], list[int]] = {}
    clamped = 0
    for req in requests:
        if not (step["start"] <= req.complete_ns < step["end"]):
            continue
        bucket = buckets[req.category]
        bucket["requests"] += 1
        bucket["bytes"] += req.nr * SECTOR_BYTES
        bucket["service_ns"] += req.service_ns
        bucket["positioning_ns"] += req.positioning_ns
        if req.seek_sectors is not None:
            bucket["seeks"].append(req.seek_sectors)
        if req.clamped:
            clamped += 1
        if req.prev_category is not None:
            key = (req.prev_category, req.category)
            cell = transitions.setdefault(key, [0, 0])
            cell[0] += 1
            cell[1] += req.positioning_ns
    service_ns = sum(bucket["service_ns"] for bucket in buckets.values())
    categories = {}
    for cat in CATEGORIES:
        bucket = buckets[cat]
        count = bucket["requests"]
        seeks = bucket["seeks"]
        mean_seek = (sum(seeks) / len(seeks)) if seeks else None
        categories[cat] = {
            "requests": count,
            "bytes": bucket["bytes"],
            "req_per_s": per_second(count, seconds),
            "mib_per_s": per_second(bucket["bytes"] / MIB, seconds),
            "mean_bytes": (bucket["bytes"] / count) if count else 0,
            "seek_mean_gib": to_gib(mean_seek),
            "seek_p50_gib": to_gib(percentile(seeks, 50)),
            "seek_p99_gib": to_gib(percentile(seeks, 99)),
            "service_ns": bucket["service_ns"],
            "positioning_ns": bucket["positioning_ns"],
            "service_ms_per_s": per_second(bucket["service_ns"] / 1_000_000, seconds),
            "positioning_ms_per_s": per_second(bucket["positioning_ns"] / 1_000_000, seconds),
        }
    trans_rows = [
        {
            "prev": prev,
            "next": nxt,
            "count": cell[0],
            "positioning_ns": cell[1],
        }
        for (prev, nxt), cell in transitions.items()
    ]
    trans_rows.sort(key=lambda row: (-row["positioning_ns"], -row["count"], row["prev"], row["next"]))
    iostat = busy_ms(samples, step["start"], step["end"])
    service_ms = service_ns / 1_000_000
    return {
        "index": step["index"],
        "name": step["name"],
        "monotonic_start_ns": step["start"],
        "monotonic_drain_ns": step.get("drain"),
        "monotonic_end_ns": step["end"],
        "realtime_start_ns": step.get("start_realtime"),
        "realtime_end_ns": step.get("end_realtime"),
        "seconds": seconds,
        "service_ns": service_ns,
        "service_ms": service_ms,
        "iostat_busy_ms": iostat,
        "busy_delta_ms": None if iostat is None else service_ms - iostat,
        "clamped_requests": clamped,
        "categories": categories,
        "transitions": trans_rows,
    }


def load_snapshots(out_dir: Path) -> list[tuple[int, list[tuple[int, int, str]]]]:
    snaps = []
    for path in sorted(out_dir.glob("extents-*.txt")):
        when, rows = parse_extent_file(path.read_text(encoding="utf-8", errors="replace"))
        if when is None:
            continue
        snaps.append((when, rows, path.name))
    snaps.sort(key=lambda item: (item[0], item[2]))
    return [(when, rows) for when, rows, _name in snaps]


def outside_step(requests: list[Request], steps: list[dict]) -> dict | None:
    if not requests:
        return None
    covered = []
    for req in requests:
        if any(step["start"] <= req.complete_ns < step["end"] for step in steps):
            continue
        covered.append(req)
    if not covered:
        return None
    start = min(req.complete_ns for req in covered)
    end = max(req.complete_ns for req in covered) + 1
    return {"index": -1, "name": "outside", "start": start, "end": end}


def build_report(
    out_dir: Path,
    steps_path: Path | None = None,
    trace_path: Path | None = None,
    zone_outer: float = OUTER_MIB_S,
    zone_inner: float = INNER_MIB_S,
    device_sectors: int | None = None,
) -> dict:
    trace_file = trace_path or (out_dir / "trace.log")
    if not trace_file.is_file():
        raise SystemExit(f"trace not found: {trace_file}")
    events, trace_dev, skipped = parse_trace(
        trace_file.read_text(encoding="utf-8", errors="replace")
    )
    requests, unmatched_issues, unmatched_completes = pair_requests(events)
    warnings = []
    if skipped:
        warnings.append(f"skipped {skipped} trace lines")
    if unmatched_issues or unmatched_completes:
        warnings.append(
            f"unmatched issues {unmatched_issues}, unmatched completes {unmatched_completes}"
        )

    device = {}
    device_file = out_dir / "device.txt"
    if device_file.is_file():
        device = parse_device(device_file.read_text(encoding="utf-8", errors="replace"))
    if device_sectors is None and "sectors" in device:
        device_sectors = int(device["sectors"])
    if device_sectors is None:
        device_sectors = 0
        warnings.append("no device sector count; zone rate stays at the outer anchor")
    if trace_dev is not None and "dev_t" in device:
        try:
            if int(device["dev_t"]) != trace_dev:
                warnings.append(
                    f"trace dev_t {trace_dev} does not match device.txt {device['dev_t']}"
                )
        except ValueError:
            pass

    snaps = load_snapshots(out_dir)
    if not snaps:
        warnings.append("no extent snapshots; file categories fall through to metadata/other")
    maps = build_maps(snaps)
    for req in requests:
        req.category = classify(req, nearest_map(maps, req.issue_ns))
    attribute(requests, device_sectors, zone_outer, zone_inner)

    step_file = steps_path
    if step_file is None:
        candidate = out_dir / "steps.jsonl"
        if candidate.is_file():
            step_file = candidate
    if step_file is None or not step_file.is_file():
        warnings.append("steps.jsonl missing; one step named all")
        if requests:
            start = min(req.issue_ns for req in requests)
            end = max(req.complete_ns for req in requests) + 1
        else:
            start, end = 0, 1
        steps = [{"index": 0, "name": "all", "start": start, "end": end}]
    else:
        steps = parse_steps(step_file.read_text(encoding="utf-8", errors="replace"))
        if not steps:
            warnings.append("steps.jsonl has no closed step")

    samples: list[tuple[int, int]] = []
    busy_file = out_dir / "busy.log"
    if busy_file.is_file():
        samples = parse_busy(busy_file.read_text(encoding="utf-8", errors="replace"))
    else:
        warnings.append("no busy.log; iostat busy delta is omitted")

    extra = outside_step(requests, steps)
    if extra is not None:
        steps = list(steps) + [extra]
        warnings.append("some completions fall outside every step; see step outside")

    rendered = [summarize_step(step, requests, samples) for step in steps]
    return {
        "zone_outer_mib_s": zone_outer,
        "zone_inner_mib_s": zone_inner,
        "zone_note": ZONE_NOTE,
        "device_sectors": device_sectors,
        "device": device.get("name"),
        "trace_dev_t": trace_dev,
        "requests": len(requests),
        "unmatched_issues": unmatched_issues,
        "unmatched_completes": unmatched_completes,
        "warnings": warnings,
        "steps": rendered,
    }


def fmt_gib(value: float | None) -> str:
    if value is None:
        return "      -"
    return f"{value:7.3f}"


def render_text(report: dict) -> str:
    lines = []
    lines.append(
        f"zone {report['zone_outer_mib_s']:g} -> {report['zone_inner_mib_s']:g} MiB/s"
        f"  device_sectors {report['device_sectors']}"
        f"  requests {report['requests']}"
    )
    lines.append(report["zone_note"])
    for warning in report["warnings"]:
        lines.append(f"warning: {warning}")
    lines.append(
        "service = complete - max(issue, previous completion), completion order."
    )
    lines.append(
        "positioning = service - bytes / zone_rate(start LBA), clamped at 0."
    )
    lines.append(
        "A flush with nr_sector 0 does not move the seek cursor."
        " Its positioning is the whole service time (write-cache drain)."
    )
    lines.append("")
    header = (
        f"{'category':<16} {'req/s':>8} {'MiB/s':>8} {'mean KiB':>10}"
        f" {'seek GiB mean/p50/p99':>26} {'svc ms/s':>10} {'pos ms/s':>10}"
    )
    for step in report["steps"]:
        drain = step.get("monotonic_drain_ns")
        drain_s = ""
        if drain is not None:
            drain_s = f"  drain {(step['monotonic_end_ns'] - drain) / 1e9:.3f}s"
        lines.append(
            f"step {step['index']} {step['name']}  {step['seconds']:.3f}s{drain_s}"
            f"  mono {step['monotonic_start_ns']}..{step['monotonic_end_ns']}"
        )
        lines.append(header)
        for cat in CATEGORIES:
            row = step["categories"][cat]
            mean_kib = row["mean_bytes"] / 1024
            seek = " / ".join(
                fmt_gib(row[key])
                for key in ("seek_mean_gib", "seek_p50_gib", "seek_p99_gib")
            )
            lines.append(
                f"{label(cat):<16} {row['req_per_s']:8.2f} {row['mib_per_s']:8.2f}"
                f" {mean_kib:10.1f} {seek:>26} {row['service_ms_per_s']:10.2f}"
                f" {row['positioning_ms_per_s']:10.2f}"
            )
        busy = step["iostat_busy_ms"]
        if busy is None:
            busy_text = "iostat busy: no sample"
            delta_text = "delta n/a"
        else:
            busy_text = f"iostat busy {busy:.1f} ms"
            delta = step["busy_delta_ms"]
            delta_text = f"delta {delta:+.1f} ms"
        lines.append(
            f"  total service {step['service_ms']:.1f} ms  {busy_text}  {delta_text}"
        )
        if step["clamped_requests"]:
            lines.append(
                f"  zone model clamped {step['clamped_requests']} requests"
                " (transfer estimate longer than service; positioning set to 0)"
            )
        lines.append("  transitions (who seeked to whom), by positioning:")
        if not step["transitions"]:
            lines.append("    (none)")
        for row in step["transitions"]:
            pos_ms = row["positioning_ns"] / 1_000_000
            lines.append(
                f"    {label(row['prev'])} -> {label(row['next'])}"
                f"  n={row['count']}  pos {pos_ms:.1f} ms"
            )
        lines.append("")
    lines.append(
        "NCQ can complete in a different order than the head moved."
        " Per-request splits are approximate. The step service total is the"
        " figure to compare with iostat."
    )
    return "\n".join(lines) + "\n"


def write_csv(report: dict, out_dir: Path) -> None:
    cat_path = out_dir / "blk-report.csv"
    with cat_path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(
            [
                "step_index",
                "step_name",
                "category",
                "requests",
                "seconds",
                "req_per_s",
                "mib_per_s",
                "mean_bytes",
                "seek_mean_gib",
                "seek_p50_gib",
                "seek_p99_gib",
                "service_ns",
                "positioning_ns",
                "service_ms_per_s",
                "positioning_ms_per_s",
            ]
        )
        for step in report["steps"]:
            for cat in CATEGORIES:
                row = step["categories"][cat]
                writer.writerow(
                    [
                        step["index"],
                        step["name"],
                        cat,
                        row["requests"],
                        f"{step['seconds']:.6f}",
                        f"{row['req_per_s']:.6f}",
                        f"{row['mib_per_s']:.6f}",
                        f"{row['mean_bytes']:.3f}",
                        "" if row["seek_mean_gib"] is None else f"{row['seek_mean_gib']:.6f}",
                        "" if row["seek_p50_gib"] is None else f"{row['seek_p50_gib']:.6f}",
                        "" if row["seek_p99_gib"] is None else f"{row['seek_p99_gib']:.6f}",
                        row["service_ns"],
                        row["positioning_ns"],
                        f"{row['service_ms_per_s']:.6f}",
                        f"{row['positioning_ms_per_s']:.6f}",
                    ]
                )
    trans_path = out_dir / "blk-transitions.csv"
    with trans_path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(
            ["step_index", "step_name", "prev", "next", "count", "positioning_ns", "positioning_ms"]
        )
        for step in report["steps"]:
            for row in step["transitions"]:
                writer.writerow(
                    [
                        step["index"],
                        step["name"],
                        row["prev"],
                        row["next"],
                        row["count"],
                        row["positioning_ns"],
                        f"{row['positioning_ns'] / 1_000_000:.6f}",
                    ]
                )


def write_outputs(report: dict, out_dir: Path) -> str:
    text = render_text(report)
    (out_dir / "blk-report.txt").write_text(text, encoding="utf-8")
    (out_dir / "blk-report.json").write_text(
        json.dumps(report, indent=2, sort_keys=False) + "\n", encoding="utf-8"
    )
    write_csv(report, out_dir)
    return text


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Per-file disk-time report for an irys-sm-bench run.")
    parser.add_argument("out_dir", type=Path)
    parser.add_argument("--steps", type=Path, help="steps.jsonl if it is not in out-dir")
    parser.add_argument("--trace", type=Path, help="bpftrace log (default: <out-dir>/trace.log)")
    parser.add_argument("--zone-outer", type=float, default=OUTER_MIB_S)
    parser.add_argument("--zone-inner", type=float, default=INNER_MIB_S)
    parser.add_argument("--device-sectors", type=int, default=None)
    args = parser.parse_args(argv)
    if args.zone_outer <= 0 or args.zone_inner <= 0:
        print("zone rates must be greater than 0", file=sys.stderr)
        return 2
    out_dir = args.out_dir
    if not out_dir.is_dir():
        print(f"out dir not found: {out_dir}", file=sys.stderr)
        return 2
    report = build_report(
        out_dir,
        steps_path=args.steps,
        trace_path=args.trace,
        zone_outer=args.zone_outer,
        zone_inner=args.zone_inner,
        device_sectors=args.device_sectors,
    )
    text = write_outputs(report, out_dir)
    sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
