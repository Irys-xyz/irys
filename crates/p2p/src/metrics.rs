use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-p2p";

    counter CHUNKS_RECEIVED("irys.gossip.chunks.received_total", "Total chunks received via gossip");
    counter BYTES_RECEIVED("irys.gossip.chunks.bytes_received_total", "Total bytes received in gossip chunk payloads");
    histogram PROCESSING_MS("irys.gossip.chunks.processing_duration_ms", "Gossip chunk processing latency in milliseconds", vec![0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]);
    counter INBOUND_ERRORS("irys.gossip.inbound.errors_total", "Gossip inbound processing errors by type");
    counter OUTBOUND_ERRORS("irys.gossip.outbound.errors_total", "Gossip outbound send errors by type");
    counter PULL_FAILURES("irys.gossip.pull.failures_total", "Gossip pull failures by request kind and reason");
    counter BLOCK_POOL_REJECTIONS("irys.block_pool.rejections_total", "Block pool rejections by reason");
    counter BLOCK_POOL_ORPHAN_RETRIES("irys.block_pool.orphan_retries_total", "Orphan-block reprocessing attempts dispatched by block pool");
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

pub(crate) fn record_gossip_pull_failure(kind: &'static str, reason: &'static str) {
    PULL_FAILURES.add(
        1,
        &[KeyValue::new("kind", kind), KeyValue::new("reason", reason)],
    );
}

/// Increment `irys.block_pool.rejections_total{reason}`. `reason` is a
/// bounded-cardinality snake_case tag drawn from the block-pool rejection
/// taxonomy (e.g. `part_of_pruned_fork`, `already_processed`).
pub(crate) fn record_block_pool_rejection(reason: &'static str) {
    BLOCK_POOL_REJECTIONS.add(1, &[KeyValue::new("reason", reason)]);
}

/// Increment `irys.block_pool.orphan_retries_total`. Bumped each time the
/// block pool dispatches an `AttemptReprocessingBlock` for an orphan whose
/// parent has just arrived.
pub(crate) fn record_block_pool_orphan_retry() {
    BLOCK_POOL_ORPHAN_RETRIES.add(1, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_block_pool_rejection_does_not_panic() {
        // OTel counters do not expose a `.get()` for in-process verification —
        // exercise the call path with each documented reason tag and rely on
        // the deploy-time `/metrics` scrape check for end-to-end verification.
        record_block_pool_rejection("part_of_pruned_fork");
        record_block_pool_rejection("already_processed");
        record_block_pool_rejection("parent_not_in_cache");
        record_block_pool_rejection("fatal_cache_corruption");
        record_block_pool_rejection("internal_prevalidation_failure");
        record_block_pool_rejection("block_validation_error");
        record_block_pool_rejection("reth_payload_unavailable");
        record_block_pool_rejection("try_reprocess_finalized");
    }

    #[test]
    fn record_block_pool_orphan_retry_does_not_panic() {
        record_block_pool_orphan_retry();
    }
}
