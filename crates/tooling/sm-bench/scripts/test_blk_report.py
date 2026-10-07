"""Tests for blk-report.py. Stdlib only.

Run from the repo or from this directory:

    python3 -m unittest crates/tooling/sm-bench/scripts/test_blk_report.py
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path


def load_report():
    path = Path(__file__).with_name("blk-report.py")
    spec = importlib.util.spec_from_file_location("blk_report", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MOD = load_report()


def issued(mono, sector, nr, rwbs, comm="a", pid=1):
    return ("I", mono, sector, nr, rwbs, comm, pid)


def complete(mono, sector, nr, rwbs):
    return ("C", mono, sector, nr, rwbs, "", 0)


class PairAndTimeTests(unittest.TestCase):
    def test_serial_service_drops_queue_overlap(self):
        requests, unmatched_i, unmatched_c = MOD.pair_requests(
            [
                issued(0, 0, 10, "R"),
                issued(50, 100, 10, "R"),
                complete(100, 0, 10, "R"),
                complete(180, 100, 10, "R"),
            ]
        )
        self.assertEqual((unmatched_i, unmatched_c), (0, 0))
        for req in requests:
            req.category = "recall"
        MOD.attribute(requests, device_sectors=2_000_000_000, outer=266, inner=130)
        by_sector = {req.sector: req for req in requests}
        self.assertEqual(by_sector[0].service_ns, 100)
        self.assertEqual(by_sector[100].service_ns, 80)
        self.assertIsNone(by_sector[0].seek_sectors)
        self.assertEqual(by_sector[100].seek_sectors, 90)
        self.assertEqual(by_sector[100].prev_category, "recall")

    def test_flush_keeps_the_head_and_owns_its_service(self):
        requests, _, _ = MOD.pair_requests(
            [
                issued(0, 1000, 100, "R", pid=1),
                complete(100, 1000, 100, "R"),
                issued(100, 0, 0, "F", comm="flush", pid=2),
                complete(150, 0, 0, "F"),
                issued(160, 1100, 10, "W", pid=3),
                complete(190, 1100, 10, "W"),
            ]
        )
        extents = [MOD.Extent(1000, 1200, "write-region.dat")]
        for req in requests:
            req.category = MOD.classify(req, extents)
        MOD.attribute(requests, 0, 266, 130)
        by_pid = {req.pid: req for req in requests}
        self.assertEqual(by_pid[2].category, "flush")
        self.assertEqual(by_pid[2].service_ns, 50)
        self.assertEqual(by_pid[2].positioning_ns, 50)
        self.assertIsNone(by_pid[2].seek_sectors)
        self.assertEqual(by_pid[3].category, "packed_write")
        self.assertEqual(by_pid[3].service_ns, 30)
        self.assertEqual(by_pid[3].seek_sectors, 0)
        self.assertEqual(by_pid[3].prev_category, "flush")

    def test_partial_complete_splits_one_issue(self):
        requests, unmatched_i, unmatched_c = MOD.pair_requests(
            [
                issued(0, 500, 100, "R"),
                complete(100, 500, 40, "R"),
                complete(150, 540, 60, "R"),
            ]
        )
        self.assertEqual((unmatched_i, unmatched_c), (0, 0))
        self.assertEqual(len(requests), 2)
        MOD.attribute(requests, 0, 266, 130)
        self.assertEqual(requests[0].nr, 40)
        self.assertEqual(requests[0].service_ns, 100)
        self.assertEqual(requests[1].sector, 540)
        self.assertEqual(requests[1].nr, 60)
        self.assertEqual(requests[1].service_ns, 50)
        self.assertEqual(requests[1].seek_sectors, 0)
        self.assertEqual(requests[0].issue_ns, 0)
        self.assertEqual(requests[1].issue_ns, 0)

    def test_merge_grows_one_request(self):
        requests, unmatched_i, _ = MOD.pair_requests(
            [
                issued(100, 500, 10, "R"),
                issued(110, 500, 40, "R"),
                complete(200, 500, 40, "R"),
            ]
        )
        self.assertEqual(unmatched_i, 0)
        self.assertEqual(len(requests), 1)
        self.assertEqual(requests[0].issue_ns, 110)
        self.assertEqual(requests[0].nr, 40)
        MOD.attribute(requests, 0, 266, 130)
        self.assertEqual(requests[0].service_ns, 90)

    def test_repeated_sector_matches_in_issue_order(self):
        # The short read must not steal a completion that sits in the earlier
        # span, and an exact complete must hit that short read rather than a
        # leftover of the earlier span.
        requests, unmatched_i, unmatched_c = MOD.pair_requests(
            [
                issued(0, 0, 100, "R", pid=1),
                issued(1, 0, 10, "R", pid=2),
                issued(2, 0, 10, "W", pid=3),
                complete(10, 50, 10, "R"),
                complete(11, 0, 10, "R"),
                complete(12, 0, 10, "W"),
                complete(13, 0, 40, "R"),
            ]
        )
        self.assertEqual((unmatched_i, unmatched_c), (2, 0))
        self.assertEqual([req.pid for req in requests], [1, 2, 3, 1])
        self.assertEqual((requests[0].sector, requests[0].nr), (50, 10))
        self.assertEqual((requests[1].sector, requests[1].nr), (0, 10))
        self.assertEqual((requests[3].sector, requests[3].nr), (0, 40))

    def test_two_flushes_match_fifo(self):
        requests, _, _ = MOD.pair_requests(
            [
                issued(0, 0, 0, "F", pid=1),
                issued(10, 0, 0, "F", pid=2),
                complete(30, 0, 0, "F"),
                complete(40, 0, 0, "F"),
            ]
        )
        self.assertEqual([req.pid for req in requests], [1, 2])
        MOD.attribute(requests, 0, 266, 130)
        self.assertEqual(requests[0].service_ns, 30)
        self.assertEqual(requests[1].service_ns, 10)


class ClassifyTests(unittest.TestCase):
    def classify_one(self, sector, nr, rwbs, extents):
        req = MOD.Request(MOD.Issued(0, sector, nr, rwbs, "a", 1), 1, 0)
        return MOD.classify(req, extents)

    def test_file_and_flush_categories(self):
        extents = [
            MOD.Extent(0, 1000, "entropy-chunks.dat"),
            MOD.Extent(1000, 2000, "write-region.dat"),
            MOD.Extent(2000, 2010, "rocks/000001.log"),
            MOD.Extent(3000, 4000, "rocks/000001.sst"),
            MOD.Extent(4000, 5000, "rocks/000001.blob"),
            MOD.Extent(5000, 5010, "rocks/MANIFEST-000001"),
            MOD.Extent(6000, 7000, "db/mdbx.dat"),
            MOD.Extent(7000, 7010, "rocks/LOG"),
        ]
        self.assertEqual(self.classify_one(0, 10, "R", extents), "recall")
        self.assertEqual(self.classify_one(1000, 10, "R", extents), "entropy_read")
        self.assertEqual(self.classify_one(1000, 10, "W", extents), "packed_write")
        self.assertEqual(self.classify_one(1000, 16, "WF", extents), "packed_write")
        self.assertEqual(self.classify_one(2000, 8, "W", extents), "index_wal")
        self.assertEqual(self.classify_one(3000, 8, "R", extents), "index_sst")
        self.assertEqual(self.classify_one(4000, 8, "W", extents), "index_blob")
        self.assertEqual(self.classify_one(5000, 8, "W", extents), "index_other")
        self.assertEqual(self.classify_one(6000, 8, "W", extents), "index_other")
        self.assertEqual(self.classify_one(7000, 4, "W", extents), "index_other")
        self.assertEqual(self.classify_one(9000, 8, "R", extents), "metadata")
        self.assertEqual(self.classify_one(0, 8, "D", extents), "metadata")
        self.assertEqual(self.classify_one(0, 0, "F", extents), "flush")
        self.assertEqual(self.classify_one(1000, 0, "WF", extents), "flush")
        self.assertEqual(self.classify_one(0, 8, "W", extents), "metadata")

    def test_majority_of_sectors_picks_the_file(self):
        extents = [
            MOD.Extent(0, 10, "entropy-chunks.dat"),
            MOD.Extent(10, 50, "write-region.dat"),
        ]
        self.assertEqual(self.classify_one(0, 40, "R", extents), "entropy_read")

    def test_deleted_file_kept_until_reused(self):
        snaps = [
            (1000, [(0, 100, "rocks/000001.log")]),
            (5000, [(200, 300, "rocks/000002.sst")]),
            (9000, [(0, 100, "rocks/000002.sst")]),
        ]
        maps = MOD.build_maps(snaps)
        early = self.classify_one(50, 10, "R", MOD.nearest_map(maps, 1200))
        kept = self.classify_one(50, 10, "R", MOD.nearest_map(maps, 4000))
        reused = self.classify_one(50, 10, "R", MOD.nearest_map(maps, 8000))
        tie = self.classify_one(50, 10, "R", MOD.nearest_map(maps, 3000))
        self.assertEqual(early, "index_wal")
        self.assertEqual(kept, "index_wal")
        self.assertEqual(reused, "index_sst")
        self.assertEqual(tie, "index_wal")

    def test_duplicate_snapshot_keeps_the_nearer_map(self):
        # The duplicate at t=5000 is only 500 ns closer than the replacement
        # at t=5001 for a request at t=4500. Dropping that timestamp would
        # classify the request as the replacement file.
        rows = [
            (0, 100, "rocks/000001.log"),
            (10_000, 10_100, "rocks/000001.sst"),
            (50_000, 60_000, "write-region.dat"),
        ]
        replacement = [
            (0, 100, "rocks/000002.sst"),
            (10_000, 10_100, "rocks/000001.sst"),
            (50_000, 60_000, "write-region.dat"),
        ]
        maps = MOD.build_maps(
            [
                (1000, rows),
                (5000, list(rows)),
                (5001, replacement),
            ]
        )
        self.assertIs(maps[0][1], maps[1][1])
        self.assertIsNot(maps[1][1], maps[2][1])
        kept = MOD.nearest_map(maps, 4500)
        self.assertEqual(self.classify_one(50, 10, "R", kept), "index_wal")
        self.assertEqual(self.classify_one(10_050, 8, "R", kept), "index_sst")
        self.assertEqual(self.classify_one(55_000, 8, "R", kept), "entropy_read")
        self.assertEqual(self.classify_one(200, 8, "R", kept), "metadata")
        replaced = MOD.nearest_map(maps, 5001)
        self.assertEqual(self.classify_one(50, 10, "R", replaced), "index_sst")

    def test_bisect_finds_a_long_extent_behind_later_starts(self):
        rows = [(0, 1_000_000, "entropy-chunks.dat")]
        rows.extend(
            (2_000_000 + index * 100, 2_000_000 + index * 100 + 8, "rocks/000001.sst")
            for index in range(400)
        )
        extents = MOD.nearest_map(MOD.build_maps([(1000, rows)]), 1000)
        # Sector 500_000 sits inside the first extent. The bisect point is
        # the first later start, so the lookup has to step back.
        self.assertEqual(self.classify_one(500_000, 8, "R", extents), "recall")
        last = 2_000_000 + 399 * 100
        self.assertEqual(self.classify_one(last, 8, "R", extents), "index_sst")
        self.assertEqual(self.classify_one(1_500_000, 8, "R", extents), "metadata")
        self.assertEqual(self.classify_one(last + 8, 8, "R", extents), "metadata")

    def test_later_row_overwrites_an_overlap(self):
        extents = MOD.build_maps(
            [(1000, [(0, 100, "rocks/000001.log"), (40, 70, "rocks/000002.sst")])]
        )[0][1]
        self.assertEqual(self.classify_one(10, 8, "R", extents), "index_wal")
        self.assertEqual(self.classify_one(50, 8, "R", extents), "index_sst")
        self.assertEqual(self.classify_one(80, 8, "R", extents), "index_wal")

    def test_outer_zone_positioning_is_exact(self):
        # 100 MiB at 266 MiB/s is 100/266 s. Integer ns:
        # 100 * 1_000_000_000 // 266 = 375_939_849.
        nbytes = 204800 * 512
        self.assertEqual(nbytes, 100 * 1024 * 1024)
        rate = 266 * 1024 * 1024
        transfer = (nbytes * 1_000_000_000) // rate
        self.assertEqual(transfer, 375_939_849)
        requests, _, _ = MOD.pair_requests(
            [
                issued(1_000_000_000, 0, 204800, "R"),
                complete(1_400_000_000, 0, 204800, "R"),
            ]
        )
        requests[0].category = "recall"
        MOD.attribute(requests, device_sectors=2_000_000_000, outer=266, inner=130)
        self.assertEqual(requests[0].transfer_ns, 375_939_849)
        self.assertEqual(requests[0].service_ns, 400_000_000)
        self.assertEqual(requests[0].positioning_ns, 24_060_151)
        self.assertFalse(requests[0].clamped)


class ReportFileTests(unittest.TestCase):
    def test_steps_flush_and_iostat_delta(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            (out / "device.txt").write_text(
                "name=sdc\ndev_t=8388640\nsectors=2000000000\nsector_bytes=512\n",
                encoding="utf-8",
            )
            (out / "extents-500000000.txt").write_text(
                "\n".join(
                    [
                        "# monotonic_ns=500000000 realtime_ns=1 dev=sdc part_start=0 fs_block=4096",
                        "0 1000000000 entropy-chunks.dat",
                        "1800000000 1900000000 write-region.dat",
                        "1900000000 1900001000 rocks/000001.log",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            (out / "trace.log").write_text(
                "\n".join(
                    [
                        "# dev_t=8388640",
                        "1000000000 I 0 204800 R irys-sm-benc 42",
                        "1400000000 C 0 204800 R",
                        "1400000000 I 0 0 F kworker 7",
                        "1450000000 C 0 0 F",
                        "2100000000 I 1800000000 2048 W irys-sm-benc 42",
                        "2200000000 I 1900000000 8 W irys-sm-benc 42",
                        "2300000000 C 1800000000 2048 W",
                        "2360000000 C 1900000000 8 W",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            (out / "steps.jsonl").write_text(
                "\n".join(
                    [
                        '{"kind":"run","realtime_ns":1700000000000000000,"monotonic_ns":900000000}',
                        '{"kind":"step","index":0,"name":"idle","phase":"start","realtime_ns":1700000000000000000,"monotonic_ns":1000000000}',
                        '{"kind":"step","index":0,"name":"idle","phase":"drain","realtime_ns":1700000000100000000,"monotonic_ns":1800000000}',
                        '{"kind":"step","index":0,"name":"idle","phase":"end","realtime_ns":1700000000200000000,"monotonic_ns":2000000000}',
                        '{"kind":"step","index":1,"name":"ingest-16","phase":"start","realtime_ns":1700000000200000000,"monotonic_ns":2000000000}',
                        '{"kind":"step","index":1,"name":"ingest-16","phase":"drain","realtime_ns":1700000000800000000,"monotonic_ns":2800000000}',
                        '{"kind":"step","index":1,"name":"ingest-16","phase":"end","realtime_ns":1700000001000000000,"monotonic_ns":3000000000}',
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            (out / "busy.log").write_text(
                "\n".join(
                    [
                        "# monotonic_ns realtime_ns io_ticks_ms",
                        "1000000000 1 0",
                        "2000000000 1 440",
                        "3000000000 1 700",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            code = MOD.main([str(out)])
            self.assertEqual(code, 0)
            report = json.loads((out / "blk-report.json").read_text(encoding="utf-8"))
            self.assertEqual(len(report["steps"]), 2)
            idle, ingest = report["steps"]
            self.assertEqual(idle["name"], "idle")
            self.assertEqual(idle["categories"]["recall"]["requests"], 1)
            self.assertEqual(idle["categories"]["recall"]["positioning_ns"], 24_060_151)
            self.assertEqual(idle["categories"]["flush"]["requests"], 1)
            self.assertEqual(idle["categories"]["flush"]["positioning_ns"], 50_000_000)
            self.assertEqual(idle["service_ns"], 450_000_000)
            self.assertEqual(idle["iostat_busy_ms"], 440)
            self.assertEqual(idle["busy_delta_ms"], 10)
            self.assertEqual(idle["transitions"][0]["prev"], "recall")
            self.assertEqual(idle["transitions"][0]["next"], "flush")
            self.assertEqual(idle["transitions"][0]["positioning_ns"], 50_000_000)
            self.assertEqual(ingest["name"], "ingest-16")
            self.assertEqual(ingest["categories"]["packed_write"]["requests"], 1)
            self.assertEqual(ingest["categories"]["index_wal"]["requests"], 1)
            self.assertEqual(ingest["categories"]["packed_write"]["service_ns"], 200_000_000)
            self.assertEqual(ingest["categories"]["index_wal"]["service_ns"], 60_000_000)
            self.assertEqual(ingest["iostat_busy_ms"], 260)
            self.assertEqual(ingest["busy_delta_ms"], 0)
            pairs = {(row["prev"], row["next"]) for row in ingest["transitions"]}
            self.assertIn(("flush", "packed_write"), pairs)
            self.assertIn(("packed_write", "index_wal"), pairs)
            text = (out / "blk-report.txt").read_text(encoding="utf-8")
            self.assertIn("metadata/other", text)
            self.assertIn("recall -> flush", text)
            self.assertIn("delta +10.0 ms", text)
            self.assertIn("iostat busy 440.0 ms", text)
            csv_text = (out / "blk-report.csv").read_text(encoding="utf-8")
            self.assertIn("index_wal", csv_text)
            self.assertIn("packed_write", csv_text)
            trans = (out / "blk-transitions.csv").read_text(encoding="utf-8")
            self.assertIn("recall,flush", trans)

    def test_missing_trace_exits(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit):
                MOD.build_report(Path(tmp))

    def test_progress_and_elapsed_go_to_stderr(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            (out / "trace.log").write_text(
                "\n".join(
                    [
                        "# dev_t=1",
                        "10 I 0 8 R bench 1",
                        "20 C 0 8 R",
                        "30 I 8 8 R bench 1",
                        "40 C 8 8 R",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            err = io.StringIO()
            with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
                code = MOD.main([str(out), "--progress", "1"])
            self.assertEqual(code, 0)
            text = err.getvalue()
            self.assertIn("progress 1 issued", text)
            self.assertIn("progress 2 issued", text)
            self.assertRegex(text, r"elapsed \d+\.\d{3}s\n")
            report = (out / "blk-report.txt").read_text(encoding="utf-8")
            self.assertNotIn("elapsed", report)
            self.assertNotIn("progress", report)


if __name__ == "__main__":
    unittest.main()
