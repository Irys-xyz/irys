use super::*;
use crate::{chunk_fetcher::MockChunkFetcher, test_helpers::build_test_service_senders};
use irys_domain::ChunkType;
use irys_domain::{BlockTree, StorageModuleInfo};
use irys_testing_utils::TempDirBuilder;
use irys_types::{
    Config, ConsensusConfig, DataLedger, H256, IrysAddress, NodeConfig,
    partition::PartitionAssignment, partition_chunk_offset_ie,
};

fn test_config(base_directory: std::path::PathBuf, num_chunks: u64) -> Config {
    let node_config = NodeConfig {
        consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: num_chunks,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            entropy_packing_iterations: 1,
            block_migration_depth: 1,
            chain_id: 1,
            ..ConsensusConfig::testing()
        }),
        base_directory,
        ..NodeConfig::testing()
    };
    Config::new_with_random_peer_id(node_config)
}

fn packed_sm(config: &Config, num_chunks: u64) -> Arc<StorageModule> {
    let pa = PartitionAssignment {
        ledger_id: Some(DataLedger::Publish.into()),
        slot_index: Some(0),
        miner_address: IrysAddress::from([1_u8; 20]),
        partition_hash: H256::random(),
    };
    let info = StorageModuleInfo {
        id: 0,
        partition_assignment: Some(pa),
        submodules: vec![(
            partition_chunk_offset_ie!(0, num_chunks as u32),
            "chunks".into(),
        )],
    };
    let sm = Arc::new(StorageModule::new(&info, config).expect("storage module"));
    sm.pack_with_zeros();
    sm
}

fn make_orchestrator(sm: Arc<StorageModule>, config: &Config) -> ChunkOrchestrator {
    let genesis = irys_testing_utils::new_mock_signed_header();
    let block_tree = BlockTree::new(&genesis, config.consensus.clone());
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
    let (service_senders, _receivers) = build_test_service_senders();
    let ledger_id: u32 = DataLedger::Publish.into();
    ChunkOrchestrator::new(
        sm,
        Arc::new(RwLock::new(HashMap::new())),
        block_tree_guard,
        &service_senders,
        Arc::new(MockChunkFetcher::new(ledger_id as usize)),
        config.node_config.clone(),
        tokio::runtime::Handle::current(),
    )
}

/// Makes `offset` durably `Data` the way the write path does: buffer, then
/// flush + fsync. `is_data_chunk_durable_at` only becomes true after the fsync.
fn store_durable_data(sm: &StorageModule, offset: PartitionChunkOffset, chunk_size: usize) {
    assert!(
        !sm.is_data_chunk_durable_at(offset),
        "offset must start non-durable"
    );
    sm.write_chunk(offset, vec![9_u8; chunk_size], ChunkType::Data);
    assert!(
        !sm.is_data_chunk_durable_at(offset),
        "a buffered write must not be reported as durable"
    );
    sm.force_sync_pending_chunks().expect("flush");
    assert!(
        sm.is_data_chunk_durable_at(offset),
        "an fsynced write must be reported as durable"
    );
}

fn insert_requested(orch: &mut ChunkOrchestrator, offset: PartitionChunkOffset, peer: IrysAddress) {
    orch.chunk_requests.insert(
        offset,
        ChunkRequest {
            chunk_offset: offset,
            excluded: None,
            request_state: ChunkRequestState::Requested(peer, Instant::now()),
        },
    );
}

#[test_log::test(tokio::test)]
async fn mark_helpers_require_valid_source_state() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([2_u8; 20]);
    let offset = PartitionChunkOffset::from(0_u32);

    // Unknown offset
    assert!(orch.mark_chunk_awaiting_durability(offset).is_err());
    assert!(orch.mark_chunk_already_durable(offset).is_err());
    assert!(
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .is_err()
    );
    assert!(orch.requeue_after_local_write_failure(offset).is_err());

    // Pending is not Requested
    orch.chunk_requests.insert(
        offset,
        ChunkRequest {
            chunk_offset: offset,
            excluded: None,
            request_state: ChunkRequestState::Pending,
        },
    );
    assert!(orch.mark_chunk_awaiting_durability(offset).is_err());
    assert!(orch.mark_chunk_already_durable(offset).is_err());
    assert!(
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .is_err()
    );
    assert!(orch.requeue_after_local_write_failure(offset).is_err());

    // Requested accepts each initial local outcome.
    insert_requested(&mut orch, offset, peer);
    orch.mark_chunk_awaiting_durability(offset).expect("buffer");
    assert!(
        matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::AwaitingDurability(..)
        ),
        "buffered write must wait for durability"
    );
    store_durable_data(&orch.storage_module, offset, 32);
    orch.reconcile_awaiting_durability(Instant::now(), DURABILITY_WAIT_TIMEOUT);
    assert_eq!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Completed
    );

    insert_requested(&mut orch, offset, peer);
    orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
        .expect("block");
    assert_eq!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
    );

    insert_requested(&mut orch, offset, peer);
    orch.requeue_after_local_write_failure(offset)
        .expect("requeue");
    assert_eq!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending
    );
    // Requeue must not blame the delivering peer.
    assert!(orch.chunk_requests[&offset].excluded.is_none());
}

#[test_log::test(tokio::test)]
async fn durability_poll_promotes_only_fsynced_offsets() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(Arc::clone(&sm), &config);
    let peer = IrysAddress::from([8_u8; 20]);
    let waiting = PartitionChunkOffset::from(1_u32);
    let other = PartitionChunkOffset::from(2_u32);

    insert_requested(&mut orch, waiting, peer);
    orch.mark_chunk_awaiting_durability(waiting).unwrap();

    // Another offset becoming durable must not promote this one.
    store_durable_data(&sm, other, 32);
    orch.reconcile_awaiting_durability(Instant::now(), DURABILITY_WAIT_TIMEOUT);
    assert!(
        matches!(
            orch.chunk_requests[&waiting].request_state,
            ChunkRequestState::AwaitingDurability(..)
        ),
        "durability credit must not leak across offsets"
    );

    store_durable_data(&sm, waiting, 32);
    orch.reconcile_awaiting_durability(Instant::now(), DURABILITY_WAIT_TIMEOUT);
    assert_eq!(
        orch.chunk_requests[&waiting].request_state,
        ChunkRequestState::Completed
    );

    // Polling again is a no-op: a completed request stays completed.
    orch.reconcile_awaiting_durability(Instant::now(), DURABILITY_WAIT_TIMEOUT);
    assert_eq!(
        orch.chunk_requests[&waiting].request_state,
        ChunkRequestState::Completed
    );
}

#[test_log::test(tokio::test)]
async fn durability_poll_returns_a_stalled_wait_to_pending() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([8_u8; 20]);
    let waiting = PartitionChunkOffset::from(1_u32);

    insert_requested(&mut orch, waiting, peer);
    orch.mark_chunk_awaiting_durability(waiting).unwrap();

    let started = Instant::now();
    orch.reconcile_awaiting_durability(started, DURABILITY_WAIT_TIMEOUT);
    assert!(matches!(
        orch.chunk_requests[&waiting].request_state,
        ChunkRequestState::AwaitingDurability(..)
    ));

    orch.reconcile_awaiting_durability(
        started + DURABILITY_WAIT_TIMEOUT + Duration::from_secs(1),
        DURABILITY_WAIT_TIMEOUT,
    );
    assert_eq!(
        orch.chunk_requests[&waiting].request_state,
        ChunkRequestState::Pending,
        "a write that never became durable must be re-fetchable"
    );
    assert!(
        orch.chunk_requests[&waiting].excluded.is_none(),
        "a stalled local write must not blame the delivering peer"
    );
}

#[test_log::test(tokio::test)]
async fn awaiting_durability_is_observable_and_times_out_to_pending() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([9_u8; 20]);
    let offset = PartitionChunkOffset::from(1_u32);

    insert_requested(&mut orch, offset, peer);
    orch.mark_chunk_awaiting_durability(offset).unwrap();
    let metrics = orch.get_metrics();
    assert_eq!(metrics.awaiting_durability_requests, 1);
    assert_eq!(metrics.active_requests, 0);

    orch.reconcile_awaiting_durability(Instant::now(), Duration::ZERO);
    assert_eq!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending,
        "a stalled durability wait must not park data sync forever"
    );
    assert_eq!(orch.get_metrics().awaiting_durability_requests, 0);
}

#[test_log::test(tokio::test)]
async fn awaiting_durability_rechecks_storage_before_refetching() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(Arc::clone(&sm), &config);
    let peer = IrysAddress::from([10_u8; 20]);
    let offset = PartitionChunkOffset::from(1_u32);

    insert_requested(&mut orch, offset, peer);
    orch.mark_chunk_awaiting_durability(offset).unwrap();
    sm.write_chunk(
        offset,
        vec![0xab; config.consensus.chunk_size as usize],
        ChunkType::Data,
    );
    sm.sync_pending_chunks().unwrap();
    assert!(sm.is_data_chunk_durable_at(offset));

    orch.reconcile_awaiting_durability(Instant::now(), Duration::ZERO);
    assert_eq!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Completed,
        "durable storage must win over a stale wait state"
    );
}

#[test_log::test(tokio::test)]
async fn on_chunk_fetched_does_not_mark_stored() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([3_u8; 20]);
    let offset = PartitionChunkOffset::from(1_u32);
    insert_requested(&mut orch, offset, peer);

    orch.on_chunk_fetched(offset, peer).expect("fetched");
    assert!(
        matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::Requested(..)
        ),
        "fetch success must not imply durable store (Completed)"
    );
}

#[test_log::test(tokio::test)]
async fn blocked_retained_while_entropy_dropped_when_data() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm.clone(), &config);
    let peer = IrysAddress::from([4_u8; 20]);
    let offset = PartitionChunkOffset::from(0_u32);

    insert_requested(&mut orch, offset, peer);
    orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
        .unwrap();

    // Still Entropy → retain Blocked
    orch.populate_request_queue();
    assert!(
        matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
        ),
        "Blocked must stay while offset is Entropy"
    );

    // Become Data (e.g. gossip/heal) → drop request
    let data_bytes = vec![0xab; config.consensus.chunk_size as usize];
    sm.write_chunk(offset, data_bytes, ChunkType::Data);
    sm.sync_pending_chunks().unwrap();
    assert!(matches!(sm.get_chunk_type(&offset), Some(ChunkType::Data)));

    orch.populate_request_queue();
    assert!(
        !orch.chunk_requests.contains_key(&offset),
        "Blocked must drop once offset is Data"
    );
}

#[test_log::test(tokio::test)]
async fn blocked_excluded_from_pending_budget_and_dispatch_selection() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([5_u8; 20]);

    // Fill with Blocked offsets — must not count as pending budget consumers.
    for i in 0..3_u32 {
        let offset = PartitionChunkOffset::from(i);
        insert_requested(&mut orch, offset, peer);
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .unwrap();
    }

    let metrics = orch.get_metrics();
    assert_eq!(metrics.blocked_requests, 3);
    assert_eq!(metrics.pending_requests, 0);
    assert_eq!(metrics.active_requests, 0);

    // Only Pending is eligible for dispatch.
    let pending_for_dispatch: Vec<_> = orch
        .chunk_requests
        .iter()
        .filter_map(|(&offset, req)| {
            matches!(req.request_state, ChunkRequestState::Pending).then_some(offset)
        })
        .collect();
    assert!(
        pending_for_dispatch.is_empty(),
        "Blocked offsets must not be selected for re-dispatch"
    );

    // Pending budget counts only Pending, so a fresh Pending can still be inserted
    // even when many offsets are Blocked (Vacant entries only — offset 3 is free).
    let offset3 = PartitionChunkOffset::from(3_u32);
    orch.chunk_requests.insert(
        offset3,
        ChunkRequest {
            chunk_offset: offset3,
            excluded: None,
            request_state: ChunkRequestState::Pending,
        },
    );
    let metrics = orch.get_metrics();
    assert_eq!(metrics.pending_requests, 1);
    assert_eq!(metrics.blocked_requests, 3);
}

#[test]
fn metric_label_stable() {
    assert_eq!(
        ChunkBlockReason::MissingDataRootIndex.as_metric_label(),
        "missing_data_root_index"
    );
}

#[test_log::test(tokio::test)]
async fn unblock_missing_data_root_index_requeues_only_that_reason() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([6_u8; 20]);

    let blocked = PartitionChunkOffset::from(0_u32);
    let pending = PartitionChunkOffset::from(1_u32);
    let completed = PartitionChunkOffset::from(2_u32);
    let requested = PartitionChunkOffset::from(3_u32);

    insert_requested(&mut orch, blocked, peer);
    orch.mark_chunk_blocked(blocked, ChunkBlockReason::MissingDataRootIndex)
        .unwrap();

    orch.chunk_requests.insert(
        pending,
        ChunkRequest {
            chunk_offset: pending,
            excluded: None,
            request_state: ChunkRequestState::Pending,
        },
    );

    insert_requested(&mut orch, completed, peer);
    orch.mark_chunk_awaiting_durability(completed).unwrap();
    store_durable_data(&orch.storage_module, completed, 32);
    orch.reconcile_awaiting_durability(Instant::now(), DURABILITY_WAIT_TIMEOUT);

    insert_requested(&mut orch, requested, peer);

    let n = orch.unblock_missing_data_root_index(usize::MAX);
    assert_eq!(n, 1, "only MissingDataRootIndex Blocked should unblock");
    assert_eq!(
        orch.chunk_requests[&blocked].request_state,
        ChunkRequestState::Pending
    );
    assert_eq!(
        orch.chunk_requests[&pending].request_state,
        ChunkRequestState::Pending
    );
    assert_eq!(
        orch.chunk_requests[&completed].request_state,
        ChunkRequestState::Completed
    );
    assert!(matches!(
        orch.chunk_requests[&requested].request_state,
        ChunkRequestState::Requested(..)
    ));

    // Idempotent when nothing is blocked.
    assert_eq!(orch.unblock_missing_data_root_index(usize::MAX), 0);
}

#[test_log::test(tokio::test)]
async fn unblock_missing_data_root_index_respects_cap() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([7_u8; 20]);

    for i in 0..3_u32 {
        let offset = PartitionChunkOffset::from(i);
        insert_requested(&mut orch, offset, peer);
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .unwrap();
    }

    assert_eq!(orch.unblock_missing_data_root_index(0), 0);
    assert!(orch.chunk_requests.values().all(|r| {
        matches!(
            r.request_state,
            ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
        )
    }));

    assert_eq!(orch.unblock_missing_data_root_index(2), 2);
    let pending = orch
        .chunk_requests
        .values()
        .filter(|r| matches!(r.request_state, ChunkRequestState::Pending))
        .count();
    let still_blocked = orch
        .chunk_requests
        .values()
        .filter(|r| {
            matches!(
                r.request_state,
                ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
            )
        })
        .count();
    assert_eq!(pending, 2);
    assert_eq!(still_blocked, 1);
}

/// With Blocked offsets {5, 1, 3} and `max = 2`, the lowest two offsets
/// (1, 3) must be unblocked — not an arbitrary HashMap-order pair.
#[test_log::test(tokio::test)]
async fn unblock_missing_data_root_index_picks_lowest_offsets_first() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 8;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([7_u8; 20]);

    for i in [5_u32, 1_u32, 3_u32] {
        let offset = PartitionChunkOffset::from(i);
        insert_requested(&mut orch, offset, peer);
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .unwrap();
    }

    let n = orch.unblock_missing_data_root_index(2);
    assert_eq!(n, 2);
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(1_u32)].request_state,
        ChunkRequestState::Pending,
        "lowest offset must be unblocked"
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(3_u32)].request_state,
        ChunkRequestState::Pending,
        "second-lowest offset must be unblocked"
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(5_u32)].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex),
        "highest offset must remain Blocked when capped below the full backlog"
    );
}

/// `is_ready` must gate which Blocked offsets re-enter Pending — unready
/// offsets stay Blocked even when free-slot budget remains.
#[test_log::test(tokio::test)]
async fn unblock_where_skips_offsets_that_are_not_ready() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 8;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([7_u8; 20]);

    for i in 0..4_u32 {
        let offset = PartitionChunkOffset::from(i);
        insert_requested(&mut orch, offset, peer);
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .unwrap();
    }

    // Only even offsets are "index ready". Probe budget covers the full map.
    let n = orch.unblock_missing_data_root_index_where(10, 10, |off| *off % 2 == 0);
    assert_eq!(n, 2);
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(0_u32)].request_state,
        ChunkRequestState::Pending
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(1_u32)].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(2_u32)].request_state,
        ChunkRequestState::Pending
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(3_u32)].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
    );
}

/// Cap applies only to ready offsets: with max=1 and ready={0,2}, only the
/// lowest ready offset unblocks.
#[test_log::test(tokio::test)]
async fn unblock_where_respects_cap_among_ready_offsets() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 8;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([7_u8; 20]);

    for i in [0_u32, 1, 2] {
        let offset = PartitionChunkOffset::from(i);
        insert_requested(&mut orch, offset, peer);
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .unwrap();
    }

    let n = orch.unblock_missing_data_root_index_where(1, 10, |off| *off != 1);
    assert_eq!(n, 1);
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(0_u32)].request_state,
        ChunkRequestState::Pending
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(1_u32)].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
    );
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(2_u32)].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex),
        "second ready offset must wait for a later re-arm pass"
    );
}

/// Probe budget stops after `max_probes` readiness checks even when free-slot
/// budget remains — a large unready prefix cannot force a full-map walk.
#[test_log::test(tokio::test)]
async fn unblock_where_respects_max_probes() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 16;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([7_u8; 20]);

    // Blocked 0..9; only offset 5 is ready. With max=3 free slots but max_probes=3
    // we only look at 0,1,2 — all unready — so nothing unblocks (would need probe 6).
    for i in 0..10_u32 {
        let offset = PartitionChunkOffset::from(i);
        insert_requested(&mut orch, offset, peer);
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .unwrap();
    }

    let n = orch.unblock_missing_data_root_index_where(3, 3, |off| *off == 5);
    assert_eq!(n, 0, "probe budget exhausted before the ready offset");
    assert!(orch.chunk_requests.values().all(|r| {
        matches!(
            r.request_state,
            ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
        )
    }));

    // Wider probe budget reaches offset 5.
    let n = orch.unblock_missing_data_root_index_where(3, 6, |off| *off == 5);
    assert_eq!(n, 1);
    assert_eq!(
        orch.chunk_requests[&PartitionChunkOffset::from(5_u32)].request_state,
        ChunkRequestState::Pending
    );
}
