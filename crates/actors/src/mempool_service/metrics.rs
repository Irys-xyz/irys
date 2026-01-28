use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{global, KeyValue};
use std::sync::OnceLock;

fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-mempool")
}

static CHUNKS_INGESTED: OnceLock<Counter<u64>> = OnceLock::new();
static BYTES_INGESTED: OnceLock<Counter<u64>> = OnceLock::new();
static DUPLICATES: OnceLock<Counter<u64>> = OnceLock::new();
static VALIDATION_MS: OnceLock<Histogram<f64>> = OnceLock::new();
static STORAGE_MS: OnceLock<Histogram<f64>> = OnceLock::new();
static ERRORS: OnceLock<Counter<u64>> = OnceLock::new();

pub(super) fn record_chunk_ingested(bytes: u64) {
    let chunks = CHUNKS_INGESTED.get_or_init(|| {
        meter()
            .u64_counter("irys.mempool.chunks.ingested_total")
            .with_description("Chunks successfully ingested into mempool")
            .build()
    });
    let bytes_counter = BYTES_INGESTED.get_or_init(|| {
        meter()
            .u64_counter("irys.mempool.chunks.bytes_ingested_total")
            .with_description("Total bytes ingested into mempool")
            .build()
    });
    chunks.add(1, &[]);
    bytes_counter.add(bytes, &[]);
}

pub(super) fn record_chunk_duplicate() {
    DUPLICATES
        .get_or_init(|| {
            meter()
                .u64_counter("irys.mempool.chunks.duplicates_total")
                .with_description("Duplicate chunks skipped")
                .build()
        })
        .add(1, &[]);
}

pub(super) fn record_validation_duration(ms: f64) {
    VALIDATION_MS
        .get_or_init(|| {
            meter()
                .f64_histogram("irys.mempool.chunks.validation_duration_ms")
                .with_description("Chunk proof validation latency")
                .build()
        })
        .record(ms, &[]);
}

pub(super) fn record_storage_duration(ms: f64) {
    STORAGE_MS
        .get_or_init(|| {
            meter()
                .f64_histogram("irys.mempool.chunks.storage_duration_ms")
                .with_description("Chunk database storage latency")
                .build()
        })
        .record(ms, &[]);
}

pub(super) fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    ERRORS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.mempool.chunks.errors_total")
                .with_description("Chunk processing errors by type")
                .build()
        })
        .add(
            1,
            &[
                KeyValue::new("error_type", error_type),
                KeyValue::new("advisory", is_advisory),
            ],
        );
}
