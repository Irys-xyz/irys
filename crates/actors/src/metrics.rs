use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-actors";

    counter BLOCKS_PRODUCED("irys.block_producer.blocks_produced_total", "Total blocks produced");
    counter BLOCK_PRODUCER_RETRIES("irys.block_producer.retry_attempts_total", "Block production retry attempts");
    counter BLOCK_PRODUCER_REBUILDS("irys.block_producer.rebuilds_total", "Block production rebuilds due to parent changes");
    counter REORGS("irys.block_tree.reorgs_total", "Chain reorganization events");
    gauge REORG_DEPTH("irys.block_tree.latest_reorg_depth", "Depth of the most recent reorg (blocks discarded from the canonical chain). Mirrors Reth's reth_blockchain_tree_latest_reorg_depth for side-by-side comparison.");
    gauge CANONICAL_TIP_HEIGHT("irys.block_tree.canonical_tip_height", "Height of the canonical chain tip");
    gauge VDF_PENDING("irys.validation.vdf_pending", "Pending VDF validation tasks (labelled by priority class)");
    gauge CONCURRENT_ACTIVE("irys.validation.concurrent_active", "Active concurrent validation tasks");
    gauge VALIDATION_QUEUE_OLDEST_AGE_MS("irys.validation.queue_oldest_age_ms", "Age of the oldest pending VDF task in milliseconds");
    counter VALIDATION_TASK_FORCE_ABORTED("irys.validation.task_force_aborted_total", "Validation watchdog force-aborts of stalled VDF tasks");
    counter VALIDATION_CONCURRENT_CANCEL_REQUEUED("irys.validation.concurrent_cancel_requeued_total", "Concurrent validation tasks requeued after unexpected JoinError::Cancelled (sustained occurrence implies Tokio distress or external abort source)");
    counter VALIDATION_CONCURRENT_CANCEL_REPEATED("irys.validation.concurrent_cancel_repeated_total", "Concurrent validation tasks parked as RepeatedCancellation SoftInternal after exceeding MAX_CONCURRENT_CANCEL_RETRIES (recovery delegated to fresh gossip).");
    gauge CACHE_CHUNK_COUNT("irys.cache.chunk_count", "Number of cached chunks");
    gauge CACHE_CHUNK_SIZE_BYTES("irys.cache.chunk_size_bytes", "Total size of cached chunks in bytes");
    counter BLOCK_DISCOVERY_ERRORS("irys.block_discovery.errors_total", "Block discovery errors by type");
    counter BLOCK_DISCOVERY_SUCCESS("irys.block_discovery.success_total", "Successful block pre-validations");
    counter DATA_SYNC_CHUNKS_COMPLETED("irys.data_sync.chunks_completed_total", "Data sync chunks completed");
    counter DATA_SYNC_CHUNK_FAILURES("irys.data_sync.chunk_failures_total", "Data sync chunk failures");
    gauge DATA_SYNC_ACTIVE_PEERS("irys.data_sync.active_peers", "Number of active data sync peers");
    counter MINING_SOLUTIONS_FOUND("irys.mining.solutions_found_total", "Mining solutions found");
    gauge PACKING_ACTIVE_WORKERS("irys.packing.active_workers", "Number of active packing workers");
    gauge PACKING_SEMAPHORE_AVAILABLE("irys.packing.semaphore_available", "Available packing semaphore permits");
    counter DATA_TX_INGESTED("irys.mempool.data_tx_ingested_total", "Data transactions ingested into mempool");
    counter DATA_TX_UNFUNDED("irys.mempool.data_tx_unfunded_total", "Data transactions rejected due to insufficient funds");

    histogram PREVALIDATION_DURATION_MS(
        "irys.validation.prevalidation_duration_ms",
        "End-to-end pre-validation duration in milliseconds",
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0]
    );
    histogram VDF_DURATION_MS(
        "irys.validation.vdf_duration_ms",
        "VDF validation duration (preemptible task) in milliseconds",
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0]
    );
    histogram VDF_STEP_WAIT_DURATION_MS(
        "irys.validation.vdf_step_wait_duration_ms",
        "Time spent waiting for VDF state to reach the required step in milliseconds",
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0]
    );
    histogram VALIDATION_STAGE_DURATION_MS(
        "irys.validation.stage_duration_ms",
        "Duration of an individual validation stage in milliseconds (labelled by stage)",
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0]
    );
    histogram PARENT_WAIT_DURATION_MS(
        "irys.validation.parent_wait_duration_ms",
        "Time concurrent validation waits for parent block to be validated in milliseconds",
        vec![10.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0, 30000.0, 60000.0, 300000.0, 600000.0]
    );
    histogram REORG_REEVALUATE_DURATION_MS(
        "irys.validation.reorg_reevaluate_duration_ms",
        "Time spent re-evaluating validation queue priorities after a reorg in milliseconds",
        vec![0.1, 0.5, 1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0]
    );
    histogram BLOCK_AGE_AT_VALIDATION_MS(
        "irys.validation.block_age_at_completion_ms",
        "Wall-clock age of a block (block timestamp -> validation completion) in milliseconds",
        vec![100.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0, 300000.0, 600000.0]
    );
    histogram VALIDATION_FULL_DURATION_MS(
        "irys.validation.full_duration_ms",
        "End-to-end full validation pipeline duration (queue receipt -> result reported) in milliseconds",
        vec![10.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0, 60000.0, 300000.0]
    );
    histogram PRODUCER_PARENT_WAIT_DURATION_MS(
        "irys.block_producer.parent_wait_duration_ms",
        "Time the block producer waits for the chosen parent block to be fully validated in milliseconds",
        vec![10.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0, 30000.0, 60000.0, 300000.0, 600000.0]
    );

    counter VALIDATION_RESULTS("irys.validation.results_total", "Validation results by stage and result (labelled)");
    counter VDF_PREEMPTIONS("irys.validation.vdf_preemptions_total", "VDF tasks preempted by higher-priority work");
    counter VALIDATION_CANCELLATIONS("irys.validation.cancellations_total", "Validation cancellations by reason (labelled)");
    counter REORG_PRIORITY_REEVALUATIONS("irys.validation.reorg_priority_reevaluations_total", "Validation priority re-evaluations triggered by reorg events");
    counter PRODUCER_PARENT_WAIT_TARGET_SWITCHES("irys.block_producer.parent_wait_target_switches_total", "Times block producer switched parent target while waiting for validation");
    counter SOFT_INTERNAL_DISCARD("irys.block.soft_internal_discard_total", "Blocks discarded from block_tree due to soft-internal validation failure (labelled by reason). Pairs with soft_internal_recovered_total to gauge whether gossip-driven recovery is sufficient.");
    counter SOFT_INTERNAL_RECOVERED("irys.block.soft_internal_recovered_total", "Blocks previously discarded as soft-internal that later reached Valid (labelled by the original discard reason). Pair with soft_internal_discard_total to gauge gossip-driven recovery rate.");
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

pub(crate) fn record_reorg_depth(depth: u64) {
    REORG_DEPTH.record(depth, &[]);
}

pub(crate) fn record_canonical_tip_height(height: u64) {
    CANONICAL_TIP_HEIGHT.record(height, &[]);
}

/// Record a snapshot of the validation queue. `vdf_pending_by_priority` MUST
/// include every priority class so that absent labels don't carry stale values.
pub(crate) fn record_validation_queue_snapshot(
    vdf_pending_by_priority: &[(&'static str, u64)],
    concurrent_active: u64,
    queue_oldest_age_ms: u64,
) {
    for (priority, count) in vdf_pending_by_priority {
        VDF_PENDING.record(*count, &[KeyValue::new("priority", *priority)]);
    }
    CONCURRENT_ACTIVE.record(concurrent_active, &[]);
    VALIDATION_QUEUE_OLDEST_AGE_MS.record(queue_oldest_age_ms, &[]);
}

pub(crate) fn record_validation_task_force_aborted(stage: &'static str) {
    VALIDATION_TASK_FORCE_ABORTED.add(1, &[KeyValue::new("stage", stage)]);
}

pub(crate) fn record_validation_concurrent_cancel_requeued() {
    VALIDATION_CONCURRENT_CANCEL_REQUEUED.add(1, &[]);
}

pub(crate) fn record_validation_concurrent_cancel_repeated() {
    VALIDATION_CONCURRENT_CANCEL_REPEATED.add(1, &[]);
}

pub(crate) fn record_cache_stats(chunk_count: u64, chunk_size_bytes: u64) {
    CACHE_CHUNK_COUNT.record(chunk_count, &[]);
    CACHE_CHUNK_SIZE_BYTES.record(chunk_size_bytes, &[]);
}

pub(crate) fn record_block_discovery_error(error_type: &'static str) {
    BLOCK_DISCOVERY_ERRORS.add(1, &[KeyValue::new("error_type", error_type)]);
}

pub(crate) fn record_block_discovery_success() {
    BLOCK_DISCOVERY_SUCCESS.add(1, &[]);
}

pub(crate) fn record_prevalidation_duration_ms(ms: f64) {
    PREVALIDATION_DURATION_MS.record(ms, &[]);
}

pub(crate) fn record_vdf_duration_ms(ms: f64) {
    VDF_DURATION_MS.record(ms, &[]);
}

pub(crate) fn record_vdf_step_wait_duration_ms(ms: f64) {
    VDF_STEP_WAIT_DURATION_MS.record(ms, &[]);
}

pub(crate) fn record_validation_stage_duration_ms(stage: &'static str, ms: f64) {
    VALIDATION_STAGE_DURATION_MS.record(ms, &[KeyValue::new("stage", stage)]);
}

pub(crate) fn record_validation_result(stage: &'static str, result: &'static str) {
    VALIDATION_RESULTS.add(
        1,
        &[
            KeyValue::new("stage", stage),
            KeyValue::new("result", result),
        ],
    );
}

pub(crate) fn record_vdf_preemption() {
    VDF_PREEMPTIONS.add(1, &[]);
}

pub(crate) fn record_validation_cancellation(reason: &'static str) {
    VALIDATION_CANCELLATIONS.add(1, &[KeyValue::new("reason", reason)]);
}

pub(crate) fn record_parent_wait_duration_ms(ms: f64) {
    PARENT_WAIT_DURATION_MS.record(ms, &[]);
}

pub(crate) fn record_block_age_at_validation_ms(ms: f64) {
    BLOCK_AGE_AT_VALIDATION_MS.record(ms, &[]);
}

pub(crate) fn record_validation_full_duration_ms(ms: f64) {
    VALIDATION_FULL_DURATION_MS.record(ms, &[]);
}

pub(crate) fn record_reorg_priority_reevaluation(duration_ms: f64) {
    REORG_PRIORITY_REEVALUATIONS.add(1, &[]);
    REORG_REEVALUATE_DURATION_MS.record(duration_ms, &[]);
}

pub(crate) fn record_producer_parent_wait_duration_ms(ms: f64) {
    PRODUCER_PARENT_WAIT_DURATION_MS.record(ms, &[]);
}

pub(crate) fn record_producer_parent_wait_target_switch() {
    PRODUCER_PARENT_WAIT_TARGET_SWITCHES.add(1, &[]);
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

pub(crate) fn record_soft_internal_discard(reason: &'static str) {
    SOFT_INTERNAL_DISCARD.add(1, &[KeyValue::new("reason", reason)]);
}

pub(crate) fn record_soft_internal_recovered(reason: &'static str) {
    SOFT_INTERNAL_RECOVERED.add(1, &[KeyValue::new("reason", reason)]);
}
