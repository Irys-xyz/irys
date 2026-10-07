//! Per-second counters. The sampler takes a bucket once a second.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Instant;

use crate::schedule::{SecondView, percentile};

/// One call's latency samples. Totals are the sum of the calls, so overlapping
/// workers can add up to more than the step.
#[derive(Debug, Default, Clone)]
pub(crate) struct Phase {
    pub count: u64,
    pub total_us: u64,
    pub samples_us: Vec<u64>,
}

impl Phase {
    pub(crate) fn push(&mut self, micros: u64) {
        self.count = self.count.saturating_add(1);
        self.total_us = self.total_us.saturating_add(micros);
        self.samples_us.push(micros);
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DriverBucket {
    pub ops: u64,
    pub bytes: u64,
    pub skips: u64,
    pub lats_us: Vec<u64>,
    pub delays_us: Vec<u64>,
    pub enqueued: u64,
    pub durable: u64,
    /// Wall time this driver waited before it could hand bytes to the module.
    pub enqueue_blocked_us: u64,
    /// Wall time the generator was past its release deadline and not asleep.
    pub pace_behind_us: u64,
    /// Wall time the generator slept because the next release was not due.
    pub pace_sleep_us: u64,
    /// Bytes the pacer released. This is not `rate * step`.
    pub pace_released: u64,
    /// `index_transaction_data`, including the wait for the index lock.
    pub txreg: Phase,
    /// Generator time blocked on the index result. Overlap mode waits only
    /// for the leftover; the call itself is still in `txreg`.
    pub txreg_wait_us: u64,
    /// `settle_pending_body_migration`. Does not block the generator.
    pub settle: Phase,
    /// Ingest: `write_data_chunk_queued` until ACK. Migration: one deposit call.
    pub enqueue_lat: Phase,
    /// After ACK or deposit return, until `is_data_chunk_durable_at`.
    pub durable_wait: Phase,
    /// Generator time building the synthetic header, path, and body.
    pub gen_us: u64,
    /// Generator time waiting for the ingest semaphore.
    pub permit_wait_us: u64,
    pub permit_max_in_use: u64,
    /// `avg_in_use_milli * elapsed_us`, so seconds merge without averaging averages.
    pub permit_area: u128,
    pub permit_elapsed_us: u64,
}

impl DriverBucket {
    pub(crate) fn add_op(&mut self, bytes: u64, lat_us: u64) {
        self.ops = self.ops.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.lats_us.push(lat_us);
    }
}

#[derive(Debug, Default)]
pub(crate) struct Buckets {
    pub recall: DriverBucket,
    pub ingest: DriverBucket,
    pub migration: DriverBucket,
    pub serve: DriverBucket,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct StepAccum {
    pub seconds: u64,
    pub recall: DriverBucket,
    pub ingest: DriverBucket,
    pub migration: DriverBucket,
    pub serve: DriverBucket,
    pub pending_samples: Vec<u64>,
    pub overloaded_secs: u64,
    /// One entry per scored second. The drain is not included.
    pub views: Vec<SecondView>,
}

impl StepAccum {
    pub(crate) fn add_second(
        &mut self,
        recall: &DriverBucket,
        ingest: &DriverBucket,
        migration: &DriverBucket,
        serve: &DriverBucket,
        pending_bytes: u64,
        overloaded: bool,
    ) {
        self.seconds = self.seconds.saturating_add(1);
        merge(&mut self.recall, recall);
        merge(&mut self.ingest, ingest);
        merge(&mut self.migration, migration);
        merge(&mut self.serve, serve);
        self.pending_samples.push(pending_bytes);
        if overloaded {
            self.overloaded_secs = self.overloaded_secs.saturating_add(1);
        }
        self.views.push(second_view(
            recall,
            ingest,
            migration,
            pending_bytes,
            overloaded,
        ));
    }

    pub(crate) fn add_permit_sample(&mut self, sample: &PermitSample) {
        fold_permit(&mut self.ingest, sample);
    }

    /// Fold completions that landed after the step clock stopped.
    /// Those bytes count toward durable. They do not add a scored second.
    pub(crate) fn add_quiet(&mut self, buckets: &Buckets) {
        merge(&mut self.recall, &buckets.recall);
        merge(&mut self.ingest, &buckets.ingest);
        merge(&mut self.migration, &buckets.migration);
        merge(&mut self.serve, &buckets.serve);
    }
}

impl Buckets {
    pub(crate) fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

fn second_view(
    recall: &DriverBucket,
    ingest: &DriverBucket,
    migration: &DriverBucket,
    pending_bytes: u64,
    overloaded: bool,
) -> SecondView {
    SecondView {
        recall_ops: recall.ops,
        recall_skips: recall.skips,
        recall_start_us: recall.delays_us.clone(),
        recall_lat_us: recall.lats_us.clone(),
        ingest_enqueued: ingest.enqueued,
        ingest_durable: ingest.durable,
        ingest_blocked_us: ingest.enqueue_blocked_us,
        migration_enqueued: migration.enqueued,
        migration_durable: migration.durable,
        migration_blocked_us: migration.enqueue_blocked_us,
        pending_bytes,
        overloaded,
    }
}

fn merge_phase(into: &mut Phase, from: &Phase) {
    into.count = into.count.saturating_add(from.count);
    into.total_us = into.total_us.saturating_add(from.total_us);
    into.samples_us.extend_from_slice(&from.samples_us);
}

fn merge(into: &mut DriverBucket, from: &DriverBucket) {
    into.ops = into.ops.saturating_add(from.ops);
    into.bytes = into.bytes.saturating_add(from.bytes);
    into.skips = into.skips.saturating_add(from.skips);
    into.enqueued = into.enqueued.saturating_add(from.enqueued);
    into.durable = into.durable.saturating_add(from.durable);
    into.enqueue_blocked_us = into
        .enqueue_blocked_us
        .saturating_add(from.enqueue_blocked_us);
    into.pace_behind_us = into.pace_behind_us.saturating_add(from.pace_behind_us);
    into.pace_sleep_us = into.pace_sleep_us.saturating_add(from.pace_sleep_us);
    into.pace_released = into.pace_released.saturating_add(from.pace_released);
    merge_phase(&mut into.txreg, &from.txreg);
    into.txreg_wait_us = into.txreg_wait_us.saturating_add(from.txreg_wait_us);
    merge_phase(&mut into.settle, &from.settle);
    merge_phase(&mut into.enqueue_lat, &from.enqueue_lat);
    merge_phase(&mut into.durable_wait, &from.durable_wait);
    into.gen_us = into.gen_us.saturating_add(from.gen_us);
    into.permit_wait_us = into.permit_wait_us.saturating_add(from.permit_wait_us);
    into.permit_max_in_use = into.permit_max_in_use.max(from.permit_max_in_use);
    into.permit_area = into.permit_area.saturating_add(from.permit_area);
    into.permit_elapsed_us = into
        .permit_elapsed_us
        .saturating_add(from.permit_elapsed_us);
    into.lats_us.extend_from_slice(&from.lats_us);
    into.delays_us.extend_from_slice(&from.delays_us);
}

/// Time-weighted ingest semaphore occupancy. `avg_in_use_milli` is the
/// average permit count times 1000.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PermitSample {
    pub avg_in_use_milli: u64,
    pub max_in_use: u64,
    pub elapsed_us: u64,
}

#[derive(Debug)]
struct PermitInner {
    in_use: u64,
    max_in_use: u64,
    at: Instant,
    window_start: Instant,
    area_us: u128,
}

impl PermitInner {
    fn touch(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.at).as_micros();
        self.area_us = self
            .area_us
            .saturating_add(u128::from(self.in_use).saturating_mul(dt));
        self.at = now;
    }
}

/// Ingest permits in use. Acquire and release are one per chunk.
#[derive(Debug)]
pub(crate) struct PermitGauge {
    inner: Mutex<PermitInner>,
}

impl PermitGauge {
    pub(crate) fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            inner: Mutex::new(PermitInner {
                in_use: 0,
                max_in_use: 0,
                at: now,
                window_start: now,
                area_us: 0,
            }),
        }
    }

    pub(crate) fn acquire(&self) {
        self.change_at(Instant::now(), 1);
    }

    pub(crate) fn release(&self) {
        self.change_at(Instant::now(), -1);
    }

    pub(crate) fn sample(&self) -> PermitSample {
        self.sample_at(Instant::now())
    }

    /// Drop the open window. In-flight permits stay in use.
    pub(crate) fn reset_window(&self) {
        let _ = self.sample();
    }

    fn change_at(&self, now: Instant, delta: i64) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.touch(now);
        if delta >= 0 {
            guard.in_use = guard.in_use.saturating_add(delta as u64);
        } else {
            guard.in_use = guard.in_use.saturating_sub(delta.unsigned_abs());
        }
        guard.max_in_use = guard.max_in_use.max(guard.in_use);
    }

    fn sample_at(&self, now: Instant) -> PermitSample {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.touch(now);
        let elapsed = now
            .saturating_duration_since(guard.window_start)
            .as_micros();
        let area = guard.area_us;
        guard.area_us = 0;
        guard.window_start = now;
        let max = guard.max_in_use;
        guard.max_in_use = guard.in_use;
        let avg = match area.saturating_mul(1000).checked_div(elapsed) {
            Some(value) => u64::try_from(value).unwrap_or(u64::MAX),
            None => guard.in_use.saturating_mul(1000),
        };
        PermitSample {
            avg_in_use_milli: avg,
            max_in_use: max,
            elapsed_us: u64::try_from(elapsed).unwrap_or(u64::MAX),
        }
    }
}

pub(crate) fn fold_permit(bucket: &mut DriverBucket, sample: &PermitSample) {
    bucket.permit_max_in_use = bucket.permit_max_in_use.max(sample.max_in_use);
    let term = u128::from(sample.avg_in_use_milli).saturating_mul(u128::from(sample.elapsed_us));
    bucket.permit_area = bucket.permit_area.saturating_add(term);
    bucket.permit_elapsed_us = bucket.permit_elapsed_us.saturating_add(sample.elapsed_us);
}

pub(crate) fn permit_avg_milli(bucket: &DriverBucket) -> u64 {
    if bucket.permit_elapsed_us == 0 {
        0
    } else {
        u64::try_from(bucket.permit_area / u128::from(bucket.permit_elapsed_us)).unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LatSummary {
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
}

pub(crate) fn summarize(samples: &[u64]) -> LatSummary {
    LatSummary {
        p50: percentile(samples, 50),
        p99: percentile(samples, 99),
        max: samples.iter().copied().max().unwrap_or(0),
    }
}

/// Durable offsets. Random serve indexes `order`. Sequential serve walks `by_offset`.
#[derive(Debug, Default)]
pub(crate) struct Written {
    order: Vec<u32>,
    by_offset: BTreeSet<u32>,
}

impl Written {
    pub(crate) fn insert(&mut self, offset: u32) {
        if self.by_offset.insert(offset) {
            self.order.push(offset);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub(crate) fn get(&self, index: usize) -> Option<u32> {
        self.order.get(index).copied()
    }

    /// Next contiguous Data run at or after `cursor`, at most `max_chunks` long.
    pub(crate) fn run_from(&self, cursor: u32, max_chunks: u32, end: u32) -> Option<(u32, u32)> {
        let start = *self.by_offset.range(cursor..end).next()?;
        let mut last = start;
        let limit = start.saturating_add(max_chunks.saturating_sub(1));
        while last < limit {
            let next = last.saturating_add(1);
            if next >= end || !self.by_offset.contains(&next) {
                break;
            }
            last = next;
        }
        Some((start, last))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn permit_average_is_time_weighted() {
        let start = Instant::now();
        let gauge = PermitGauge::new_at(start);
        gauge.change_at(start, 1);
        gauge.change_at(start + Duration::from_secs(1), 1);
        let sample = gauge.sample_at(start + Duration::from_secs(2));
        assert_eq!(sample.max_in_use, 2);
        assert_eq!(sample.elapsed_us, 2_000_000);
        assert_eq!(sample.avg_in_use_milli, 1500);
        let mut bucket = DriverBucket::default();
        fold_permit(&mut bucket, &sample);
        assert_eq!(permit_avg_milli(&bucket), 1500);
        assert_eq!(bucket.permit_max_in_use, 2);
    }

    #[test]
    fn merge_keeps_phase_totals_and_permit_max() {
        let mut into = DriverBucket::default();
        let mut from = DriverBucket::default();
        into.txreg.push(10);
        from.txreg.push(30);
        from.pace_released = 5;
        from.permit_max_in_use = 7;
        from.permit_area = 3_000;
        from.permit_elapsed_us = 2;
        merge(&mut into, &from);
        assert_eq!(into.txreg.count, 2);
        assert_eq!(into.txreg.total_us, 40);
        assert_eq!(into.pace_released, 5);
        assert_eq!(into.permit_max_in_use, 7);
        assert_eq!(permit_avg_milli(&into), 1500);
    }
}
