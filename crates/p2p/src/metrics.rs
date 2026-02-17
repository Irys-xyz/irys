use opentelemetry::metrics::Counter;
use opentelemetry::{global, KeyValue};
use std::sync::OnceLock;

fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-p2p")
}

static CHUNKS_RECEIVED: OnceLock<Counter<u64>> = OnceLock::new();
static BYTES_RECEIVED: OnceLock<Counter<u64>> = OnceLock::new();
static INBOUND_ERRORS: OnceLock<Counter<u64>> = OnceLock::new();
static OUTBOUND_ERRORS: OnceLock<Counter<u64>> = OnceLock::new();

pub(crate) fn record_gossip_chunk_received(bytes: u64) {
    let chunks = CHUNKS_RECEIVED.get_or_init(|| {
        meter()
            .u64_counter("irys.gossip.chunks.received_total")
            .with_description("Total chunks received via gossip")
            .build()
    });
    let bytes_counter = BYTES_RECEIVED.get_or_init(|| {
        meter()
            .u64_counter("irys.gossip.chunks.bytes_received_total")
            .with_description("Total bytes received in gossip chunk payloads")
            .build()
    });
    chunks.add(1, &[]);
    bytes_counter.add(bytes, &[]);
}

pub(crate) fn record_gossip_inbound_error(error_type: &'static str, is_advisory: bool) {
    INBOUND_ERRORS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.gossip.inbound.errors_total")
                .with_description("Gossip inbound processing errors by type")
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

pub(crate) fn record_gossip_outbound_error(error_type: &'static str) {
    OUTBOUND_ERRORS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.gossip.outbound.errors_total")
                .with_description("Gossip outbound send errors by type")
                .build()
        })
        .add(1, &[KeyValue::new("error_type", error_type)]);
}
