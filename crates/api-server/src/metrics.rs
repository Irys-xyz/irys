use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-api-server";

    counter CHUNKS_RECEIVED("irys.api.chunks.received_total", "Total chunks received via API");
    counter BYTES_RECEIVED("irys.api.chunks.bytes_received_total", "Total bytes received in chunk payloads");
    counter CHUNK_ERRORS("irys.api.chunks.errors_total", "Chunk processing errors by type");
    histogram CHUNK_PROCESSING_MS("irys.api.chunks.processing_duration_ms", "Chunk processing latency in milliseconds", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]);
}

pub fn record_chunk_received(bytes: u64) {
    CHUNKS_RECEIVED.add(1, &[]);
    BYTES_RECEIVED.add(bytes, &[]);
}

pub fn record_chunk_processing_duration(ms: f64) {
    CHUNK_PROCESSING_MS.record(ms, &[]);
}

pub fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    CHUNK_ERRORS.add(
        1,
        &[
            KeyValue::new("error_type", error_type),
            KeyValue::new("advisory", if is_advisory { "true" } else { "false" }),
        ],
    );
}
