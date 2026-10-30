use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-mempool";

    counter CHUNKS_INGESTED("irys.mempool.chunks.ingested_total", "Chunks successfully ingested into mempool");
    counter BYTES_INGESTED("irys.mempool.chunks.bytes_ingested_total", "Total bytes ingested into mempool");
    counter DUPLICATES("irys.mempool.chunks.duplicates_total", "Duplicate chunks skipped");
    histogram VALIDATION_MS("irys.mempool.chunks.validation_duration_ms", "Chunk proof validation latency", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0]);
    histogram ENQUEUE_MS("irys.mempool.chunks.enqueue_duration_ms", "Chunk write-behind enqueue latency", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0]);
    histogram CACHE_COMMIT_MS("irys.mempool.chunks.cache_commit_duration_ms", "Chunk cache batch commit wait latency", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0]);
    counter FLUSH_FAILURES("irys.mempool.chunks.flush_failures_total", "Chunk writer flush failures");
    counter ERRORS("irys.mempool.chunks.errors_total", "Chunk processing errors by type");
    counter MODULE_WRITES_SKIPPED("irys.mempool.chunks.module_writes_skipped_total", "HTTP chunk storage-module writes left to data sync because the module was at its pending-memory ceiling");
}

pub(super) fn record_chunk_ingested(bytes: u64) {
    CHUNKS_INGESTED.add(1, &[]);
    BYTES_INGESTED.add(bytes, &[]);
}

pub(super) fn record_chunk_duplicate() {
    DUPLICATES.add(1, &[]);
}

pub(super) fn record_validation_duration(ms: f64) {
    VALIDATION_MS.record(ms, &[]);
}

pub(super) fn record_enqueue_duration(ms: f64) {
    ENQUEUE_MS.record(ms, &[]);
}

pub(super) fn record_cache_commit_duration(ms: f64) {
    CACHE_COMMIT_MS.record(ms, &[]);
}

pub(super) fn record_flush_failure() {
    FLUSH_FAILURES.add(1, &[]);
}

pub(super) fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    ERRORS.add(
        1,
        &[
            KeyValue::new("error_type", error_type),
            KeyValue::new("advisory", is_advisory),
        ],
    );
}

pub(super) fn record_module_write_skipped() {
    MODULE_WRITES_SKIPPED.add(1, &[]);
    #[cfg(test)]
    MODULE_WRITES_SKIPPED_TEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
static MODULE_WRITES_SKIPPED_TEST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Process-wide count of [`record_module_write_skipped`] calls.
///
/// Crate tests share one process. Compare a delta, not an absolute total.
#[cfg(test)]
pub(super) fn test_module_writes_skipped() -> u64 {
    MODULE_WRITES_SKIPPED_TEST.load(std::sync::atomic::Ordering::Relaxed)
}
