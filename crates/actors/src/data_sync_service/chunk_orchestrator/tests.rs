use super::*;
use crate::{
    chunk_fetcher::{ChunkFetchFailureKind, MockChunkFetcher},
    metrics,
    test_helpers::build_test_service_senders,
};
use irys_database::{
    IrysDatabaseArgs as _, cache_chunk, cache_data_root, open_or_create_db, tables::IrysTables,
};
use irys_domain::ChunkType;
use irys_domain::{BlockTree, StorageModuleInfo};
use irys_testing_utils::TempDirBuilder;
use irys_types::{
    Base64, Config, ConsensusConfig, DataLedger, DataTransaction, DataTransactionLedger, H256,
    IrysAddress, LedgerChunkOffset, LedgerChunkRange, NodeConfig, PeerAddress, PeerListItem,
    TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider, irys::IrysSigner,
    ledger_chunk_offset_ii, partition::PartitionAssignment, partition_chunk_offset_ie,
};
use nodit::interval::ii;
use std::sync::atomic::{AtomicU64, Ordering};

fn test_config(base_directory: std::path::PathBuf, num_chunks: u64) -> Config {
    test_config_with_pending_limit(base_directory, num_chunks, 1_000)
}

fn test_config_with_pending_limit(
    base_directory: std::path::PathBuf,
    num_chunks: u64,
    max_pending_chunk_requests: u64,
) -> Config {
    let mut node_config = NodeConfig {
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
    node_config.data_sync.max_pending_chunk_requests = max_pending_chunk_requests;
    Config::new_with_random_peer_id(node_config)
}

fn packed_sm(config: &Config, num_chunks: u64) -> Arc<StorageModule> {
    packed_sm_for_ledger(config, num_chunks, DataLedger::Publish)
}

fn packed_sm_for_ledger(
    config: &Config,
    num_chunks: u64,
    ledger: DataLedger,
) -> Arc<StorageModule> {
    let pa = PartitionAssignment {
        ledger_id: Some(ledger.into()),
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

fn open_test_db(config: &Config) -> DatabaseProvider {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = config
        .node_config
        .base_directory
        .join(format!("orchestrator-db-{n}"));
    let env = open_or_create_db(
        path,
        IrysTables::ALL,
        reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
    )
    .expect("open orchestrator test db");
    DatabaseProvider(Arc::new(env))
}

fn make_orchestrator(sm: Arc<StorageModule>, config: &Config) -> ChunkOrchestrator {
    let db = open_test_db(config);
    make_orchestrator_with_db(sm, config, db).0
}

fn make_orchestrator_with_db(
    sm: Arc<StorageModule>,
    config: &Config,
    db: DatabaseProvider,
) -> (ChunkOrchestrator, Arc<MockChunkFetcher>) {
    let genesis = irys_testing_utils::new_mock_signed_header();
    let block_tree = BlockTree::new(&genesis, config.consensus.clone());
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
    let (service_senders, _receivers) = build_test_service_senders();
    let ledger_id: u32 = DataLedger::Publish.into();
    let fetcher = Arc::new(MockChunkFetcher::new(ledger_id as usize));
    let orchestrator = ChunkOrchestrator::new(
        sm,
        Arc::new(RwLock::new(HashMap::new())),
        block_tree_guard,
        &service_senders,
        fetcher.clone(),
        config.node_config.clone(),
        tokio::runtime::Handle::current(),
        db,
    );
    (orchestrator, fetcher)
}

fn make_orchestrator_with_ledger_chunks(
    sm: Arc<StorageModule>,
    config: &Config,
    total_chunks: u64,
) -> ChunkOrchestrator {
    make_orchestrator_with_ledger_total(sm, config, DataLedger::Publish, total_chunks)
}

fn make_orchestrator_with_ledger_total(
    sm: Arc<StorageModule>,
    config: &Config,
    ledger: DataLedger,
    total_chunks: u64,
) -> ChunkOrchestrator {
    use irys_testing_utils::IrysBlockHeaderTestExt as _;

    let mut genesis = irys_testing_utils::new_mock_signed_header();
    genesis.data_ledgers[ledger].total_chunks = total_chunks;
    genesis.test_sign();
    let block_tree = BlockTree::new(&genesis, config.consensus.clone());
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
    let (service_senders, _receivers) = build_test_service_senders();
    let db = open_test_db(config);
    ChunkOrchestrator::new(
        sm,
        Arc::new(RwLock::new(HashMap::new())),
        block_tree_guard,
        &service_senders,
        Arc::new(MockChunkFetcher::new(ledger as usize)),
        config.node_config.clone(),
        tokio::runtime::Handle::current(),
        db,
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
            excluded: HashSet::new(),
            local_cache_miss: false,
            request_state: ChunkRequestState::Requested(peer, Instant::now()),
            attempt_count: 1,
            retry_round: 0,
            ready_since: Instant::now(),
        },
    );
}

fn insert_pending(orch: &mut ChunkOrchestrator, offset: PartitionChunkOffset) {
    let now = Instant::now();
    orch.chunk_requests.insert(
        offset,
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss: false,
            request_state: ChunkRequestState::Pending,
            attempt_count: 0,
            retry_round: 0,
            ready_since: now,
        },
    );
    orch.ready_offsets.push_back(offset);
}

fn add_assigned_peer(
    orch: &mut ChunkOrchestrator,
    config: &Config,
    byte: u8,
    port: u16,
) -> IrysAddress {
    let address = IrysAddress::from([byte; 20]);
    let api = format!("127.0.0.1:{port}").parse().unwrap();
    let item = PeerListItem {
        mining_address: address,
        address: PeerAddress {
            api,
            gossip: api,
            ..Default::default()
        },
        is_online: true,
        ..Default::default()
    };
    let mut manager = PeerBandwidthManager::new(&address, &item, config);
    manager.partition_assignments.push(
        orch.storage_module
            .partition_assignment()
            .expect("assigned storage module"),
    );
    orch.active_sync_peers
        .write()
        .unwrap()
        .insert(address, manager);
    orch.add_peer(address);
    address
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
            excluded: HashSet::new(),
            local_cache_miss: false,
            request_state: ChunkRequestState::Pending,
            attempt_count: 0,
            retry_round: 0,
            ready_since: Instant::now(),
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
    assert!(orch.chunk_requests[&offset].excluded.is_empty());
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
        orch.chunk_requests[&waiting].excluded.is_empty(),
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
            excluded: HashSet::new(),
            local_cache_miss: false,
            request_state: ChunkRequestState::Pending,
            attempt_count: 0,
            retry_round: 0,
            ready_since: Instant::now(),
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
            excluded: HashSet::new(),
            local_cache_miss: false,
            request_state: ChunkRequestState::Pending,
            attempt_count: 0,
            retry_round: 0,
            ready_since: Instant::now(),
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

#[test_log::test(tokio::test)]
async fn failed_offset_returns_to_fifo_tail_after_other_ready_work() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 8);
    let sm = packed_sm(&config, 8);
    let mut orch = make_orchestrator(sm, &config);
    let peer_a = add_assigned_peer(&mut orch, &config, 21, 9021);
    let _peer_b = add_assigned_peer(&mut orch, &config, 22, 9022);
    let failed = PartitionChunkOffset::from(0_u32);
    let later_a = PartitionChunkOffset::from(6_u32);
    let later_b = PartitionChunkOffset::from(7_u32);

    insert_pending(&mut orch, later_a);
    insert_pending(&mut orch, later_b);
    insert_requested(&mut orch, failed, peer_a);
    orch.on_chunk_failed(
        failed,
        peer_a,
        crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
    )
    .unwrap();

    assert_eq!(
        orch.ready_offsets.iter().copied().collect::<Vec<_>>(),
        vec![later_a, later_b, failed],
        "a failed sparse-prefix offset must yield to every offset already ready"
    );
}

#[test_log::test(tokio::test)]
async fn not_found_rotates_peer_without_global_health_penalty() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm, &config);
    let peer_a = add_assigned_peer(&mut orch, &config, 31, 9031);
    let peer_b = add_assigned_peer(&mut orch, &config, 32, 9032);
    let offset = PartitionChunkOffset::from(0_u32);
    insert_requested(&mut orch, offset, peer_a);
    orch.active_sync_peers
        .write()
        .unwrap()
        .get_mut(&peer_a)
        .unwrap()
        .on_chunk_request_started();

    orch.on_chunk_failed(
        offset,
        peer_a,
        crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
    )
    .unwrap();

    let request = &orch.chunk_requests[&offset];
    assert!(matches!(request.request_state, ChunkRequestState::Pending));
    assert!(request.excluded.contains(&peer_a));
    assert!(!request.excluded.contains(&peer_b));
    assert_eq!(orch.find_best_peer(Some(&request.excluded)), Some(peer_b));
    let peers = orch.active_sync_peers.read().unwrap();
    let peer_a_stats = &peers[&peer_a];
    assert_eq!(peer_a_stats.active_requests(), 0);
    assert_eq!(peer_a_stats.total_failures(), 0);
}

#[test_log::test(tokio::test)]
async fn transport_failure_releases_concurrency_and_penalizes_peer() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm, &config);
    let peer = add_assigned_peer(&mut orch, &config, 33, 9033);
    let offset = PartitionChunkOffset::from(0_u32);
    insert_requested(&mut orch, offset, peer);
    orch.active_sync_peers
        .write()
        .unwrap()
        .get_mut(&peer)
        .unwrap()
        .on_chunk_request_started();

    orch.on_chunk_failed(
        offset,
        peer,
        crate::chunk_fetcher::ChunkFetchFailureKind::Transport,
    )
    .unwrap();

    let peers = orch.active_sync_peers.read().unwrap();
    assert_eq!(peers[&peer].active_requests(), 0);
    assert_eq!(peers[&peer].total_failures(), 1);
}

#[test_log::test(tokio::test)]
async fn exhausting_all_peers_delays_then_rearms_offset() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm, &config);
    let peer_a = add_assigned_peer(&mut orch, &config, 41, 9041);
    let peer_b = add_assigned_peer(&mut orch, &config, 42, 9042);
    let offset = PartitionChunkOffset::from(0_u32);

    insert_requested(&mut orch, offset, peer_a);
    orch.on_chunk_failed(
        offset,
        peer_a,
        crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
    )
    .unwrap();
    orch.chunk_requests.get_mut(&offset).unwrap().request_state =
        ChunkRequestState::Requested(peer_b, Instant::now());
    orch.on_chunk_failed(
        offset,
        peer_b,
        crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
    )
    .unwrap();

    let retry_at = match orch.chunk_requests[&offset].request_state {
        ChunkRequestState::Delayed { retry_at } => retry_at,
        ref state => panic!("expected first delayed retry, got {state:?}"),
    };
    assert_eq!(orch.chunk_requests[&offset].retry_round, 1);
    assert_eq!(orch.chunk_requests[&offset].excluded.len(), 2);
    orch.populate_request_queue_at(
        retry_at
            .checked_sub(Duration::from_millis(1))
            .expect("retry deadline is at least one second in the future"),
    );
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Delayed { .. }
    ));

    orch.populate_request_queue_at(retry_at);
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending
    ));
    assert!(orch.chunk_requests[&offset].excluded.is_empty());
    assert_eq!(orch.ready_offsets.back(), Some(&offset));
}

#[test_log::test(tokio::test)]
async fn delayed_retry_metadata_discards_soonest_deadline_within_bound() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 2);
    let sm = packed_sm(&config, 8);
    let mut orch = make_orchestrator(sm, &config);
    let now = Instant::now();

    for value in 0..3_u32 {
        let offset = PartitionChunkOffset::from(value);
        orch.chunk_requests.insert(
            offset,
            ChunkRequest {
                excluded: HashSet::new(),
                local_cache_miss: false,
                request_state: ChunkRequestState::Delayed {
                    retry_at: now + Duration::from_secs(u64::from(value) + 1),
                },
                attempt_count: 1,
                retry_round: 1,
                ready_since: now,
            },
        );
    }
    orch.trim_delayed_requests();

    assert_eq!(
        orch.chunk_requests
            .values()
            .filter(|request| matches!(request.request_state, ChunkRequestState::Delayed { .. }))
            .count(),
        2
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(0_u32)),
        "the soonest retry has the least suppression left and returns to Entropy reconciliation"
    );
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(2_u32)),
        "the longest remaining backoff must be retained"
    );
}

#[test_log::test(tokio::test)]
async fn dispatch_restores_pending_offset_when_selected_peer_disappears() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm, &config);
    let peer = add_assigned_peer(&mut orch, &config, 52, 9052);
    let offset = PartitionChunkOffset::from(0_u32);
    insert_pending(&mut orch, offset);
    orch.ready_offsets.clear();
    orch.active_sync_peers.write().unwrap().remove(&peer);

    assert!(!orch.dispatch_chunk_request(offset, peer));
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending
    ));
    assert_eq!(orch.ready_offsets.front(), Some(&offset));
}

#[test_log::test(tokio::test)]
async fn production_shaped_sparse_prefix_yields_to_transaction_range() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 8);
    let sm = packed_sm_for_ledger(&config, 8, DataLedger::Submit);
    let mut orch = make_orchestrator(sm, &config);
    let peer = add_assigned_peer(&mut orch, &config, 61, 9061);

    for value in 0..11_u32 {
        let offset = PartitionChunkOffset::from(value);
        insert_requested(&mut orch, offset, peer);
        orch.on_chunk_failed(
            offset,
            peer,
            crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
        )
        .unwrap();
    }

    for value in 168_586..=174_797_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    let first = orch.ready_offsets.front().copied();
    let last = orch.ready_offsets.back().copied();
    assert_eq!(first, Some(PartitionChunkOffset::from(168_586_u32)));
    assert_eq!(last, Some(PartitionChunkOffset::from(174_797_u32)));
    assert_eq!(orch.ready_offsets.len(), 6_212);
    assert_eq!(orch.get_metrics().delayed_requests, 11);
}

#[test_log::test(tokio::test)]
async fn delayed_prefix_frees_discovery_budget_for_later_entropy() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 2);
    let sm = packed_sm(&config, 8);
    let mut orch = make_orchestrator_with_ledger_chunks(sm, &config, 8);
    let peer = add_assigned_peer(&mut orch, &config, 71, 9071);

    orch.populate_request_queue();
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(0_u32))
    );
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(1_u32))
    );

    for value in 0..2_u32 {
        let offset = PartitionChunkOffset::from(value);
        orch.chunk_requests.get_mut(&offset).unwrap().request_state =
            ChunkRequestState::Requested(peer, Instant::now());
        orch.on_chunk_failed(
            offset,
            peer,
            crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
        )
        .unwrap();
    }

    orch.populate_request_queue();
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(2_u32))
            && orch
                .chunk_requests
                .contains_key(&PartitionChunkOffset::from(3_u32)),
        "delayed sparse-prefix work must not consume the hot discovery budget"
    );
    assert_eq!(orch.scan_cursor, 4);
}

#[test]
fn retry_backoff_is_exponential_jittered_and_capped() {
    let offset = PartitionChunkOffset::from(17_u32);
    let delays: Vec<_> = (1..=8)
        .map(|round| ChunkOrchestrator::retry_delay(offset, round))
        .collect();
    assert!(delays.windows(2).all(|pair| pair[1] >= pair[0]));
    assert!(delays[0] >= RETRY_BACKOFF_INITIAL);
    assert_eq!(*delays.last().unwrap(), RETRY_BACKOFF_MAX);
}

#[test_log::test(tokio::test)]
async fn restart_recovers_from_durable_storage_and_entropy_intervals() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 2);
    let sm = packed_sm(&config, 8);
    let mut before_restart = make_orchestrator_with_ledger_chunks(Arc::clone(&sm), &config, 8);
    let peer = add_assigned_peer(&mut before_restart, &config, 81, 9081);
    before_restart.populate_request_queue();

    let complete = PartitionChunkOffset::from(0_u32);
    let unresolved = PartitionChunkOffset::from(1_u32);
    store_durable_data(&sm, complete, 32);
    before_restart
        .chunk_requests
        .get_mut(&unresolved)
        .unwrap()
        .request_state = ChunkRequestState::Requested(peer, Instant::now());
    before_restart
        .on_chunk_failed(
            unresolved,
            peer,
            crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
        )
        .unwrap();
    assert!(matches!(
        before_restart.chunk_requests[&unresolved].request_state,
        ChunkRequestState::Delayed { .. }
    ));
    drop(before_restart);

    let mut after_restart = make_orchestrator_with_ledger_chunks(sm, &config, 8);
    after_restart.populate_request_queue();
    assert!(
        !after_restart.chunk_requests.contains_key(&complete),
        "durably stored data must remain complete without scheduler metadata"
    );
    assert!(after_restart.chunk_requests.contains_key(&unresolved));
    assert!(
        after_restart
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(2_u32)),
        "restart must still discover work after the formerly delayed offset"
    );
}

/// If the prefix is already Data and the hot budget is one slot, a low-to-high
/// Entropy walk queues `frontier-1` and never discovers the write head. That
/// is the "replica is one chunk below the frontier" shape: everything except
/// the newest migrated offset is durable. Term ledgers must queue the write
/// head first.
#[test_log::test(tokio::test)]
async fn term_ledger_hot_budget_queues_the_write_head_before_the_prefix() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 1);
    let sm = packed_sm_for_ledger(&config, 8, DataLedger::Submit);
    for value in 0..6_u32 {
        store_durable_data(&sm, PartitionChunkOffset::from(value), 32);
    }
    let mut orch = make_orchestrator_with_ledger_total(sm, &config, DataLedger::Submit, 8);
    orch.populate_request_queue();

    let frontier = PartitionChunkOffset::from(7_u32);
    let just_behind = PartitionChunkOffset::from(6_u32);
    assert_eq!(
        orch.ready_offsets.front().copied(),
        Some(frontier),
        "the term-ledger write head must dispatch before the remaining prefix hole"
    );
    assert!(orch.chunk_requests.contains_key(&frontier));
    assert!(
        !orch.chunk_requests.contains_key(&just_behind),
        "a one-slot hot budget must not be spent on frontier-1 while the write head is still Entropy"
    );
}

#[test_log::test(tokio::test)]
async fn publish_hot_budget_still_walks_from_the_slot_start() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 1);
    let sm = packed_sm(&config, 8);
    let mut orch = make_orchestrator_with_ledger_chunks(sm, &config, 8);
    orch.populate_request_queue();

    assert_eq!(
        orch.ready_offsets.front().copied(),
        Some(PartitionChunkOffset::from(0_u32)),
        "Publish must keep filling from the start of the slot"
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(7_u32)),
        "Publish must not spend its only hot slot on the write head"
    );
}

/// Term-ledger gaps at low offsets are often abandoned uploads. Discovery
/// must walk down from the write head so a small hot budget is spent on
/// frontier Entropy, not on the prefix.
#[test_log::test(tokio::test)]
async fn term_ledger_scan_walks_from_the_frontier_downward() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 2);
    let sm = packed_sm_for_ledger(&config, 8, DataLedger::Submit);
    let mut orch = make_orchestrator_with_ledger_total(sm, &config, DataLedger::Submit, 8);
    orch.populate_request_queue();

    assert_eq!(
        orch.ready_offsets.front().copied(),
        Some(PartitionChunkOffset::from(7_u32))
    );
    assert_eq!(
        orch.ready_offsets.back().copied(),
        Some(PartitionChunkOffset::from(6_u32))
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(0_u32)),
        "a two-slot budget must not be spent on the abandoned-upload prefix"
    );
}

/// After the frontier has been scanned, the reverse walk wraps and still
/// probes low-offset Entropy — just not first.
#[test_log::test(tokio::test)]
async fn term_ledger_reverse_scan_visits_the_prefix_after_the_frontier() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, 2);
    let sm = packed_sm_for_ledger(&config, 8, DataLedger::Submit);
    let mut orch = make_orchestrator_with_ledger_total(sm, &config, DataLedger::Submit, 8);
    let peer = add_assigned_peer(&mut orch, &config, 91, 9091);

    orch.populate_request_queue();
    for value in [7_u32, 6_u32] {
        let offset = PartitionChunkOffset::from(value);
        orch.chunk_requests.get_mut(&offset).unwrap().request_state =
            ChunkRequestState::Requested(peer, Instant::now());
        orch.on_chunk_failed(
            offset,
            peer,
            crate::chunk_fetcher::ChunkFetchFailureKind::NotFound,
        )
        .unwrap();
    }

    orch.populate_request_queue();
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(5_u32))
            && orch
                .chunk_requests
                .contains_key(&PartitionChunkOffset::from(4_u32)),
        "after the frontier is delayed, reverse discovery must continue downward"
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(0_u32)),
        "the prefix must still wait until higher offsets have been scanned"
    );
}

fn signed_tx(config: &Config, num_chunks: u64) -> (DataTransaction, Vec<u8>) {
    let chunk_size = usize::try_from(config.consensus.chunk_size).expect("chunk size");
    let data = vec![7_u8; chunk_size * usize::try_from(num_chunks).expect("chunk count")];
    let signer = IrysSigner::random_signer(&config.consensus);
    let tx = signer
        .create_transaction(data.clone(), H256::zero())
        .expect("transaction");
    let tx = signer.sign_transaction(tx).expect("signed transaction");
    (tx, data)
}

/// Index the tx onto slot 0. Leaves the body-migration row the index write creates.
fn index_tx(sm: &StorageModule, tx: &DataTransaction, last_inclusive: u64) {
    let (_root, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
    sm.index_transaction_data(
        &tx.header,
        &proofs[0].proof,
        LedgerChunkRange(ledger_chunk_offset_ii!(0, last_inclusive)),
        0,
    )
    .expect("index tx");
}

/// Index the tx onto slot 0 and delete the body-migration row the index write
/// creates. The row's absence is what lets data sync own the hole.
fn index_and_settle(sm: &StorageModule, tx: &DataTransaction, last_inclusive: u64) {
    index_tx(sm, tx, last_inclusive);
    for (key, job) in sm.pending_body_migrations().expect("pending rows") {
        assert!(
            sm.settle_pending_body_migration(key, job.data_root, job.block_height)
                .expect("settle")
        );
    }
}

fn cache_bodies_at(db: &DatabaseProvider, tx: &DataTransaction, data: &[u8], offsets: &[usize]) {
    db.update_eyre(|wtx| {
        cache_data_root(wtx, &tx.header, None)?;
        for &i in offsets {
            let node = &tx.chunks[i];
            let chunk = UnpackedChunk {
                data_root: tx.header.data_root,
                data_size: tx.header.data_size,
                data_path: Base64(tx.proofs[i].proof.clone()),
                bytes: Base64(data[node.min_byte_range..node.max_byte_range].to_vec()),
                tx_offset: TxChunkOffset::from(u32::try_from(i).expect("tx offset")),
            };
            cache_chunk(wtx, &chunk)?;
        }
        Ok(())
    })
    .expect("cache bodies");
}

fn strip_cached_body(db: &DatabaseProvider, tx: &DataTransaction) {
    use irys_database::db_cache::CachedChunk;
    use irys_database::tables::CachedChunks;
    use reth_db::transaction::{DbTx as _, DbTxMut as _};

    let path_hash = UnpackedChunk::hash_data_path(&tx.proofs[0].proof);
    db.update_eyre(|wtx| {
        let existing = wtx
            .get::<CachedChunks>(path_hash)?
            .expect("cached chunk row");
        wtx.put::<CachedChunks>(
            path_hash,
            CachedChunk {
                chunk: None,
                data_path: existing.data_path,
            },
        )?;
        Ok(())
    })
    .expect("strip cached body");
}

async fn wait_for_ledger_requests(fetcher: &MockChunkFetcher, n: usize) {
    for _ in 0..100 {
        if fetcher.request_log.read().unwrap().len() >= n {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for {n} peer chunk requests");
}

#[test_log::test(tokio::test)]
async fn local_cache_hit_writes_without_a_peer_request() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 1);
    index_and_settle(&sm, &tx, 0);
    cache_bodies_at(&db, &tx, &data, &[0]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    add_assigned_peer(&mut orch, &config, 11, 9111);
    let offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, offset);

    let before = metrics::test_fetch_by_source_count("local_cache", "success");
    assert!(orch.dispatch_next());
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(
        fetcher.request_log.read().unwrap().is_empty(),
        "a cached body must not ask a peer"
    );
    assert!(fetcher.data_root_request_log.read().unwrap().is_empty());
    assert!(sm.is_data_write_pending_at(offset));
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::AwaitingDurability(_)
    ));
    assert!(!orch.chunk_requests[&offset].local_cache_miss);
    assert!(metrics::test_fetch_by_source_count("local_cache", "success") > before);
}

#[test_log::test(tokio::test)]
async fn local_cache_hit_writes_when_no_peer_is_connected() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 1);
    index_and_settle(&sm, &tx, 0);
    cache_bodies_at(&db, &tx, &data, &[0]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    let offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, offset);

    assert!(orch.dispatch_next());
    assert!(fetcher.request_log.read().unwrap().is_empty());
    assert!(sm.is_data_write_pending_at(offset));
}

#[test_log::test(tokio::test)]
async fn local_cache_contiguous_run_writes_hits_around_a_hole() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 3);
    index_and_settle(&sm, &tx, 2);
    cache_bodies_at(&db, &tx, &data, &[0, 2]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    add_assigned_peer(&mut orch, &config, 12, 9112);
    for value in 0..3_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    let before = metrics::test_fetch_by_source_count("local_cache", "success");
    assert!(orch.dispatch_next());
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert!(fetcher.request_log.read().unwrap().is_empty());
    assert!(sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
    assert!(!sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));
    assert!(sm.is_data_write_pending_at(PartitionChunkOffset::from(2)));
    assert!(orch.chunk_requests[&PartitionChunkOffset::from(1)].local_cache_miss);
    assert_eq!(
        orch.ready_offsets.front(),
        Some(&PartitionChunkOffset::from(1))
    );
    assert!(metrics::test_fetch_by_source_count("local_cache", "success") >= before + 2);
}

#[test_log::test(tokio::test)]
async fn local_cache_miss_sticks_for_one_peer_cycle_then_retries_the_cache() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 1);
    index_and_settle(&sm, &tx, 0);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db.clone());
    add_assigned_peer(&mut orch, &config, 13, 9113);
    add_assigned_peer(&mut orch, &config, 14, 9114);
    let offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, offset);

    let before = metrics::test_fetch_by_source_count("local_cache", "not_found");
    assert!(orch.dispatch_next());
    wait_for_ledger_requests(&fetcher, 1).await;
    assert!(orch.chunk_requests[&offset].local_cache_miss);
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Requested(_, _)
    ));
    assert!(metrics::test_fetch_by_source_count("local_cache", "not_found") > before);
    assert!(!sm.is_data_write_pending_at(offset));

    let requested = match orch.chunk_requests[&offset].request_state {
        ChunkRequestState::Requested(addr, _) => addr,
        ref other => panic!("expected Requested, got {other:?}"),
    };
    orch.on_chunk_failed(offset, requested, ChunkFetchFailureKind::NotFound)
        .expect("peer failure");
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending
    ));

    cache_bodies_at(&db, &tx, &data, &[0]);
    assert!(orch.dispatch_next());
    wait_for_ledger_requests(&fetcher, 2).await;
    assert!(
        !sm.is_data_write_pending_at(offset),
        "a memoized miss must not read the cache again"
    );
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Requested(_, _)
    ));
    assert!(orch.chunk_requests[&offset].local_cache_miss);

    let requested = match orch.chunk_requests[&offset].request_state {
        ChunkRequestState::Requested(addr, _) => addr,
        ref other => panic!("expected Requested, got {other:?}"),
    };
    orch.on_chunk_failed(offset, requested, ChunkFetchFailureKind::NotFound)
        .expect("second peer failure");
    let retry_at = match orch.chunk_requests[&offset].request_state {
        ChunkRequestState::Delayed { retry_at } => retry_at,
        ref other => panic!("expected Delayed, got {other:?}"),
    };
    assert!(
        orch.chunk_requests[&offset].local_cache_miss,
        "the backoff itself keeps the memo"
    );

    orch.populate_request_queue_at(retry_at);
    assert!(!orch.chunk_requests[&offset].local_cache_miss);
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending
    ));
    assert!(orch.dispatch_next());
    assert!(
        sm.is_data_write_pending_at(offset),
        "a body cached during the backoff is written on the next cycle"
    );
}

#[test_log::test(tokio::test)]
async fn local_cache_index_only_row_is_a_miss() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 1);
    index_and_settle(&sm, &tx, 0);
    cache_bodies_at(&db, &tx, &data, &[0]);
    strip_cached_body(&db, &tx);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    add_assigned_peer(&mut orch, &config, 15, 9115);
    let offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, offset);

    let before = metrics::test_fetch_by_source_count("local_cache", "not_found");
    assert!(orch.dispatch_next());
    wait_for_ledger_requests(&fetcher, 1).await;
    assert!(orch.chunk_requests[&offset].local_cache_miss);
    assert!(!sm.is_data_write_pending_at(offset));
    assert!(metrics::test_fetch_by_source_count("local_cache", "not_found") > before);
}

/// A retired body job clears the cache-miss memo and leaves the hole in its
/// queue place. The next dispatch writes the cached body. An offset that is
/// not entropy, and a fetch already in flight, are left alone.
#[test_log::test(tokio::test)]
async fn retired_holes_clear_the_cache_memo_and_are_written_locally() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 1);
    index_and_settle(&sm, &tx, 0);
    cache_bodies_at(&db, &tx, &data, &[0]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    let offset = PartitionChunkOffset::from(0);
    let later = PartitionChunkOffset::from(2);
    insert_pending(&mut orch, later);
    insert_pending(&mut orch, offset);
    orch.chunk_requests
        .get_mut(&offset)
        .unwrap()
        .local_cache_miss = true;
    orch.chunk_requests.get_mut(&later).unwrap().request_state =
        ChunkRequestState::Requested(IrysAddress::ZERO, Instant::now());
    orch.chunk_requests
        .get_mut(&later)
        .unwrap()
        .local_cache_miss = true;

    orch.queue_body_migration_holes(&[later, offset, PartitionChunkOffset::from(100)]);
    assert!(!orch.chunk_requests[&offset].local_cache_miss);
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Pending
    ));
    assert_eq!(orch.ready_offsets.front(), Some(&later));
    assert!(orch.ready_offsets.contains(&offset));
    assert!(matches!(
        orch.chunk_requests[&later].request_state,
        ChunkRequestState::Requested(_, _)
    ));
    assert!(orch.chunk_requests[&later].local_cache_miss);
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(100))
    );

    assert!(orch.dispatch_next());
    assert!(fetcher.request_log.read().unwrap().is_empty());
    assert!(sm.is_data_write_pending_at(offset));
}

/// A live body-migration job covers every offset in its span, not only the
/// row key. Data sync holds those offsets and still asks a peer for a hole
/// the job does not own. After the job is settled, the held offsets are
/// written from the cache.
#[test_log::test(tokio::test)]
async fn pending_body_offsets_are_skipped_until_the_job_settles() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 3);
    index_tx(&sm, &tx, 2);
    cache_bodies_at(&db, &tx, &data, &[0, 1, 2]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    add_assigned_peer(&mut orch, &config, 16, 9116);
    for value in 0..4_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    assert!(orch.dispatch_next());
    for value in 0..3_u32 {
        let offset = PartitionChunkOffset::from(value);
        assert!(
            matches!(
                orch.chunk_requests[&offset].request_state,
                ChunkRequestState::HeldByBodyWorker
            ),
            "offset {value} is inside the live job"
        );
        assert_eq!(orch.chunk_requests[&offset].attempt_count, 0);
        assert!(!orch.chunk_requests[&offset].local_cache_miss);
        assert!(!sm.is_data_write_pending_at(offset));
    }
    assert_eq!(orch.holds_started, 3, "each covered offset is counted once");
    wait_for_ledger_requests(&fetcher, 1).await;
    {
        let log = fetcher.request_log.read().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(*log[0].0, 3, "only the uncovered hole asks a peer");
    }
    assert!(fetcher.data_root_request_log.read().unwrap().is_empty());

    assert!(!orch.dispatch_next());
    assert_eq!(orch.holds_started, 3);
    assert_eq!(fetcher.request_log.read().unwrap().len(), 1);

    for (key, job) in sm.pending_body_migrations().expect("pending rows") {
        assert!(
            sm.settle_pending_body_migration(key, job.data_root, job.block_height)
                .expect("settle")
        );
    }
    orch.prepare_tick();
    for _ in 0..3 {
        let waiting = (0..3_u32).all(|value| {
            matches!(
                orch.chunk_requests[&PartitionChunkOffset::from(value)].request_state,
                ChunkRequestState::AwaitingDurability(_)
            )
        });
        if waiting {
            break;
        }
        assert!(orch.dispatch_next());
    }
    for value in 0..3_u32 {
        let offset = PartitionChunkOffset::from(value);
        assert!(sm.is_data_write_pending_at(offset));
        assert!(matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::AwaitingDurability(_)
        ));
        assert!(!orch.chunk_requests[&offset].local_cache_miss);
    }
    assert_eq!(fetcher.request_log.read().unwrap().len(), 1);
    assert_eq!(orch.holds_started, 3);
}

/// A hold that sees no row-key movement is released to peers. The clock is
/// the monotonic time since the hold started. One second inside the timeout
/// stays held. Past the timeout, every covered offset is Pending and counted.
#[test_log::test(tokio::test)]
async fn pending_body_hold_expires_and_fetches_from_a_peer() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let (tx, _data) = signed_tx(&config, 3);
    index_tx(&sm, &tx, 2);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm, &config, open_test_db(&config));
    add_assigned_peer(&mut orch, &config, 17, 9117);
    for value in 0..3_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    assert!(!orch.dispatch_next());
    for value in 0..3_u32 {
        assert!(matches!(
            orch.chunk_requests[&PartitionChunkOffset::from(value)].request_state,
            ChunkRequestState::HeldByBodyWorker
        ));
    }

    let timeout = config.node_config.data_sync.pending_body_hold_timeout;
    orch.release_body_worker_holds(Instant::now() + timeout - Duration::from_secs(1));
    assert_eq!(orch.hold_expired, 0);
    for value in 0..3_u32 {
        assert!(
            matches!(
                orch.chunk_requests[&PartitionChunkOffset::from(value)].request_state,
                ChunkRequestState::HeldByBodyWorker
            ),
            "offset {value} is still inside the timeout"
        );
    }

    orch.release_body_worker_holds(Instant::now() + timeout + Duration::from_secs(1));
    assert_eq!(orch.hold_expired, 3);
    for value in 0..3_u32 {
        let offset = PartitionChunkOffset::from(value);
        assert!(matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::Pending
        ));
        assert!(orch.body_holds[&offset].expired);
    }

    assert!(orch.dispatch_next());
    wait_for_ledger_requests(&fetcher, 1).await;
    assert!(
        !orch
            .chunk_requests
            .values()
            .any(|request| matches!(request.request_state, ChunkRequestState::HeldByBodyWorker)),
        "an expired hold is not taken again for the same row key"
    );
}

/// Moving the pending row key restarts the hold for the offsets that key
/// still covers. The prefix the worker already left is fetched. The restart
/// is the range start, not the job's attempt count.
#[test_log::test(tokio::test)]
async fn pending_body_hold_resets_when_the_row_key_advances() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let (tx, _data) = signed_tx(&config, 3);
    index_tx(&sm, &tx, 2);
    let (mut orch, _fetcher) =
        make_orchestrator_with_db(sm.clone(), &config, open_test_db(&config));
    for value in 0..3_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }
    assert!(!orch.dispatch_next());

    let jobs = sm.pending_body_migrations().expect("pending rows");
    assert_eq!(jobs.len(), 1);
    let (key, job) = &jobs[0];
    assert_eq!(*key, PartitionChunkOffset::from(0));
    let landed = sm
        .advance_pending_body_migration(*key, PartitionChunkOffset::from(1), job)
        .expect("advance");
    assert_eq!(landed, PartitionChunkOffset::from(1));

    orch.reload_pending_body_coverage();
    assert_eq!(orch.pending_body_ranges, vec![(1, 2)]);
    orch.release_body_worker_holds(Instant::now() + Duration::from_secs(60 * 60));

    assert_eq!(orch.hold_expired, 0);
    assert!(matches!(
        orch.chunk_requests[&PartitionChunkOffset::from(0)].request_state,
        ChunkRequestState::Pending
    ));
    assert!(!orch.body_holds.contains_key(&PartitionChunkOffset::from(0)));
    for value in 1..3_u32 {
        let offset = PartitionChunkOffset::from(value);
        assert!(
            matches!(
                orch.chunk_requests[&offset].request_state,
                ChunkRequestState::HeldByBodyWorker
            ),
            "offset {value} is still covered by the advanced key"
        );
        assert_eq!(orch.body_holds[&offset].anchor, 1);
        assert!(!orch.body_holds[&offset].expired);
    }
}

/// A coverage read error is one attempt per tick. Dispatch does not read
/// again, and the offsets are fetched instead of held.
#[test_log::test(tokio::test)]
async fn pending_body_coverage_read_error_does_not_spin() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let (tx, _data) = signed_tx(&config, 3);
    index_tx(&sm, &tx, 2);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm, &config, open_test_db(&config));
    add_assigned_peer(&mut orch, &config, 18, 9118);
    for value in 0..4_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    orch.fail_coverage_read = true;
    assert!(orch.dispatch_next());
    assert_eq!(orch.coverage_reads, 1);
    assert!(orch.pending_body_ranges.is_empty());
    for value in 0..4_u32 {
        assert!(
            !matches!(
                orch.chunk_requests[&PartitionChunkOffset::from(value)].request_state,
                ChunkRequestState::HeldByBodyWorker
            ),
            "offset {value} is not held when coverage could not be read"
        );
    }
    for _ in 0..5 {
        let _ = orch.dispatch_next();
    }
    assert_eq!(orch.coverage_reads, 1);
    wait_for_ledger_requests(&fetcher, 1).await;

    orch.fail_coverage_read = false;
    orch.prepare_tick();
    assert_eq!(orch.coverage_reads, 2);
}

/// Retired holes join the tail and stop at the hot budget. The offset already
/// at the front stays there. Offsets past the budget are not tracked.
#[test_log::test(tokio::test)]
async fn retired_holes_keep_the_write_head_and_the_hot_budget() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 4, 2);
    let sm = packed_sm_for_ledger(&config, 4, DataLedger::Submit);
    let (mut orch, _fetcher) = make_orchestrator_with_db(sm, &config, open_test_db(&config));
    let head = PartitionChunkOffset::from(3);
    insert_pending(&mut orch, head);

    orch.queue_body_migration_holes(&[
        PartitionChunkOffset::from(0),
        PartitionChunkOffset::from(1),
        PartitionChunkOffset::from(2),
    ]);

    assert_eq!(orch.ready_offsets.front(), Some(&head));
    assert_eq!(
        orch.ready_offsets.back(),
        Some(&PartitionChunkOffset::from(0))
    );
    assert_eq!(orch.ready_offsets.len(), 2);
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(0))
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(1))
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(2))
    );
}

/// A blocked offset keeps its reason. The handoff only clears the cache memo.
#[test_log::test(tokio::test)]
async fn retired_hole_does_not_unblock_a_missing_index() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let (mut orch, _fetcher) = make_orchestrator_with_db(sm, &config, open_test_db(&config));
    let offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, offset);
    orch.chunk_requests.get_mut(&offset).unwrap().request_state =
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex);
    orch.chunk_requests
        .get_mut(&offset)
        .unwrap()
        .local_cache_miss = true;
    orch.ready_offsets.clear();

    orch.queue_body_migration_holes(&[offset]);

    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
    ));
    assert!(!orch.chunk_requests[&offset].local_cache_miss);
    assert!(!orch.ready_offsets.contains(&offset));
}

/// After the job is gone, only as many held offsets as free hot slots become
/// Pending. A later tick takes the next one once a slot is free.
#[test_log::test(tokio::test)]
async fn release_promotes_held_offsets_only_up_to_the_hot_budget() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 4, 1);
    let sm = packed_sm(&config, 4);
    let (tx, _data) = signed_tx(&config, 3);
    index_tx(&sm, &tx, 2);
    let (mut orch, _fetcher) =
        make_orchestrator_with_db(sm.clone(), &config, open_test_db(&config));
    for value in 0..3_u32 {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }
    assert!(!orch.dispatch_next());

    for (key, job) in sm.pending_body_migrations().expect("pending rows") {
        assert!(
            sm.settle_pending_body_migration(key, job.data_root, job.block_height)
                .expect("settle")
        );
    }
    orch.prepare_tick();
    assert_eq!(count_state(&orch, &ChunkRequestState::Pending), 1);
    assert_eq!(count_state(&orch, &ChunkRequestState::HeldByBodyWorker), 2);

    orch.prepare_tick();
    assert_eq!(count_state(&orch, &ChunkRequestState::Pending), 1);
    assert_eq!(count_state(&orch, &ChunkRequestState::HeldByBodyWorker), 2);

    let pending = orch
        .chunk_requests
        .iter()
        .find(|(_, request)| matches!(request.request_state, ChunkRequestState::Pending))
        .map(|(offset, _)| *offset)
        .expect("one pending offset");
    orch.chunk_requests.get_mut(&pending).unwrap().request_state = ChunkRequestState::Completed;
    orch.prepare_tick();
    assert_eq!(count_state(&orch, &ChunkRequestState::Pending), 1);
    assert_eq!(count_state(&orch, &ChunkRequestState::HeldByBodyWorker), 1);
}

/// One tick does at most the configured number of cache lookups. The next
/// offset is put back and written after the next tick resets the count.
#[test_log::test(tokio::test)]
async fn local_cache_lookups_stop_at_the_per_tick_cap() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let mut node_config = NodeConfig {
        consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: 4,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            entropy_packing_iterations: 1,
            block_migration_depth: 1,
            chain_id: 1,
            ..ConsensusConfig::testing()
        }),
        base_directory: tmp.path().to_path_buf(),
        ..NodeConfig::testing()
    };
    node_config.data_sync.max_local_cache_hits_per_tick = 1;
    let chunk_size = node_config.consensus_config().chunk_size;
    node_config.storage.entropy_sweep_max_bytes = chunk_size;
    let config = Config::new_with_random_peer_id(node_config);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 2);
    index_and_settle(&sm, &tx, 1);
    cache_bodies_at(&db, &tx, &data, &[0, 1]);
    let (mut orch, _fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    let first = PartitionChunkOffset::from(0);
    let second = PartitionChunkOffset::from(1);
    insert_pending(&mut orch, first);
    insert_pending(&mut orch, second);

    assert!(orch.dispatch_next());
    assert!(sm.is_data_write_pending_at(first));
    assert!(!sm.is_data_write_pending_at(second));
    assert_eq!(orch.cache_lookups_this_tick, 1);

    assert!(!orch.dispatch_next());
    assert!(!sm.is_data_write_pending_at(second));
    assert_eq!(orch.ready_offsets.front(), Some(&second));
    assert!(matches!(
        orch.chunk_requests[&second].request_state,
        ChunkRequestState::Pending
    ));

    orch.prepare_tick();
    assert_eq!(orch.cache_lookups_this_tick, 0);
    assert!(orch.dispatch_next());
    assert!(sm.is_data_write_pending_at(second));
}

fn count_state(orch: &ChunkOrchestrator, state: &ChunkRequestState) -> usize {
    orch.chunk_requests
        .values()
        .filter(|request| &request.request_state == state)
        .count()
}
