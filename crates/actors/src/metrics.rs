use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-actors";

    counter BLOCKS_PRODUCED("irys.block_producer.blocks_produced_total", "Total blocks produced");
    counter BLOCK_PRODUCER_RETRIES("irys.block_producer.retry_attempts_total", "Block production retry attempts");
    counter BLOCK_PRODUCER_REBUILDS("irys.block_producer.rebuilds_total", "Block production rebuilds due to parent changes");
    counter REORGS("irys.block_tree.reorgs_total", "Chain reorganization events");
    gauge CANONICAL_TIP_HEIGHT("irys.block_tree.canonical_tip_height", "Height of the canonical chain tip");
    gauge VDF_PENDING("irys.validation.vdf_pending", "Pending VDF validation tasks");
    gauge CONCURRENT_ACTIVE("irys.validation.concurrent_active", "Active concurrent validation tasks");
    gauge CACHE_CHUNK_COUNT("irys.cache.chunk_count", "Number of cached chunks");
    gauge CACHE_CHUNK_SIZE_BYTES("irys.cache.chunk_size_bytes", "Total size of cached chunks in bytes");
    counter BLOCK_DISCOVERY_ERRORS("irys.block_discovery.errors_total", "Block discovery errors by type");
    counter DATA_SYNC_CHUNKS_COMPLETED("irys.data_sync.chunks_completed_total", "Data sync chunks completed");
    counter DATA_SYNC_CHUNK_FAILURES("irys.data_sync.chunk_failures_total", "Data sync chunk failures");
    gauge DATA_SYNC_ACTIVE_PEERS("irys.data_sync.active_peers", "Number of active data sync peers");
    counter MINING_SOLUTIONS_FOUND("irys.mining.solutions_found_total", "Mining solutions found");
    gauge PACKING_ACTIVE_WORKERS("irys.packing.active_workers", "Number of active packing workers");
    gauge PACKING_SEMAPHORE_AVAILABLE("irys.packing.semaphore_available", "Available packing semaphore permits");
    counter DATA_TX_INGESTED("irys.mempool.data_tx_ingested_total", "Data transactions ingested into mempool");
    counter DATA_TX_UNFUNDED("irys.mempool.data_tx_unfunded_total", "Data transactions rejected due to insufficient funds");
}

// Defined separately: uses the "irys-chain" meter (not "irys-actors") but
// lives in actors because reth_service needs it and chain depends on actors.
static RETH_FCU_HEAD_HEIGHT: std::sync::LazyLock<opentelemetry::metrics::Gauge<u64>> =
    std::sync::LazyLock::new(|| {
        opentelemetry::global::meter("irys-chain")
            .u64_gauge("irys.reth.fcu_head_height")
            .with_description("Reth fork choice update head block height")
            .build()
    });

pub fn record_reth_fcu_head_height(height: u64) {
    RETH_FCU_HEAD_HEIGHT.record(height, &[]);
}

pub(crate) fn record_block_produced() {
    BLOCKS_PRODUCED.add(1, &[]);
}

pub(crate) fn record_block_producer_retry() {
    BLOCK_PRODUCER_RETRIES.add(1, &[]);
}

pub(crate) fn record_block_producer_rebuild(count: u64) {
    BLOCK_PRODUCER_REBUILDS.add(count, &[]);
}

pub(crate) fn record_reorg() {
    REORGS.add(1, &[]);
}

pub(crate) fn record_canonical_tip_height(height: u64) {
    CANONICAL_TIP_HEIGHT.record(height, &[]);
}

pub(crate) fn record_validation_pipeline(vdf_pending: u64, concurrent_active: u64) {
    VDF_PENDING.record(vdf_pending, &[]);
    CONCURRENT_ACTIVE.record(concurrent_active, &[]);
}

pub(crate) fn record_cache_stats(chunk_count: u64, chunk_size_bytes: u64) {
    CACHE_CHUNK_COUNT.record(chunk_count, &[]);
    CACHE_CHUNK_SIZE_BYTES.record(chunk_size_bytes, &[]);
}

pub(crate) fn record_block_discovery_error(error_type: &'static str) {
    BLOCK_DISCOVERY_ERRORS.add(1, &[KeyValue::new("error_type", error_type)]);
}

pub(crate) fn record_data_sync_chunk_completed() {
    DATA_SYNC_CHUNKS_COMPLETED.add(1, &[]);
}

pub(crate) fn record_data_sync_chunk_failure() {
    DATA_SYNC_CHUNK_FAILURES.add(1, &[]);
}

pub(crate) fn record_data_sync_active_peers(count: u64) {
    DATA_SYNC_ACTIVE_PEERS.record(count, &[]);
}

pub(crate) fn record_mining_solution_found() {
    MINING_SOLUTIONS_FOUND.add(1, &[]);
}

pub(crate) fn record_packing_workers(active: u64, semaphore_available: u64) {
    PACKING_ACTIVE_WORKERS.record(active, &[]);
    PACKING_SEMAPHORE_AVAILABLE.record(semaphore_available, &[]);
}

pub(crate) fn record_data_tx_ingested() {
    DATA_TX_INGESTED.add(1, &[]);
}

pub(crate) fn record_data_tx_unfunded() {
    DATA_TX_UNFUNDED.add(1, &[]);
}
