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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_cache_commit_duration_does_not_panic() {
        // OTel exposes no in-process read; exercise the call path only.
        record_cache_commit_duration(0.0);
        record_cache_commit_duration(42.5);
    }
}
