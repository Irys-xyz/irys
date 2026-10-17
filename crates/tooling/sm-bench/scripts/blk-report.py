#!/usr/bin/env python3
"""Split a blk-run.sh trace into per-file disk time.

Zone rate is linear in LBA fraction: 266 MiB/s at the first sector, 130 MiB/s
at the last. Outer-track sequential fills on the lab HDD measured about
266 MiB/s. The write region in allocation group 18 (groups 0-20) filled at
about 146 MiB/s, which is this line there. Override with --zone-outer and
--zone-inner.

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
import hashlib
import json
import random
import sys
import time
from collections import deque
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


class _IndexedExtents(list):
    """Extents sorted by start, with the start column cached for bisect."""

    def __init__(self, extents: list[Extent]) -> None:
        super().__init__(extents)
        self.starts = [ext.start for ext in self]


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


def parse_trace_lines(
    lines, progress: int = 0
) -> tuple[list[tuple], int | None, int]:
    """Return (events, dev_t, skipped lines).

    An event is (kind, mono, sector, nr, rwbs, comm, pid).
    `lines` is any iterable of trace lines, so the caller can stream a file.
    With progress > 0, write one stderr line every that many issued requests.
    """
    events = []
    dev_t = None
    skipped = 0
    issued = 0
    for raw in lines:
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
        if parts[1] == "I":
            issued += 1
            if progress and issued % progress == 0:
                print(f"progress {issued} issued", file=sys.stderr, flush=True)
            if len(parts) >= 7:
                comm = parts[5]
                try:
                    pid = int(parts[6])
                except ValueError:
                    pid = 0
        events.append((parts[1], mono, sector, nr, rwbs, comm, pid))
    return events, dev_t, skipped


def parse_trace(text: str) -> tuple[list[tuple], int | None, int]:
    """Return (events, dev_t, skipped lines).

    An event is (kind, mono, sector, nr, rwbs, comm, pid).
    """
    return parse_trace_lines(text.splitlines())


# Sequence numbers leave a gap so a split can put the leftover pieces back
# where the parent sat without renumbering the whole queue.
_SEQ_GAP = 1 << 20
_OP_NAMES = ("", "R", "W", "D")


class _TNode:
    __slots__ = ("key", "end", "prio", "left", "right", "sub")

    def __init__(self, key: int, end: int, prio: int) -> None:
        self.key = key
        self.end = end
        self.prio = prio
        self.left = None
        self.right = None
        self.sub = end


def _t_pull(node: _TNode) -> None:
    best = node.end
    left = node.left
    right = node.right
    if left is not None and left.sub > best:
        best = left.sub
    if right is not None and right.sub > best:
        best = right.sub
    node.sub = best


def _t_rot_right(node: _TNode) -> _TNode:
    left = node.left
    node.left = left.right
    left.right = node
    _t_pull(node)
    _t_pull(left)
    return left


def _t_rot_left(node: _TNode) -> _TNode:
    right = node.right
    node.right = right.left
    right.left = node
    _t_pull(node)
    _t_pull(right)
    return right


def _t_insert(node: _TNode | None, key: int, end: int, prio: int) -> _TNode:
    if node is None:
        return _TNode(key, end, prio)
    if key == node.key:
        if end > node.end:
            node.end = end
            _t_pull(node)
        return node
    if key < node.key:
        node.left = _t_insert(node.left, key, end, prio)
        if node.left.prio < node.prio:
            node = _t_rot_right(node)
    else:
        node.right = _t_insert(node.right, key, end, prio)
        if node.right.prio < node.prio:
            node = _t_rot_left(node)
    _t_pull(node)
    return node


def _t_merge(left: _TNode | None, right: _TNode | None) -> _TNode | None:
    if left is None:
        return right
    if right is None:
        return left
    if left.prio < right.prio:
        left.right = _t_merge(left.right, right)
        _t_pull(left)
        return left
    right.left = _t_merge(left, right.left)
    _t_pull(right)
    return right


def _t_delete(node: _TNode | None, key: int) -> _TNode | None:
    if node is None:
        return None
    if key < node.key:
        node.left = _t_delete(node.left, key)
    elif key > node.key:
        node.right = _t_delete(node.right, key)
    else:
        return _t_merge(node.left, node.right)
    _t_pull(node)
    return node


def _t_max_end_le(node: _TNode | None, sector: int) -> int:
    """Max end among intervals whose start is <= sector. 0 when none."""
    best = 0
    while node is not None:
        if node.key <= sector:
            left = node.left
            if left is not None and left.sub > best:
                best = left.sub
            if node.end > best:
                best = node.end
            node = node.right
        else:
            node = node.left
    return best


class _QNode:
    __slots__ = ("iss", "prev", "next", "seq")

    def __init__(self, iss: Issued, seq: int) -> None:
        self.iss = iss
        self.prev = None
        self.next = None
        self.seq = seq


class _Queue:
    """Outstanding requests in issue order, indexed by span.

    A real trace holds tens of thousands of in-flight requests. Scanning
    that list per completion is quadratic. Exact matches and merges use
    the sector indexes. A completion that is only a slice of a larger
    request is rare; a treap of (start -> max end) says whether any span
    can contain it before the list is walked.
    """

    def __init__(self) -> None:
        self.head = _QNode(Issued(0, 0, 0, "", "", 0), 0)
        self.tail = _QNode(Issued(0, 0, 0, "", "", 0), 0)
        self.head.next = self.tail
        self.tail.prev = self.head
        self.count = 0
        self.clock = 0
        self.rng = random.Random(0)
        self.exact: dict[tuple[int, int], list[_QNode]] = {}
        self.zeros: dict[int, list[_QNode]] = {}
        self.merge: dict[tuple[int, str], list[_QNode]] = {}
        self.ends: dict[tuple[str, int], list[int]] = {}
        self.roots: dict[str, _TNode | None] = {op: None for op in _OP_NAMES}

    def append(self, iss: Issued) -> None:
        self.clock += _SEQ_GAP
        self._insert_before(self.tail, iss, self.clock)

    def replace(self, node: _QNode, iss: Issued) -> None:
        self._unindex(node)
        node.iss = iss
        self._index(node)

    def unlink(self, node: _QNode) -> Issued:
        self._unindex(node)
        node.prev.next = node.next
        node.next.prev = node.prev
        self.count -= 1
        return node.iss

    def replace_with_pieces(self, node: _QNode, pieces: list[Issued]) -> None:
        nxt = node.next
        self.unlink(node)
        if not pieces:
            return
        for seq, iss in zip(self._seqs_before(nxt, len(pieces)), pieces):
            self._insert_before(nxt, iss, seq)

    def find_merge(self, sector: int, rwbs: str, nr: int) -> _QNode | None:
        bucket = self.merge.get((sector, rwbs))
        if not bucket:
            return None
        for node in bucket:
            old_nr = node.iss.nr
            if nr > old_nr > 0:
                return node
        return None

    def find_exact(self, sector: int, nr: int, rwbs: str) -> _QNode | None:
        bucket = self.exact.get((sector, nr))
        if not bucket:
            return None
        for node in bucket:
            if same_op(node.iss.rwbs, rwbs):
                return node
        return None

    def find_zero(self, sector: int, rwbs: str) -> _QNode | None:
        bucket = self.zeros.get(sector)
        if not bucket:
            return None
        for node in bucket:
            if same_op(node.iss.rwbs, rwbs):
                return node
        return None

    def find_partial(self, sector: int, nr: int, rwbs: str) -> _QNode | None:
        end = sector + nr
        if not self._may_contain(sector, end, rwbs):
            return None
        node = self.head.next
        while node is not self.tail:
            old = node.iss
            if old.nr > 0 and same_op(old.rwbs, rwbs):
                if old.sector <= sector and end <= old.sector + old.nr:
                    return node
            node = node.next
        return None

    def _may_contain(self, sector: int, end: int, rwbs: str) -> bool:
        op = op_letter(rwbs)
        names = _OP_NAMES if op == "" else (op, "")
        for name in names:
            if _t_max_end_le(self.roots[name], sector) >= end:
                return True
        return False

    def _insert_before(self, nxt: _QNode, iss: Issued, seq: int) -> None:
        node = _QNode(iss, seq)
        prev = nxt.prev
        prev.next = node
        node.prev = prev
        node.next = nxt
        nxt.prev = node
        self.count += 1
        self._index(node)

    def _seqs_before(self, nxt: _QNode, count: int) -> list[int]:
        prev = nxt.prev
        left = 0 if prev is self.head else prev.seq
        right = self.clock + _SEQ_GAP if nxt is self.tail else nxt.seq
        if right - left <= count:
            self._reseat()
            return self._seqs_before(nxt, count)
        step = (right - left) // (count + 1)
        return [left + step * (index + 1) for index in range(count)]

    def _reseat(self) -> None:
        seq = 0
        node = self.head.next
        while node is not self.tail:
            seq += _SEQ_GAP
            node.seq = seq
            node = node.next
        self.clock = seq

    def _index(self, node: _QNode) -> None:
        iss = node.iss
        if iss.nr == 0:
            _bucket_add(self.zeros, iss.sector, node)
            return
        _bucket_add(self.exact, (iss.sector, iss.nr), node)
        _bucket_add(self.merge, (iss.sector, iss.rwbs), node)
        self._set_end(op_letter(iss.rwbs), iss.sector, iss.sector + iss.nr, add=True)

    def _unindex(self, node: _QNode) -> None:
        iss = node.iss
        if iss.nr == 0:
            _bucket_drop(self.zeros, iss.sector, node)
            return
        _bucket_drop(self.exact, (iss.sector, iss.nr), node)
        _bucket_drop(self.merge, (iss.sector, iss.rwbs), node)
        self._set_end(op_letter(iss.rwbs), iss.sector, iss.sector + iss.nr, add=False)

    def _set_end(self, op: str, start: int, end: int, add: bool) -> None:
        key = (op, start)
        ends = self.ends.get(key)
        if add:
            if ends is None:
                ends = []
                self.ends[key] = ends
            ends.append(end)
        else:
            ends.remove(end)
            if not ends:
                del self.ends[key]
                ends = None
        self.roots[op] = _t_delete(self.roots[op], start)
        if ends:
            self.roots[op] = _t_insert(self.roots[op], start, max(ends), self.rng.getrandbits(32))


def _bucket_add(buckets: dict, key, node: _QNode) -> None:
    bucket = buckets.get(key)
    if bucket is None:
        buckets[key] = [node]
        return
    seq = node.seq
    lo = 0
    hi = len(bucket)
    while lo < hi:
        mid = (lo + hi) // 2
        if bucket[mid].seq < seq:
            lo = mid + 1
        else:
            hi = mid
    bucket.insert(lo, node)


def _bucket_drop(buckets: dict, key, node: _QNode) -> None:
    bucket = buckets.get(key)
    if bucket is None:
        return
    bucket.remove(node)
    if not bucket:
        del buckets[key]


def _is_flush_issue(nr: int, rwbs: str) -> bool:
    return nr == 0 and "F" in rwbs.upper()


def _is_flush_complete(nr: int, rwbs: str) -> bool:
    """Zero-length flush completion, or the tail of a PREFLUSH/FUA write.

    The tail keeps the data request's rwbs (`WSM`, `WS`) and has nr_sector 0.
    """
    if nr != 0:
        return False
    flags = rwbs.upper()
    if "F" in flags:
        return True
    return "W" in flags


def _bump(counts: dict[str, int], rwbs: str) -> None:
    counts[rwbs] = counts.get(rwbs, 0) + 1


def _sorted_counts(counts: dict[str, int]) -> dict[str, int]:
    keys = sorted(counts, key=lambda rwbs: (-counts[rwbs], rwbs))
    return {key: counts[key] for key in keys}


class Pairing(tuple):
    """`(requests, unmatched_issues, unmatched_completes)`.

    `issue_rwbs` and `complete_rwbs` are the same totals split by rwbs.
    """

    def __new__(
        cls,
        requests: list[Request],
        unmatched_issues: int,
        unmatched_completes: int,
        issue_rwbs: dict[str, int],
        complete_rwbs: dict[str, int],
    ) -> "Pairing":
        obj = super().__new__(cls, (requests, unmatched_issues, unmatched_completes))
        obj.issue_rwbs = issue_rwbs
        obj.complete_rwbs = complete_rwbs
        return obj


def _queue_rwbs(live: _Queue) -> dict[str, int]:
    counts: dict[str, int] = {}
    node = live.head.next
    while node is not live.tail:
        _bump(counts, node.iss.rwbs)
        node = node.next
    return counts


def pair_requests(events: list[tuple]) -> Pairing:
    """Match issue to complete. Merges grow an outstanding request.

    A completion may be a prefix, a suffix, or a middle slice of the issued
    span. The uncovered part stays outstanding.

    Zero-length flush issues pair FIFO with zero-length completions. The
    completion's sector and rwbs are not part of the key: `block_rq_complete`
    reports `blk_rq_pos` of a flush, which is `(sector_t)-1`, and the
    PREFLUSH/FUA sequence ends the original write again with `nr_sector` 0.
    """
    live = _Queue()
    flushes: deque[Issued] = deque()
    done: list[Request] = []
    unmatched_complete_rwbs: dict[str, int] = {}
    seq = 0
    for kind, mono, sector, nr, rwbs, comm, pid in events:
        if kind == "I":
            fresh = Issued(mono, sector, nr, rwbs, comm, pid)
            # Flush starts go to their own FIFO. A requeue traces another
            # issue and no completion; those extras stay unpaired.
            if _is_flush_issue(nr, rwbs):
                flushes.append(fresh)
                continue
            if nr > 0:
                node = live.find_merge(sector, rwbs, nr)
                if node is not None:
                    live.replace(node, fresh)
                    continue
            live.append(fresh)
            continue
        matched: Issued | None = None
        if _is_flush_complete(nr, rwbs):
            if flushes:
                matched = flushes.popleft()
            else:
                # No flush start left. Keep the tail so it is counted as a flush.
                # Its issue time is this completion, so it adds no service
                # time on top of the post-flush that just finished.
                flags = rwbs if "F" in rwbs.upper() else "F"
                matched = Issued(mono, 0, 0, flags, "", 0)
        elif nr == 0:
            node = live.find_zero(sector, rwbs)
            if node is not None:
                matched = live.unlink(node)
        else:
            node = live.find_exact(sector, nr, rwbs)
            if node is not None:
                matched = live.unlink(node)
            else:
                node = live.find_partial(sector, nr, rwbs)
                if node is not None:
                    old = node.iss
                    matched = old.clone(sector=sector, nr=nr)
                    end = sector + nr
                    old_end = old.sector + old.nr
                    pieces: list[Issued] = []
                    if old.sector < sector:
                        pieces.append(old.clone(sector=old.sector, nr=sector - old.sector))
                    if end < old_end:
                        pieces.append(old.clone(sector=end, nr=old_end - end))
                    live.replace_with_pieces(node, pieces)
        if matched is None:
            _bump(unmatched_complete_rwbs, rwbs)
            continue
        done.append(Request(matched, mono, seq))
        seq += 1
    issue_rwbs = _queue_rwbs(live)
    for iss in flushes:
        _bump(issue_rwbs, iss.rwbs)
    issue_rwbs = _sorted_counts(issue_rwbs)
    complete_rwbs = _sorted_counts(unmatched_complete_rwbs)
    return Pairing(
        done,
        sum(issue_rwbs.values()),
        sum(complete_rwbs.values()),
        issue_rwbs,
        complete_rwbs,
    )


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


def _rows_overlap(ordered: list[tuple[int, int, str]]) -> bool:
    for index in range(len(ordered) - 1):
        if ordered[index][1] > ordered[index + 1][0]:
            return True
    return False


def _erase_range(
    pieces: list[tuple[int, int, str]], starts: list[int], start: int, end: int
) -> None:
    """Cut [start, end) out of a sorted, non-overlapping piece list."""
    index = bisect.bisect_left(starts, start)
    if index > 0:
        index -= 1
    while index < len(pieces):
        piece_start, piece_end, path = pieces[index]
        if piece_start >= end:
            break
        if piece_end <= start:
            index += 1
            continue
        replacement: list[tuple[int, int, str]] = []
        if piece_start < start:
            replacement.append((piece_start, start, path))
        if piece_end > end:
            replacement.append((end, piece_end, path))
        pieces[index : index + 1] = replacement
        starts[index : index + 1] = [item[0] for item in replacement]
        if piece_end > end:
            break
        if not replacement:
            continue
        index += 1


def _coverage(rows: list[tuple[int, int, str]]) -> list[tuple[int, int, str]]:
    """Sorted, non-overlapping coverage. A later row overwrites an overlap."""
    fresh = [(start, end, path) for start, end, path in rows if end > start]
    if not fresh:
        return []
    ordered = sorted(fresh, key=lambda item: (item[0], item[1], item[2]))
    if not _rows_overlap(ordered):
        return ordered
    pieces: list[tuple[int, int, str]] = []
    starts: list[int] = []
    for start, end, path in fresh:
        _erase_range(pieces, starts, start, end)
        index = bisect.bisect_right(starts, start)
        pieces.insert(index, (start, end, path))
        starts.insert(index, start)
    return pieces


def _subtract_coverage(
    kept: list[tuple[int, int, str]], cuts: list[tuple[int, int, str]]
) -> list[tuple[int, int, str]]:
    """Drop `cuts` from `kept`. Both lists are sorted and non-overlapping."""
    out: list[tuple[int, int, str]] = []
    cut_index = 0
    cut_count = len(cuts)
    for start, end, path in kept:
        while cut_index < cut_count and cuts[cut_index][1] <= start:
            cut_index += 1
        cursor = start
        scan = cut_index
        while scan < cut_count and cuts[scan][0] < end:
            cut_start, cut_end, _cut_path = cuts[scan]
            if cut_start > cursor:
                out.append((cursor, cut_start, path))
            if cut_end > cursor:
                cursor = cut_end
            if cursor >= end:
                break
            scan += 1
        if cursor < end:
            out.append((cursor, end, path))
    return out


def evolve(prev: list[Extent], rows: list[tuple[int, int, str]]) -> list[Extent]:
    """Files present in this snapshot replace their old extents.

    A file that is absent keeps its last extents until a present file
    takes those sectors.

    Each snapshot is thousands of rows. Cutting those rows out of a copied
    extent list one row at a time is quadratic, so the rows become one
    sorted coverage and the kept extents are cut in one pass.
    """
    present = {path for _, _, path in rows}
    kept = [(ext.start, ext.end, ext.path) for ext in prev if ext.path not in present]
    fresh = _coverage(rows)
    if fresh:
        kept = _subtract_coverage(kept, fresh)
    merged = coalesce([Extent(start, end, path) for start, end, path in kept + fresh])
    return _IndexedExtents(merged)


def build_maps(snapshots: list[tuple[int, list[tuple[int, int, str]]]]) -> list[tuple[int, list[Extent]]]:
    """One extent map per snapshot time.

    Identical consecutive row lists reuse one map. Replaying the same rows
    does not change the map, and a request still bisects every timestamp.
    """
    current: list[Extent] = []
    maps = []
    prev_rows: list[tuple[int, int, str]] | None = None
    for when, rows in snapshots:
        if prev_rows is not None and (rows is prev_rows or rows == prev_rows):
            maps.append((when, current))
            continue
        current = evolve(current, rows)
        maps.append((when, current))
        prev_rows = rows
    return maps


def nearest_index(times: list[int], when: int) -> int:
    """Index of the snapshot time closest to `when`. Ties take the earlier one."""
    index = bisect.bisect_left(times, when)
    if index <= 0:
        return 0
    if index >= len(times):
        return len(times) - 1
    if when - times[index - 1] <= times[index] - when:
        return index - 1
    return index


def nearest_map(maps: list[tuple[int, list[Extent]]], when: int) -> list[Extent]:
    if not maps:
        return []
    times = [item[0] for item in maps]
    return maps[nearest_index(times, when)][1]


def overlap_counts(extents: list[Extent], start: int, end: int) -> dict[str, int]:
    counts: dict[str, int] = {}
    starts = extents.starts if isinstance(extents, _IndexedExtents) else [ext.start for ext in extents]
    if not starts:
        return counts
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


def parse_extent_text(text: str) -> tuple[int | None, list[tuple[int, int, str]], bytes]:
    """Return (monotonic_ns, rows, sha256 of the parsed rows in file order).

    The header timestamp is not part of the digest. Consecutive snapshots
    with the same digest are the same map.
    """
    when = None
    rows = []
    hasher = hashlib.sha256()
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
        hasher.update(str(start).encode())
        hasher.update(b"\t")
        hasher.update(str(end).encode())
        hasher.update(b"\t")
        hasher.update(path.encode())
        hasher.update(b"\n")
    return when, rows, hasher.digest()


def parse_extent_file(text: str) -> tuple[int | None, list[tuple[int, int, str]]]:
    when, rows, _digest = parse_extent_text(text)
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
    """Snapshots sorted by time. Identical consecutive bodies share one row list."""
    loaded = []
    for path in sorted(out_dir.glob("extents-*.txt")):
        when, rows, digest = parse_extent_text(
            path.read_text(encoding="utf-8", errors="replace")
        )
        if when is None:
            continue
        loaded.append((when, path.name, rows, digest))
    loaded.sort(key=lambda item: (item[0], item[1]))
    snaps = []
    prev_digest = None
    prev_rows: list[tuple[int, int, str]] | None = None
    for when, _name, rows, digest in loaded:
        if digest == prev_digest and prev_rows is not None:
            snaps.append((when, prev_rows))
            continue
        snaps.append((when, rows))
        prev_digest = digest
        prev_rows = rows
    return snaps


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
    progress: int = 0,
) -> dict:
    trace_file = trace_path or (out_dir / "trace.log")
    if not trace_file.is_file():
        raise SystemExit(f"trace not found: {trace_file}")
    with trace_file.open(encoding="utf-8", errors="replace") as handle:
        events, trace_dev, skipped = parse_trace_lines(handle, progress=progress)
    pairing = pair_requests(events)
    requests, unmatched_issues, unmatched_completes = pairing
    warnings = []
    if skipped:
        warnings.append(f"skipped {skipped} trace lines")

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
    times = [item[0] for item in maps]
    for req in requests:
        extents = maps[nearest_index(times, req.issue_ns)][1] if times else []
        req.category = classify(req, extents)
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
        "unpaired_issues": pairing.issue_rwbs,
        "unpaired_completes": pairing.complete_rwbs,
        "warnings": warnings,
        "steps": rendered,
    }


def format_unpaired(title: str, counts: dict[str, int]) -> str:
    if not counts:
        body = "none"
    else:
        body = ", ".join(f"{rwbs} {count}" for rwbs, count in counts.items())
    return f"unpaired {title}: {body}"


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
    lines.append(format_unpaired("issues", report.get("unpaired_issues") or {}))
    lines.append(format_unpaired("completes", report.get("unpaired_completes") or {}))
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
        lines.append("  transitions (which category the head moved from and to), by positioning:")
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
    parser.add_argument(
        "--progress",
        type=int,
        metavar="N",
        help="write a stderr line every N issued requests",
    )
    args = parser.parse_args(argv)
    if args.zone_outer <= 0 or args.zone_inner <= 0:
        print("zone rates must be greater than 0", file=sys.stderr)
        return 2
    if args.progress is not None and args.progress < 1:
        print("--progress needs a positive interval", file=sys.stderr)
        return 2
    out_dir = args.out_dir
    if not out_dir.is_dir():
        print(f"out dir not found: {out_dir}", file=sys.stderr)
        return 2
    started = time.perf_counter()
    try:
        report = build_report(
            out_dir,
            steps_path=args.steps,
            trace_path=args.trace,
            zone_outer=args.zone_outer,
            zone_inner=args.zone_inner,
            device_sectors=args.device_sectors,
            progress=args.progress or 0,
        )
        text = write_outputs(report, out_dir)
        sys.stdout.write(text)
    finally:
        elapsed = time.perf_counter() - started
        print(f"elapsed {elapsed:.3f}s", file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
