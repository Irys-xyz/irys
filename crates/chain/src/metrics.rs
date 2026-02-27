use opentelemetry::KeyValue;
use std::sync::OnceLock;
use std::time::Instant;

irys_utils::define_metrics! {
    meter: "irys-chain";

    gauge BLOCK_HEIGHT("irys.chain.block_height", "Current block height");
    gauge PEER_COUNT("irys.chain.peer_count", "Number of connected peers");
    gauge PENDING_CHUNKS("irys.mempool.pending_chunks", "Number of pending chunks in mempool");
    gauge PENDING_DATA_TXS("irys.mempool.pending_data_txs", "Number of pending data transactions in mempool");
    gauge SYNC_STATE("irys.sync.state", "Node sync state (1=synced, 0=syncing)");
    gauge NODE_UP("irys.node.up", "Node liveness indicator (1=up)");
    gauge NODE_UPTIME("irys.node.uptime_seconds", "Node uptime in seconds");
    gauge VDF_GLOBAL_STEP("irys.vdf.global_step_number", "Current VDF global step number");
    gauge VDF_MINING_ENABLED("irys.vdf.mining_enabled", "Whether VDF mining is enabled (1=yes, 0=no)");
    gauge STORAGE_MODULES_TOTAL("irys.storage.modules_total", "Total number of storage modules");
    gauge PARTITIONS_ASSIGNED("irys.storage.partitions_assigned", "Number of storage modules with a partition assigned");
    gauge PARTITIONS_UNASSIGNED("irys.storage.partitions_unassigned", "Number of storage modules without a partition assigned");
    counter NODE_SHUTDOWN("irys.node.shutdown_total", "Node shutdown events by reason");
    counter PLEDGE_TX_POSTED("irys.commitment.pledge_tx_posted_total", "Pledge transactions posted");
    counter PEER_FETCH_ERRORS("irys.peer.fetch_errors_total", "Peer fetch errors by type");
    gauge RETH_FCU_HEAD_HEIGHT("irys.reth.fcu_head_height", "Reth fork choice update head block height");
}

// Not a metric instrument — tracks process start time for uptime calculation.
static NODE_START_TIME: OnceLock<Instant> = OnceLock::new();

pub(crate) fn record_block_height(height: u64) {
    BLOCK_HEIGHT.record(height, &[]);
}

pub(crate) fn record_peer_count(count: u64) {
    PEER_COUNT.record(count, &[]);
}

pub(crate) fn record_pending_chunks(count: u64) {
    PENDING_CHUNKS.record(count, &[]);
}

pub(crate) fn record_pending_data_txs(count: u64) {
    PENDING_DATA_TXS.record(count, &[]);
}

pub(crate) fn record_sync_state(synced: bool) {
    SYNC_STATE.record(u64::from(synced), &[]);
}

pub(crate) fn record_node_up() {
    NODE_UP.record(1, &[]);
}

pub(crate) fn record_node_uptime() {
    let start = NODE_START_TIME.get_or_init(Instant::now);
    NODE_UPTIME.record(start.elapsed().as_secs(), &[]);
}

pub(crate) fn record_vdf_global_step(step: u64) {
    VDF_GLOBAL_STEP.record(step, &[]);
}

pub(crate) fn record_vdf_mining_enabled(enabled: bool) {
    VDF_MINING_ENABLED.record(u64::from(enabled), &[]);
}

pub(crate) fn record_storage_modules_total(count: u64) {
    STORAGE_MODULES_TOTAL.record(count, &[]);
}

pub(crate) fn record_partitions_assigned(count: u64) {
    PARTITIONS_ASSIGNED.record(count, &[]);
}

pub(crate) fn record_partitions_unassigned(count: u64) {
    PARTITIONS_UNASSIGNED.record(count, &[]);
}

pub(crate) fn record_node_shutdown(reason: &'static str) {
    NODE_SHUTDOWN.add(1, &[KeyValue::new("reason", reason)]);
}

pub(crate) fn record_pledge_tx_posted() {
    PLEDGE_TX_POSTED.add(1, &[]);
}

pub(crate) fn record_peer_fetch_error(error_type: &'static str) {
    PEER_FETCH_ERRORS.add(1, &[KeyValue::new("error_type", error_type)]);
}
