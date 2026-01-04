use opentelemetry::{global, KeyValue};

// Called each time to ensure we get the real meter after telemetry init.
fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-mempool")
}

pub fn record_chunk_ingested(bytes: u64) {
    let m = meter();
    m.u64_counter("irys.mempool.chunks.ingested_total")
        .with_description("Chunks successfully ingested into mempool")
        .build()
        .add(1, &[]);
    m.u64_counter("irys.mempool.chunks.bytes_ingested_total")
        .with_description("Total bytes ingested into mempool")
        .build()
        .add(bytes, &[]);
}

pub fn record_chunk_duplicate() {
    meter()
        .u64_counter("irys.mempool.chunks.duplicates_total")
        .with_description("Duplicate chunks skipped")
        .build()
        .add(1, &[]);
}

pub fn record_validation_duration(ms: f64) {
    meter()
        .f64_histogram("irys.mempool.chunks.validation_duration_ms")
        .with_description("Chunk proof validation latency")
        .build()
        .record(ms, &[]);
}

pub fn record_storage_duration(ms: f64) {
    meter()
        .f64_histogram("irys.mempool.chunks.storage_duration_ms")
        .with_description("Chunk database storage latency")
        .build()
        .record(ms, &[]);
}

pub fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    meter()
        .u64_counter("irys.mempool.chunks.errors_total")
        .with_description("Chunk processing errors by type")
        .build()
        .add(
            1,
            &[
                KeyValue::new("error_type", error_type),
                KeyValue::new("advisory", is_advisory.to_string()),
            ],
        );
}
