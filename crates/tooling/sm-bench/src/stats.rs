//! Per-second counters. The sampler takes a bucket once a second.

use std::collections::BTreeSet;

use crate::schedule::percentile;

#[derive(Debug, Default, Clone)]
pub(crate) struct DriverBucket {
    pub ops: u64,
    pub bytes: u64,
    pub skips: u64,
    pub lats_us: Vec<u64>,
    pub delays_us: Vec<u64>,
    pub enqueued: u64,
    pub durable: u64,
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

fn merge(into: &mut DriverBucket, from: &DriverBucket) {
    into.ops = into.ops.saturating_add(from.ops);
    into.bytes = into.bytes.saturating_add(from.bytes);
    into.skips = into.skips.saturating_add(from.skips);
    into.enqueued = into.enqueued.saturating_add(from.enqueued);
    into.durable = into.durable.saturating_add(from.durable);
    into.lats_us.extend_from_slice(&from.lats_us);
    into.delays_us.extend_from_slice(&from.delays_us);
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
