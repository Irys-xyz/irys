use opentelemetry::metrics::{Counter, Gauge};
use opentelemetry::{global, KeyValue};
use std::sync::OnceLock;

fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-actors")
}

static BLOCKS_PRODUCED: OnceLock<Counter<u64>> = OnceLock::new();
static BLOCK_PRODUCER_RETRIES: OnceLock<Counter<u64>> = OnceLock::new();
static BLOCK_PRODUCER_REBUILDS: OnceLock<Counter<u64>> = OnceLock::new();

static REORGS: OnceLock<Counter<u64>> = OnceLock::new();
static CANONICAL_TIP_HEIGHT: OnceLock<Gauge<u64>> = OnceLock::new();

static VDF_PENDING: OnceLock<Gauge<u64>> = OnceLock::new();
static CONCURRENT_ACTIVE: OnceLock<Gauge<u64>> = OnceLock::new();

static CACHE_CHUNK_COUNT: OnceLock<Gauge<u64>> = OnceLock::new();
static CACHE_CHUNK_SIZE_BYTES: OnceLock<Gauge<u64>> = OnceLock::new();

static BLOCK_DISCOVERY_ERRORS: OnceLock<Counter<u64>> = OnceLock::new();

static DATA_SYNC_CHUNKS_COMPLETED: OnceLock<Counter<u64>> = OnceLock::new();
static DATA_SYNC_CHUNK_FAILURES: OnceLock<Counter<u64>> = OnceLock::new();
static DATA_SYNC_ACTIVE_PEERS: OnceLock<Gauge<u64>> = OnceLock::new();

static MINING_SOLUTIONS_FOUND: OnceLock<Counter<u64>> = OnceLock::new();

static PACKING_ACTIVE_WORKERS: OnceLock<Gauge<u64>> = OnceLock::new();
static PACKING_SEMAPHORE_AVAILABLE: OnceLock<Gauge<u64>> = OnceLock::new();

static DATA_TX_INGESTED: OnceLock<Counter<u64>> = OnceLock::new();
static DATA_TX_UNFUNDED: OnceLock<Counter<u64>> = OnceLock::new();

pub(crate) fn record_block_produced() {
    BLOCKS_PRODUCED
        .get_or_init(|| {
            meter()
                .u64_counter("irys.block_producer.blocks_produced_total")
                .with_description("Total blocks produced")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_block_producer_retry() {
    BLOCK_PRODUCER_RETRIES
        .get_or_init(|| {
            meter()
                .u64_counter("irys.block_producer.retry_attempts_total")
                .with_description("Block production retry attempts")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_block_producer_rebuild(count: u64) {
    BLOCK_PRODUCER_REBUILDS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.block_producer.rebuilds_total")
                .with_description("Block production rebuilds due to parent changes")
                .build()
        })
        .add(count, &[]);
}

pub(crate) fn record_reorg() {
    REORGS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.block_tree.reorgs_total")
                .with_description("Chain reorganization events")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_canonical_tip_height(height: u64) {
    CANONICAL_TIP_HEIGHT
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.block_tree.canonical_tip_height")
                .with_description("Height of the canonical chain tip")
                .build()
        })
        .record(height, &[]);
}

pub(crate) fn record_validation_pipeline(vdf_pending: u64, concurrent_active: u64) {
    VDF_PENDING
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.validation.vdf_pending")
                .with_description("Pending VDF validation tasks")
                .build()
        })
        .record(vdf_pending, &[]);
    CONCURRENT_ACTIVE
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.validation.concurrent_active")
                .with_description("Active concurrent validation tasks")
                .build()
        })
        .record(concurrent_active, &[]);
}

pub(crate) fn record_cache_stats(chunk_count: u64, chunk_size_bytes: u64) {
    CACHE_CHUNK_COUNT
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.cache.chunk_count")
                .with_description("Number of cached chunks")
                .build()
        })
        .record(chunk_count, &[]);
    CACHE_CHUNK_SIZE_BYTES
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.cache.chunk_size_bytes")
                .with_description("Total size of cached chunks in bytes")
                .build()
        })
        .record(chunk_size_bytes, &[]);
}

pub(crate) fn record_block_discovery_error(error_type: &str) {
    BLOCK_DISCOVERY_ERRORS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.block_discovery.errors_total")
                .with_description("Block discovery errors by type")
                .build()
        })
        .add(1, &[KeyValue::new("error_type", error_type.to_owned())]);
}

pub(crate) fn record_data_sync_chunk_completed() {
    DATA_SYNC_CHUNKS_COMPLETED
        .get_or_init(|| {
            meter()
                .u64_counter("irys.data_sync.chunks_completed_total")
                .with_description("Data sync chunks completed")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_data_sync_chunk_failure() {
    DATA_SYNC_CHUNK_FAILURES
        .get_or_init(|| {
            meter()
                .u64_counter("irys.data_sync.chunk_failures_total")
                .with_description("Data sync chunk failures")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_data_sync_active_peers(count: u64) {
    DATA_SYNC_ACTIVE_PEERS
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.data_sync.active_peers")
                .with_description("Number of active data sync peers")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_mining_solution_found() {
    MINING_SOLUTIONS_FOUND
        .get_or_init(|| {
            meter()
                .u64_counter("irys.mining.solutions_found_total")
                .with_description("Mining solutions found")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_packing_workers(active: u64, semaphore_available: u64) {
    PACKING_ACTIVE_WORKERS
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.packing.active_workers")
                .with_description("Number of active packing workers")
                .build()
        })
        .record(active, &[]);
    PACKING_SEMAPHORE_AVAILABLE
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.packing.semaphore_available")
                .with_description("Available packing semaphore permits")
                .build()
        })
        .record(semaphore_available, &[]);
}

pub(crate) fn record_data_tx_ingested() {
    DATA_TX_INGESTED
        .get_or_init(|| {
            meter()
                .u64_counter("irys.mempool.data_tx_ingested_total")
                .with_description("Data transactions ingested into mempool")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_data_tx_unfunded() {
    DATA_TX_UNFUNDED
        .get_or_init(|| {
            meter()
                .u64_counter("irys.mempool.data_tx_unfunded_total")
                .with_description("Data transactions rejected due to insufficient funds")
                .build()
        })
        .add(1, &[]);
}
