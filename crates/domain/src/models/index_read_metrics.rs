//! Submodule index lookups, labelled by the caller that opened the view.
//!
//! One `data_path` count is one hash-keyed page get. A cursor walk is one
//! count, not one count per row. The global meter is a no-op until telemetry
//! installs a provider.

use std::{cell::Cell, sync::LazyLock};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::{KeyValue, global, metrics::Counter};

pub(crate) const SERVE_LEDGER: &str = "serve_ledger";
pub(crate) const SERVE_DATA_ROOT: &str = "serve_data_root";
pub(crate) const SERVE_SPAN: &str = "serve_span";
pub(crate) const RECALL: &str = "recall";
pub const MIGRATION: &str = "migration";
pub(crate) const METADATA: &str = "metadata";
/// Placement of a chunk body onto entropy offsets.
pub const PLACEMENT: &str = "placement";
/// Cache reclamation asking which bodies are durable.
pub const CACHE_RECLAIM: &str = "cache_reclaim";
/// Data sync resolving a data root for a residual hole.
pub const DATA_SYNC: &str = "data_sync";
/// Index heal probing whether a placement is ready.
pub const INDEX_HEAL: &str = "index_heal";

thread_local! {
    static CALLER: Cell<&'static str> = const { Cell::new("unknown") };
}

struct Reset {
    previous: &'static str,
}

impl Drop for Reset {
    fn drop(&mut self) {
        CALLER.with(|cell| cell.set(self.previous));
    }
}

pub(crate) fn with_caller<T>(caller: &'static str, body: impl FnOnce() -> T) -> T {
    let previous = CALLER.with(|cell| cell.replace(caller));
    let _reset = Reset { previous };
    body()
}

/// Set the index caller on this thread. A rayon worker does not see the
/// caller's thread-local, so the worker closure calls this itself.
pub fn with_index_caller<T>(caller: &'static str, body: impl FnOnce() -> T) -> T {
    with_caller(caller, body)
}

/// One debug span per operation, named `index_read`, with the caller as a field.
/// The counters stay at the call. Page gets under the span keep that caller.
/// Do not call this inside a rayon worker: the span would have no parent and
/// the export would be one trace per get.
pub fn trace_index_read<T>(caller: &'static str, body: impl FnOnce() -> T) -> T {
    let span = tracing::debug_span!("index_read", caller);
    let _enter = span.enter();
    with_caller(caller, body)
}

fn current() -> &'static str {
    CALLER.with(Cell::get)
}

/// Caller label installed by [`with_caller`]. Chunk reads that are not a
/// mining recall use this so the crc counters share the serve label.
pub(crate) fn caller() -> &'static str {
    current()
}

static READS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.index_reads_total")
        .with_description(
            "Submodule index lookups by caller and kind. data_path, tx_path, tx_leaf, and data_root are individual gets. offset_walk is one cursor, not one row.",
        )
        .build()
});

static VIEWS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.index_views_total")
        .with_description("Submodule read views opened for an offset span, by caller")
        .build()
});

static SPAN_OFFSETS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.index_span_offsets_total")
        .with_description("Partition offsets covered by submodule index views, by caller")
        .build()
});

pub(crate) fn note(kind: &'static str) {
    READS.add(
        1,
        &[
            KeyValue::new("caller", current()),
            KeyValue::new("kind", kind),
        ],
    );
}

pub(crate) fn note_view(offsets: u64) {
    let caller = current();
    VIEWS.add(1, &[KeyValue::new("caller", caller)]);
    SPAN_OFFSETS.add(offsets, &[KeyValue::new("caller", caller)]);
}

static PRESENCE_RECONCILED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.presence_rows_reconciled_total")
        .with_description("Presence rows removed at open because their offset interval is not Data")
        .build()
});

pub(crate) fn note_presence_reconciled(rows: u64) {
    PRESENCE_RECONCILED.add(rows, &[]);
}

static UNCOVERED_DATA_PATH: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.uncovered_data_path_total")
        .with_description("Data-path proofs not stored because no interval covers the offset")
        .build()
});

#[cfg(test)]
static UNCOVERED_DATA_PATH_TEST: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_uncovered_data_path(rows: u64) {
    UNCOVERED_DATA_PATH.add(rows, &[]);
    #[cfg(test)]
    {
        UNCOVERED_DATA_PATH_TEST.fetch_add(rows, Ordering::Relaxed);
    }
}

static BAD_LEAF_OFFSET: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.bad_leaf_offset_total")
        .with_description(
            "Stored data paths skipped because the leaf offset is not a u32 chunk index",
        )
        .build()
});

#[cfg(test)]
static BAD_LEAF_OFFSET_TEST: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_bad_leaf_offset() {
    BAD_LEAF_OFFSET.add(1, &[]);
    #[cfg(test)]
    {
        BAD_LEAF_OFFSET_TEST.fetch_add(1, Ordering::Relaxed);
    }
}

/// Bad leaf offsets skipped since process start. Tests assert a delta.
#[cfg(test)]
pub(crate) fn bad_leaf_offset_for_test() -> u64 {
    BAD_LEAF_OFFSET_TEST.load(Ordering::Relaxed)
}

/// Proofs dropped since process start. Tests assert a delta. Parallel tests
/// can add to the same counter.
#[cfg(test)]
pub(crate) fn uncovered_data_path_for_test() -> u64 {
    UNCOVERED_DATA_PATH_TEST.load(Ordering::Relaxed)
}

static DURABLE_POINTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.durable_points_total")
        .with_description(
            "Lane durable points by timing and cause. immediate did not wait out min_durable_interval_ms. deferred waited for that interval. cause is forced, ceiling, headroom, or interval.",
        )
        .build()
});

pub(super) fn note_durable_point(timing: &'static str, cause: &'static str) {
    DURABLE_POINTS.add(
        1,
        &[
            KeyValue::new("timing", timing),
            KeyValue::new("cause", cause),
        ],
    );
}

static CHUNK_FSYNC_FAILURES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.chunk_fsync_failures_total")
        .with_description(
            "chunks.dat fdatasync failures. Parked bytes are queued for another pwrite before the next durable point.",
        )
        .build()
});

#[cfg(test)]
static CHUNK_FSYNC_FAILURES_TEST: AtomicU64 = AtomicU64::new(0);

pub(super) fn note_chunk_fsync_failure() {
    CHUNK_FSYNC_FAILURES.add(1, &[]);
    #[cfg(test)]
    {
        CHUNK_FSYNC_FAILURES_TEST.fetch_add(1, Ordering::Relaxed);
    }
}

/// `chunks.dat` fdatasync failures since process start. Tests assert a delta.
#[cfg(test)]
pub(crate) fn chunk_fsync_failures_for_test() -> u64 {
    CHUNK_FSYNC_FAILURES_TEST.load(Ordering::Relaxed)
}

static RECALL_IO: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.recall_reads_total")
        .with_description(
            "Mining recall preads of chunks.dat by mode. direct uses the O_DIRECT descriptor. buffered is the normal descriptor when storage.recall_direct_io is false. fallback asked for O_DIRECT and used the buffered descriptor because the open failed or the range was not 4096-byte aligned. One count is one pread.",
        )
        .build()
});

pub(super) fn note_recall_io(mode: &'static str) {
    RECALL_IO.add(1, &[KeyValue::new("mode", mode)]);
}

static RECALL_PREAD_US: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.recall_pread_us_total")
        .with_description(
            "Wall time of mining recall pread batches, in microseconds. One sample is one parallel batch, not one pread. Subtract from disk_idle_us_total{reason=recall_hold}: that sleep covers the preads and the gaps between them.",
        )
        .build()
});

/// Wall time of one parallel recall-pread batch.
pub(super) fn note_recall_pread(elapsed: std::time::Duration) {
    let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    RECALL_PREAD_US.add(us, &[]);
}

static ENTROPY_SPANS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_spans_total")
        .with_description("Entropy preads issued. One count is one span.")
        .build()
});

static ENTROPY_SPAN_BYTES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_span_bytes_total")
        .with_description(
            "Bytes of entropy preads issued, holes included. Divide by entropy_spans_total for the mean span.",
        )
        .build()
});

static ENTROPY_SPAN_SLOTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_span_slots_total")
        .with_description(
            "Queued chunks served by entropy preads. A hole is not a slot. Divide by entropy_spans_total for the mean.",
        )
        .build()
});

static ENTROPY_SPAN_READS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_span_reads_total")
        .with_description(
            "Entropy preads by pread size. bytes is 256KiB, 512KiB, 1MiB, 2MiB, 4MiB, 8MiB, or 16MiB+.",
        )
        .build()
});

static ENTROPY_SPAN_SLOT_READS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_span_slot_reads_total")
        .with_description(
            "Entropy preads by queued chunks served. slots is 1, 2, 3-4, 5-8, 9-16, 17-32, or 33+.",
        )
        .build()
});

static READAHEAD_STORES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_readahead_stores_total")
        .with_description(
            "Entropy pads stored after a pread that read past its queued chunks. One count is one chunk.",
        )
        .build()
});

static READAHEAD_HITS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.entropy_readahead_hits_total")
        .with_description(
            "Sweep slots packed from the entropy read-ahead cache. One count is one chunk that did not read chunks.dat.",
        )
        .build()
});

/// One pad kept from a read-ahead pread.
pub(super) fn note_readahead_store() {
    READAHEAD_STORES.add(1, &[]);
}

/// One sweep slot packed from a cached pad. A pad that was taken and then rejected is not counted.
pub(super) fn note_readahead_hit() {
    READAHEAD_HITS.add(1, &[]);
}

/// One entropy pread about to be issued. `slots` is the queued chunks it
/// serves. `bytes` is the pread, including a hole.
pub(super) fn note_entropy_pread(slots: u64, bytes: u64) {
    ENTROPY_SPANS.add(1, &[]);
    ENTROPY_SPAN_BYTES.add(bytes, &[]);
    ENTROPY_SPAN_SLOTS.add(slots, &[]);
    ENTROPY_SPAN_READS.add(1, &[KeyValue::new("bytes", span_bytes_bucket(bytes))]);
    ENTROPY_SPAN_SLOT_READS.add(1, &[KeyValue::new("slots", span_slots_bucket(slots))]);
}

fn span_bytes_bucket(bytes: u64) -> &'static str {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes <= 256 * KIB {
        "256KiB"
    } else if bytes <= 512 * KIB {
        "512KiB"
    } else if bytes <= MIB {
        "1MiB"
    } else if bytes <= 2 * MIB {
        "2MiB"
    } else if bytes <= 4 * MIB {
        "4MiB"
    } else if bytes <= 8 * MIB {
        "8MiB"
    } else {
        "16MiB+"
    }
}

fn span_slots_bucket(slots: u64) -> &'static str {
    match slots {
        0 | 1 => "1",
        2 => "2",
        3 | 4 => "3-4",
        5..=8 => "5-8",
        9..=16 => "9-16",
        17..=32 => "17-32",
        _ => "33+",
    }
}

static DISK_IDLE: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.disk_idle_total")
        .with_description(
            "Lane sleeps with no chunk command in the kernel. reason is nothing_queued, read_grace, recall_hold, packed_write_hold, exclusive_io, inflight_budget, sync_in_progress, or durable_point. recall_hold includes mining pread time; see recall_pread_us_total.",
        )
        .build()
});

static DISK_IDLE_US: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.disk_idle_us_total")
        .with_description(
            "Microseconds of irys.storage.disk_idle_total. durable_point also counts the presence-commit slice after fdatasync, while the lane holds no chunk command.",
        )
        .build()
});

/// One stretch where the lane issued no chunk command and the chunk file was
/// not held for a syscall.
pub(super) fn note_disk_idle(reason: &'static str, elapsed: std::time::Duration) {
    let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    let labels = [KeyValue::new("reason", reason)];
    DISK_IDLE.add(1, &labels);
    DISK_IDLE_US.add(us, &labels);
}

static INGEST_STAGE_MS: LazyLock<opentelemetry::metrics::Histogram<f64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .f64_histogram("irys.storage.ingest_stage_ms")
        .with_description(
            "Ingress chunk latency in milliseconds. queue_to_entropy is sweep enqueue until the entropy pread returns. entropy_to_packed is the XOR and the packed-queue insert. packed_to_pwrite is the packed queue until the pwrite returns. pwrite_to_durable is that return until the durable point drops the offset.",
        )
        .with_boundaries(vec![
            1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0,
        ])
        .build()
});

/// One ingress stage. Other priorities are not recorded.
pub(super) fn note_ingest_stage(stage: &'static str, elapsed: std::time::Duration) {
    let ms = elapsed.as_secs_f64() * 1_000.0;
    INGEST_STAGE_MS.record(ms, &[KeyValue::new("stage", stage)]);
}

#[cfg(test)]
mod tests {
    use super::{CACHE_RECLAIM, DATA_SYNC, PLACEMENT, current, with_caller};

    #[test]
    fn caller_restores_after_nesting() {
        assert_eq!(current(), "unknown");
        with_caller(CACHE_RECLAIM, || {
            assert_eq!(current(), CACHE_RECLAIM);
            with_caller(DATA_SYNC, || assert_eq!(current(), DATA_SYNC));
            assert_eq!(current(), CACHE_RECLAIM);
        });
        assert_eq!(current(), "unknown");
    }

    #[test]
    fn caller_does_not_cross_threads() {
        let seen = std::thread::scope(|scope| {
            with_caller(PLACEMENT, || scope.spawn(current).join().unwrap())
        });
        assert_eq!(seen, "unknown");
        let seen = std::thread::scope(|scope| {
            scope
                .spawn(|| with_caller(PLACEMENT, current))
                .join()
                .unwrap()
        });
        assert_eq!(seen, PLACEMENT);
    }

    #[test]
    fn entropy_span_buckets_split_one_chunk_from_a_run() {
        assert_eq!(super::span_bytes_bucket(256 * 1024), "256KiB");
        assert_eq!(super::span_bytes_bucket(256 * 1024 + 1), "512KiB");
        assert_eq!(super::span_bytes_bucket(4 * 1024 * 1024), "4MiB");
        assert_eq!(super::span_bytes_bucket(8 * 1024 * 1024 + 1), "16MiB+");
        assert_eq!(super::span_slots_bucket(1), "1");
        assert_eq!(super::span_slots_bucket(2), "2");
        assert_eq!(super::span_slots_bucket(4), "3-4");
        assert_eq!(super::span_slots_bucket(33), "33+");
    }
}
