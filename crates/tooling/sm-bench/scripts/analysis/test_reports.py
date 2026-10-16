"""Pure-function tests for the load-bench analysis scripts."""

from __future__ import annotations

import unittest

from analysis.disktime import IostatFormatError, parse_iostat, window_time
from analysis.ladder import percentile, summarize_step
from analysis.minutes import iter_minutes, minute_bounds, minute_stats


def sample(step, sec, driver, **fields):
    row = {
        "step": step,
        "sec": sec,
        "driver": driver,
        "ops": 0,
        "skips": 0,
        "enqueued": 0,
        "durable": 0,
        "lat_max_us": 0,
        "start_delay_max_us": 0,
        "pending_chunks": 0,
        "phases": {},
    }
    row.update(fields)
    return row


def iostat_line(device, rmb, wmb, util, rs=10, rsz=128, ws=10, wsz=128, fs=1):
    # Indexes 0..19 plus two trailing fields so %util is last and f/s is 19.
    parts = ["0"] * 22
    parts[0] = device
    parts[1] = str(rs)
    parts[2] = str(rmb)
    parts[5] = "1"
    parts[6] = str(rsz)
    parts[7] = str(ws)
    parts[8] = str(wmb)
    parts[11] = "1"
    parts[12] = str(wsz)
    parts[19] = str(fs)
    parts[-1] = str(util)
    return " ".join(parts)


class PercentileTests(unittest.TestCase):
    def test_floor_rank_and_empty(self):
        self.assertEqual(percentile([], 0.99), 0)
        self.assertEqual(percentile([10, 20, 30, 40], 0.5), 30)
        self.assertEqual(percentile([10, 20, 30], 0.99), 30)


class LadderTests(unittest.TestCase):
    def test_step_mean_and_skip_total(self):
        rows = [
            sample(
                "ingest-16",
                1,
                "ingest",
                enqueued=16 * 2**20,
                durable=16 * 2**20,
                phases={"limit": "rate"},
            ),
            sample(
                "ingest-16",
                2,
                "ingest",
                enqueued=8 * 2**20,
                durable=8 * 2**20,
                phases={"limit": "txreg"},
            ),
            sample("ingest-16", 1, "recall", ops=1, skips=0, lat_max_us=1000, start_delay_max_us=2000),
            sample("ingest-16", 2, "recall", ops=1, skips=2, lat_max_us=9000, start_delay_max_us=8000),
            sample("idle", 1, "ingest", enqueued=0, durable=0),
        ]
        summary = summarize_step(rows, "ingest-16")
        self.assertEqual(summary["secs"], 2)
        self.assertEqual(summary["enq_mib_s"], 12)
        self.assertEqual(summary["dur_mib_s"], 12)
        self.assertEqual(summary["skips"], 2)
        self.assertEqual(summary["limit"], {"rate": 1, "txreg": 1})
        self.assertEqual(summary["start_p99_ms"], 8)


class MinuteTests(unittest.TestCase):
    def test_bounds_are_exclusive_inclusive(self):
        self.assertEqual(minute_bounds(0, 60), (0, 60))
        self.assertEqual(minute_bounds(1, 60), (60, 120))

    def test_late_minute_is_separate_from_the_first(self):
        rows = []
        for sec in range(1, 121):
            mib = 32 if sec <= 60 else 12
            rows.append(
                sample(
                    "ingest-32",
                    sec,
                    "ingest",
                    enqueued=mib * 2**20,
                    durable=mib * 2**20,
                    pending_chunks=10 if sec <= 60 else 320,
                )
            )
            delay = 1_000 if sec <= 60 else 900_000
            rows.append(
                sample(
                    "ingest-32",
                    sec,
                    "recall",
                    ops=1,
                    start_delay_max_us=delay,
                    lat_max_us=delay,
                )
            )
        first = minute_stats(rows, "ingest-32", 0)
        second = minute_stats(rows, "ingest-32", 1)
        self.assertEqual(first["enq_mib_s"], 32)
        self.assertEqual(first["start_max_ms"], 1)
        self.assertEqual(second["enq_mib_s"], 12)
        self.assertEqual(second["dur_mib_s"], 12)
        self.assertEqual(second["start_max_ms"], 900)
        self.assertEqual(second["pending_max"], 320)
        self.assertIsNone(minute_stats(rows, "ingest-32", 2))

    def test_contiguous_minutes_have_no_gap(self):
        rows = [
            sample("s", sec, "ingest", enqueued=2**20, durable=2**20, pending_chunks=1)
            for sec in range(1, 121)
        ]
        found = list(iter_minutes(rows, "s", 60))
        self.assertEqual([item["minute"] for item in found], [1, 2])
        self.assertTrue(all(not item.get("gap") for item in found))

    def test_an_empty_minute_does_not_hide_later_minutes(self):
        rows = []
        for sec in list(range(1, 61)) + list(range(121, 181)):
            rows.append(
                sample("s", sec, "ingest", enqueued=2**20, durable=2**20, pending_chunks=1)
            )
        found = list(iter_minutes(rows, "s", 60))
        self.assertEqual([item["minute"] for item in found], [1, 2, 3])
        self.assertTrue(found[1].get("gap"))
        self.assertFalse(found[0].get("gap"))
        self.assertEqual(found[0]["secs"], 60)
        self.assertEqual(found[2]["secs"], 60)

    def test_a_leading_empty_minute_still_yields_the_later_one(self):
        rows = [
            sample("s", sec, "ingest", enqueued=2**20, durable=2**20, pending_chunks=1)
            for sec in range(61, 121)
        ]
        found = list(iter_minutes(rows, "s", 60))
        self.assertEqual([item["minute"] for item in found], [1, 2])
        self.assertTrue(found[0].get("gap"))
        self.assertFalse(found[1].get("gap"))
        self.assertEqual(found[1]["secs"], 60)


class DiskTimeTests(unittest.TestCase):
    def test_since_boot_row_is_dropped_and_device_is_filtered(self):
        lines = [
            "Device r/s rMB/s",
            iostat_line("sdc", 1, 1, 1),
            iostat_line("sdd", 50, 50, 50),
            iostat_line("sdc", 100, 16, 80, rs=35, ws=19, fs=16),
        ]
        rows = parse_iostat(lines, "sdc")
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["rmb"], 100)
        self.assertEqual(rows[0]["util"], 80)

    def test_positioning_is_busy_minus_transfer(self):
        rows = parse_iostat(
            [
                iostat_line("sdc", 0, 0, 0),
                iostat_line("sdc", 116, 16, 96, fs=16),
            ],
            "sdc",
        )
        stats = window_time(rows, recall_rate=200, region_rate=146, recall_mib=100)
        # busy = 96% * 10 = 960 ms
        # recall = 100/200*1000 = 500 ms
        # other read = 16/146*1000
        # write = 16/146*1000
        self.assertEqual(stats["n"], 1)
        self.assertAlmostEqual(stats["busy_ms"], 960)
        self.assertAlmostEqual(stats["recall_ms"], 500)
        self.assertAlmostEqual(stats["other_read_mib"], 16)
        self.assertAlmostEqual(stats["write_mib"], 16)
        expected_pos = 960 - 500 - (16 / 146 * 1000) - (16 / 146 * 1000)
        self.assertAlmostEqual(stats["positioning_ms"], expected_pos)
        self.assertAlmostEqual(stats["idle_ms"], 40)

    def test_zone_rates_change_the_split(self):
        rows = parse_iostat(
            [iostat_line("nvme", 0, 0, 0), iostat_line("nvme", 100, 0, 50)],
            "nvme",
        )
        slow = window_time(rows, recall_rate=100, region_rate=100, recall_mib=100)
        fast = window_time(rows, recall_rate=400, region_rate=100, recall_mib=100)
        self.assertAlmostEqual(slow["recall_ms"], 1000)
        self.assertAlmostEqual(fast["recall_ms"], 250)
        self.assertGreater(slow["recall_ms"], fast["recall_ms"])

    def test_short_line_is_an_error(self):
        with self.assertRaises(IostatFormatError):
            parse_iostat(["sdc 1 2 3"], "sdc")


if __name__ == "__main__":
    unittest.main()
