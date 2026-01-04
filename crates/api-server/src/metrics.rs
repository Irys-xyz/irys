use opentelemetry::{global, KeyValue};

// Called each time to ensure we get the real meter after telemetry init.
fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-api-server")
}

pub fn record_chunk_received(bytes: u64) {
    let m = meter();
    m.u64_counter("irys.api.chunks.received_total")
        .with_description("Total chunks received via API")
        .build()
        .add(1, &[]);
    m.u64_counter("irys.api.chunks.bytes_received_total")
        .with_description("Total bytes received in chunk payloads")
        .build()
        .add(bytes, &[]);
}

pub fn record_chunk_processing_duration(ms: f64) {
    meter()
        .f64_histogram("irys.api.chunks.processing_duration_ms")
        .with_description("Chunk processing latency in milliseconds")
        .build()
        .record(ms, &[]);
}

pub fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    meter()
        .u64_counter("irys.api.chunks.errors_total")
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
