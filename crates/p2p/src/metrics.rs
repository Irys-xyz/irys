use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-p2p";

    counter CHUNKS_RECEIVED("irys.gossip.chunks.received_total", "Total chunks received via gossip");
    counter BYTES_RECEIVED("irys.gossip.chunks.bytes_received_total", "Total bytes received in gossip chunk payloads");
    histogram PROCESSING_MS("irys.gossip.chunks.processing_duration_ms", "Gossip chunk processing latency in milliseconds", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]);
    counter INBOUND_ERRORS("irys.gossip.inbound.errors_total", "Gossip inbound processing errors by type");
    counter OUTBOUND_ERRORS("irys.gossip.outbound.errors_total", "Gossip outbound send errors by type");
}

pub(crate) fn record_gossip_chunk_received(bytes: u64) {
    CHUNKS_RECEIVED.add(1, &[]);
    BYTES_RECEIVED.add(bytes, &[]);
}

pub(crate) fn record_gossip_chunk_processing_duration(ms: f64) {
    PROCESSING_MS.record(ms, &[]);
}

pub(crate) fn record_gossip_inbound_error(error_type: &'static str, is_advisory: bool) {
    INBOUND_ERRORS.add(
        1,
        &[
            KeyValue::new("error_type", error_type),
            KeyValue::new("advisory", is_advisory),
        ],
    );
}

pub(crate) fn record_gossip_outbound_error(error_type: &'static str) {
    OUTBOUND_ERRORS.add(1, &[KeyValue::new("error_type", error_type)]);
}
