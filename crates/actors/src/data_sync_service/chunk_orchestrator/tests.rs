use super::*;
use crate::{chunk_fetcher::MockChunkFetcher, test_helpers::build_test_service_senders};
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

fn insert_requested(orch: &mut ChunkOrchestrator, offset: PartitionChunkOffset, peer: IrysAddress) {
    orch.chunk_requests.insert(
        offset,
        ChunkRequest {
            ledger_id: 0,
            slot_index: 0,
            chunk_offset: offset,
            excluded: None,
            request_state: ChunkRequestState::Requested(peer, Instant::now()),
        },
    );
}

#[test_log::test(tokio::test)]
async fn mark_helpers_require_requested_state() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    let peer = IrysAddress::from([2_u8; 20]);
    let offset = PartitionChunkOffset::from(0_u32);

    // Unknown offset
    assert!(orch.mark_chunk_stored(offset).is_err());
    assert!(
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .is_err()
    );
    assert!(orch.requeue_after_local_write_failure(offset).is_err());

    // Pending is not Requested
    orch.chunk_requests.insert(
        offset,
        ChunkRequest {
            ledger_id: 0,
            slot_index: 0,
            chunk_offset: offset,
            excluded: None,
            request_state: ChunkRequestState::Pending,
        },
    );
    assert!(orch.mark_chunk_stored(offset).is_err());
    assert!(
        orch.mark_chunk_blocked(offset, ChunkBlockReason::MissingDataRootIndex)
            .is_err()
    );
    assert!(orch.requeue_after_local_write_failure(offset).is_err());

    // Requested accepts each transition
    insert_requested(&mut orch, offset, peer);
    orch.mark_chunk_stored(offset).expect("store");
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
            ledger_id: 0,
            slot_index: 0,
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
            ledger_id: 0,
            slot_index: 0,
            chunk_offset: pending,
            excluded: None,
            request_state: ChunkRequestState::Pending,
        },
    );

    insert_requested(&mut orch, completed, peer);
    orch.mark_chunk_stored(completed).unwrap();

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
