use opentelemetry::metrics::{Counter, Gauge};
use opentelemetry::{global, KeyValue};
use std::sync::OnceLock;
use std::time::Instant;

fn meter() -> opentelemetry::metrics::Meter {
    global::meter("irys-chain")
}

static BLOCK_HEIGHT: OnceLock<Gauge<u64>> = OnceLock::new();
static PEER_COUNT: OnceLock<Gauge<u64>> = OnceLock::new();
static PENDING_CHUNKS: OnceLock<Gauge<u64>> = OnceLock::new();
static PENDING_DATA_TXS: OnceLock<Gauge<u64>> = OnceLock::new();
static SYNC_STATE: OnceLock<Gauge<u64>> = OnceLock::new();
static NODE_UP: OnceLock<Gauge<u64>> = OnceLock::new();
static NODE_UPTIME: OnceLock<Gauge<u64>> = OnceLock::new();
static NODE_START_TIME: OnceLock<Instant> = OnceLock::new();
static VDF_MINING_ENABLED: OnceLock<Gauge<u64>> = OnceLock::new();
static STORAGE_MODULES_TOTAL: OnceLock<Gauge<u64>> = OnceLock::new();
static PARTITIONS_ASSIGNED: OnceLock<Gauge<u64>> = OnceLock::new();
static PARTITIONS_UNASSIGNED: OnceLock<Gauge<u64>> = OnceLock::new();
static VDF_GLOBAL_STEP: OnceLock<Gauge<u64>> = OnceLock::new();
static NODE_SHUTDOWN: OnceLock<Counter<u64>> = OnceLock::new();
static PLEDGE_TX_POSTED: OnceLock<Counter<u64>> = OnceLock::new();
static PEER_FETCH_ERRORS: OnceLock<Counter<u64>> = OnceLock::new();
static RETH_FCU_HEAD_HEIGHT: OnceLock<Gauge<u64>> = OnceLock::new();

pub(crate) fn record_block_height(height: u64) {
    BLOCK_HEIGHT
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.chain.block_height")
                .with_description("Current block height")
                .build()
        })
        .record(height, &[]);
}

pub(crate) fn record_peer_count(count: u64) {
    PEER_COUNT
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.chain.peer_count")
                .with_description("Number of connected peers")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_pending_chunks(count: u64) {
    PENDING_CHUNKS
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.mempool.pending_chunks")
                .with_description("Number of pending chunks in mempool")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_pending_data_txs(count: u64) {
    PENDING_DATA_TXS
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.mempool.pending_data_txs")
                .with_description("Number of pending data transactions in mempool")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_sync_state(synced: bool) {
    SYNC_STATE
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.sync.state")
                .with_description("Node sync state (1=synced, 0=syncing)")
                .build()
        })
        .record(u64::from(synced), &[]);
}

pub(crate) fn record_node_up() {
    NODE_UP
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.node.up")
                .with_description("Node liveness indicator (1=up)")
                .build()
        })
        .record(1, &[]);
}

pub(crate) fn record_node_uptime() {
    let start = NODE_START_TIME.get_or_init(Instant::now);
    NODE_UPTIME
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.node.uptime_seconds")
                .with_description("Node uptime in seconds")
                .build()
        })
        .record(start.elapsed().as_secs(), &[]);
}

pub(crate) fn record_vdf_global_step(step: u64) {
    VDF_GLOBAL_STEP
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.vdf.global_step_number")
                .with_description("Current VDF global step number")
                .build()
        })
        .record(step, &[]);
}

pub(crate) fn record_vdf_mining_enabled(enabled: bool) {
    VDF_MINING_ENABLED
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.vdf.mining_enabled")
                .with_description("Whether VDF mining is enabled (1=yes, 0=no)")
                .build()
        })
        .record(u64::from(enabled), &[]);
}

pub(crate) fn record_storage_modules_total(count: u64) {
    STORAGE_MODULES_TOTAL
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.storage.modules_total")
                .with_description("Total number of storage modules")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_partitions_assigned(count: u64) {
    PARTITIONS_ASSIGNED
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.storage.partitions_assigned")
                .with_description("Number of storage modules with a partition assigned")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_partitions_unassigned(count: u64) {
    PARTITIONS_UNASSIGNED
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.storage.partitions_unassigned")
                .with_description("Number of storage modules without a partition assigned")
                .build()
        })
        .record(count, &[]);
}

pub(crate) fn record_node_shutdown(reason: &str) {
    NODE_SHUTDOWN
        .get_or_init(|| {
            meter()
                .u64_counter("irys.node.shutdown_total")
                .with_description("Node shutdown events by reason")
                .build()
        })
        .add(1, &[KeyValue::new("reason", reason.to_owned())]);
}

pub(crate) fn record_pledge_tx_posted() {
    PLEDGE_TX_POSTED
        .get_or_init(|| {
            meter()
                .u64_counter("irys.commitment.pledge_tx_posted_total")
                .with_description("Pledge transactions posted")
                .build()
        })
        .add(1, &[]);
}

pub(crate) fn record_peer_fetch_error(error_type: &'static str) {
    PEER_FETCH_ERRORS
        .get_or_init(|| {
            meter()
                .u64_counter("irys.peer.fetch_errors_total")
                .with_description("Peer fetch errors by type")
                .build()
        })
        .add(1, &[KeyValue::new("error_type", error_type)]);
}

pub(crate) fn record_reth_fcu_head_height(height: u64) {
    RETH_FCU_HEAD_HEIGHT
        .get_or_init(|| {
            meter()
                .u64_gauge("irys.reth.fcu_head_height")
                .with_description("Reth fork choice update head block height")
                .build()
        })
        .record(height, &[]);
}
