use opentelemetry::metrics::Counter;
use opentelemetry::{global, KeyValue};
use std::sync::OnceLock;

fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-api-server")
}

static CHUNKS_RECEIVED: OnceLock<Counter<u64>> = OnceLock::new();
static BYTES_RECEIVED: OnceLock<Counter<u64>> = OnceLock::new();
static CHUNK_ERRORS: OnceLock<Counter<u64>> = OnceLock::new();

pub fn record_chunk_received(bytes: u64) {
    let chunks = CHUNKS_RECEIVED.get_or_init(|| {
        meter()
            .u64_counter("irys.api.chunks.received_total")
            .with_description("Total chunks received via API")
            .build()
    });
    let bytes_counter = BYTES_RECEIVED.get_or_init(|| {
        meter()
            .u64_counter("irys.api.chunks.bytes_received_total")
            .with_description("Total bytes received in chunk payloads")
            .build()
    });
    chunks.add(1, &[]);
    bytes_counter.add(bytes, &[]);
}

pub fn record_chunk_error(error_type: &'static str, is_advisory: bool) {
    CHUNK_ERRORS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.api.chunks.errors_total")
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
