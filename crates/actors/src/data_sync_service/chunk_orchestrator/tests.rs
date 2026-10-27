use super::{DispatchStart, *};
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

fn unpacked_sm_for_ledger(
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
    Arc::new(StorageModule::new(&info, config).expect("storage module"))
}

fn write_synced_chunk(sm: &StorageModule, offset: u32, chunk_size: usize, kind: ChunkType) {
    assert!(
        sm.write_chunk(
            PartitionChunkOffset::from(offset),
            vec![0xD4; chunk_size],
            kind,
        ),
        "write at {offset}"
    );
    sm.force_sync_pending_chunks().expect("sync");
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

fn try_make_orchestrator(
    sm: Arc<StorageModule>,
    config: &Config,
    ledger_id: u32,
) -> Option<ChunkOrchestrator> {
    let db = open_test_db(config);
    let genesis = irys_testing_utils::new_mock_signed_header();
    let block_tree = BlockTree::new(&genesis, config.consensus.clone());
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
    let (service_senders, _receivers) = build_test_service_senders();
    ChunkOrchestrator::try_new_for_ledger(
        sm,
        Arc::new(RwLock::new(HashMap::new())),
        block_tree_guard,
        &service_senders,
        Arc::new(MockChunkFetcher::new(ledger_id as usize)),
        config.node_config.clone(),
        tokio::runtime::Handle::current(),
        db,
        ledger_id,
    )
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
    let db = open_test_db(config);
    make_orchestrator_with_ledger_total_and_db(sm, config, ledger, total_chunks, db)
}

fn make_orchestrator_with_ledger_total_and_db(
    sm: Arc<StorageModule>,
    config: &Config,
    ledger: DataLedger,
    total_chunks: u64,
    db: DatabaseProvider,
) -> ChunkOrchestrator {
    use irys_testing_utils::IrysBlockHeaderTestExt as _;

    let mut genesis = irys_testing_utils::new_mock_signed_header();
    genesis.data_ledgers[ledger].total_chunks = total_chunks;
    genesis.test_sign();
    let block_tree = BlockTree::new(&genesis, config.consensus.clone());
    let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
    let (service_senders, _receivers) = build_test_service_senders();
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

/// Track `offset` in `state`. A Pending offset also joins the FIFO tail.
fn insert_request(
    orch: &mut ChunkOrchestrator,
    offset: PartitionChunkOffset,
    request_state: ChunkRequestState,
    local_cache_miss: bool,
) {
    let pending = matches!(request_state, ChunkRequestState::Pending);
    let delayed = matches!(request_state, ChunkRequestState::Delayed { .. });
    orch.chunk_requests.insert(
        offset,
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss,
            body_in_ingress: false,
            request_state,
            attempt_count: u32::from(!pending),
            retry_round: u32::from(delayed),
            ready_since: Instant::now(),
        },
    );
    if pending {
        orch.ready_offsets.push_back(offset);
    }
}

fn insert_requested(orch: &mut ChunkOrchestrator, offset: PartitionChunkOffset, peer: IrysAddress) {
    insert_request(
        orch,
        offset,
        ChunkRequestState::Requested(peer, Instant::now()),
        false,
    );
}

fn insert_pending(orch: &mut ChunkOrchestrator, offset: PartitionChunkOffset) {
    insert_request(orch, offset, ChunkRequestState::Pending, false);
}

/// A retry whose deadline is `now`. The memo is set so the test can see the
/// new attempt cycle clear it.
fn insert_due(orch: &mut ChunkOrchestrator, offset: PartitionChunkOffset, now: Instant) {
    insert_request(
        orch,
        offset,
        ChunkRequestState::Delayed { retry_at: now },
        true,
    );
}

fn add_assigned_peer(
    orch: &mut ChunkOrchestrator,
    config: &Config,
    byte: u8,
    port: u16,
) -> IrysAddress {
    let assignment = orch
        .storage_module
        .partition_assignment()
        .expect("assigned storage module");
    add_peer_with_assignment(orch, config, byte, port, assignment)
}

fn add_peer_with_assignment(
    orch: &mut ChunkOrchestrator,
    config: &Config,
    byte: u8,
    port: u16,
    assignment: PartitionAssignment,
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
    manager.partition_assignments.push(assignment);
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
            body_in_ingress: false,
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

/// A slow flush is not a stalled write. While the module still holds the
/// write, a timed-out wait keeps waiting instead of fetching the chunk again,
/// up to the pending limit.
#[test_log::test(tokio::test)]
async fn a_write_still_in_the_module_keeps_waiting_past_the_timeout() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm.clone(), &config);
    let peer = IrysAddress::from([8_u8; 20]);
    let waiting = PartitionChunkOffset::from(1_u32);
    let chunk_size = usize::try_from(config.consensus.chunk_size).expect("chunk size");

    insert_requested(&mut orch, waiting, peer);
    orch.mark_chunk_awaiting_durability(waiting).unwrap();
    sm.write_chunk(waiting, vec![9_u8; chunk_size], ChunkType::Data);
    assert!(
        sm.is_data_write_pending_at(waiting),
        "buffered, not flushed"
    );

    let started = Instant::now();
    orch.reconcile_awaiting_durability(
        started + DURABILITY_WAIT_TIMEOUT + Duration::from_secs(1),
        DURABILITY_WAIT_TIMEOUT,
    );
    assert!(
        matches!(
            orch.chunk_requests[&waiting].request_state,
            ChunkRequestState::AwaitingDurability(..)
        ),
        "the write is still pending: fetching again would add nothing"
    );
    assert!(!orch.ready_offsets.contains(&waiting));

    orch.reconcile_awaiting_durability(
        started + DURABILITY_PENDING_LIMIT + Duration::from_secs(1),
        DURABILITY_WAIT_TIMEOUT,
    );
    assert_eq!(
        orch.chunk_requests[&waiting].request_state,
        ChunkRequestState::Pending,
        "past the pending limit the chunk is fetched again"
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
            body_in_ingress: false,
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
            body_in_ingress: false,
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
                body_in_ingress: false,
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

/// A disconnected peer's in-flight offsets go back in scan order, so their
/// neighbours meet at the queue head and share one cache lookup.
#[test_log::test(tokio::test)]
async fn a_disconnected_peers_offsets_requeue_in_scan_order() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 16);
    let sm = packed_sm(&config, 16);
    let mut orch = make_orchestrator(sm, &config);
    let gone = add_assigned_peer(&mut orch, &config, 61, 9161);
    add_assigned_peer(&mut orch, &config, 62, 9162);
    for value in [9_u32, 3, 12, 5, 7, 4] {
        insert_requested(&mut orch, PartitionChunkOffset::from(value), gone);
    }

    orch.remove_peer(gone);

    let queued: Vec<u32> = orch.ready_offsets.iter().map(|offset| offset.0).collect();
    assert_eq!(queued, vec![3, 4, 5, 7, 9, 12], "Publish scans low to high");
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

    assert_eq!(
        orch.dispatch_chunk_request(offset, peer),
        DispatchStart::Skip
    );
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
        .map(|round| ChunkOrchestrator::retry_delay(offset, round, 16))
        .collect();
    assert!(delays.windows(2).all(|pair| pair[1] >= pair[0]));
    assert!(delays[0] >= RETRY_BACKOFF_INITIAL);
    let capped = *delays.last().unwrap();
    assert!(
        capped >= RETRY_BACKOFF_MAX * 3 / 4 && capped <= RETRY_BACKOFF_MAX,
        "the backoff reaches the capped tier: {capped:?}"
    );
}

/// A full delayed set reaches the cap together. Different runs must not share
/// one deadline, or the whole set comes due in one tick. One run must share
/// its deadline, or its retries come due one offset per tick and each costs
/// a cache lookup.
#[test]
fn capped_retry_deadlines_spread_runs_and_keep_each_run_together() {
    const BUCKET: u32 = 16;
    let delay = |value: u32, round: u32| {
        ChunkOrchestrator::retry_delay(PartitionChunkOffset::from(value), round, u64::from(BUCKET))
    };
    let per_run: HashSet<_> = (0..64_u32).map(|run| delay(run * BUCKET, 8)).collect();
    assert!(
        per_run.iter().all(|delay| *delay <= RETRY_BACKOFF_MAX),
        "the cap is still the maximum delay"
    );
    assert!(
        per_run
            .iter()
            .all(|delay| *delay >= RETRY_BACKOFF_MAX * 3 / 4),
        "jitter at the cap opens below it, not near zero"
    );
    assert!(
        per_run.len() > 32,
        "capped runs must not share one deadline: {per_run:?}"
    );
    for round in 1..=8 {
        let run: HashSet<_> = (BUCKET * 5..BUCKET * 6)
            .map(|value| delay(value, round))
            .collect();
        assert_eq!(run.len(), 1, "round {round}: one run, one deadline");
    }
}

/// Due retries are queued in scan order, so adjacent offsets meet at the queue
/// head and one cache lookup can cover them.
#[rstest::rstest]
#[case::publish(DataLedger::Publish, vec![1, 2, 3, 4, 5])]
#[case::term_ledger(DataLedger::Submit, vec![5, 4, 3, 2, 1])]
#[tokio::test]
async fn due_retries_are_queued_in_scan_order(
    #[case] ledger: DataLedger,
    #[case] expected: Vec<u32>,
) {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 8);
    let sm = packed_sm_for_ledger(&config, 8, ledger);
    // No migrated chunks, so the fill promotes retries and does not scan.
    let mut orch = make_orchestrator_with_ledger_total(sm, &config, ledger, 0);
    let now = Instant::now();
    for value in [3_u32, 1, 5, 2, 4] {
        insert_due(&mut orch, PartitionChunkOffset::from(value), now);
    }

    orch.populate_request_queue_at(now);

    let queued: Vec<u32> = orch.ready_offsets.iter().map(|offset| offset.0).collect();
    assert_eq!(queued, expected);
    assert!(
        orch.chunk_requests
            .values()
            .all(|request| !request.local_cache_miss),
        "a new attempt cycle reads the cache again"
    );
}

/// A wave that comes due together takes at most half of the free hot slots.
/// The scan keeps the rest, and the leftover retries go first next tick.
#[tokio::test]
async fn a_due_retry_wave_leaves_half_the_hot_budget_to_the_scan() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 32, 8);
    let sm = packed_sm(&config, 32);
    let mut orch = make_orchestrator_with_ledger_chunks(sm, &config, 32);
    let now = Instant::now();
    for value in 0..10_u32 {
        insert_due(&mut orch, PartitionChunkOffset::from(value), now);
    }

    orch.populate_request_queue_at(now);

    let retried: Vec<u32> = (0..10_u32)
        .filter(|value| {
            matches!(
                orch.chunk_requests[&PartitionChunkOffset::from(*value)].request_state,
                ChunkRequestState::Pending
            )
        })
        .collect();
    assert_eq!(
        retried,
        vec![0, 1, 2, 3],
        "half of 8 free slots, in scan order"
    );
    assert_eq!(
        count_state(&orch, &ChunkRequestState::Pending),
        8,
        "the scan filled the other half"
    );
    assert!(
        (10..32_u32).any(|value| orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(value))),
        "the scan queued entropy past the retry wave"
    );
    assert!(matches!(
        orch.chunk_requests[&PartitionChunkOffset::from(4_u32)].request_state,
        ChunkRequestState::Delayed { .. }
    ));
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

/// Holds fill the discovery budget and are not hot rows. The frontier is
/// Entropy and untracked. It must still be queued.
#[test_log::test(tokio::test)]
async fn term_ledger_write_head_is_queued_when_holds_fill_the_budget() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let budget = 2_u64;
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 8, budget);
    let sm = packed_sm_for_ledger(&config, 8, DataLedger::Submit);
    let mut orch = make_orchestrator_with_ledger_total(sm, &config, DataLedger::Submit, 8);
    for value in 0..u32::try_from(budget).expect("budget") {
        insert_request(
            &mut orch,
            PartitionChunkOffset::from(value),
            ChunkRequestState::HeldByBodyWorker,
            false,
        );
    }
    let frontier = PartitionChunkOffset::from(7_u32);
    assert!(
        !orch.chunk_requests.contains_key(&frontier),
        "the frontier starts untracked"
    );
    assert_eq!(
        orch.storage_module.get_chunk_type(&frontier),
        Some(ChunkType::Entropy)
    );

    orch.populate_request_queue();

    assert!(
        orch.chunk_requests.contains_key(&frontier),
        "a full hold budget must not skip the term write head"
    );
    assert!(matches!(
        orch.chunk_requests[&frontier].request_state,
        ChunkRequestState::Pending
    ));
    assert_eq!(
        count_state(&orch, &ChunkRequestState::HeldByBodyWorker),
        usize::try_from(budget).expect("budget"),
        "the holds stay holds"
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

fn interval_count(sm: &StorageModule) -> usize {
    sm.intervals().read().unwrap().iter().count()
}

/// The interval budget is also the visit budget. A range with more fragments
/// than that budget must not be read to the end, in either direction.
#[test]
fn entropy_scan_reads_only_the_budgeted_intervals() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 32, 1);
    let sm = unpacked_sm_for_ledger(&config, 32, DataLedger::Publish);
    {
        let mut intervals = sm.intervals().write().unwrap();
        for offset in 0..32 {
            let kind = if offset % 2 == 0 {
                ChunkType::Data
            } else {
                ChunkType::Uninitialized
            };
            StorageModule::cut_then_insert_interval_if_touching(
                &mut intervals,
                PartitionChunkOffset::from(offset),
                kind,
            );
        }
    }
    assert!(
        interval_count(&sm) > 2,
        "the fixture must be more fragments than the budget"
    );

    let before = sm.interval_visit_count();
    let scan = entropy_scan_runs(&sm, 0, 31, false, 2);
    let visits = sm.interval_visit_count() - before;
    assert!(scan.interval_budget_spent);
    assert_eq!(visits, 2, "a forward budget of 2 read {visits} intervals");

    let before = sm.interval_visit_count();
    let scan = entropy_scan_runs(&sm, 0, 31, true, 2);
    let visits = sm.interval_visit_count() - before;
    assert!(scan.interval_budget_spent);
    assert_eq!(visits, 2, "a reverse budget of 2 read {visits} intervals");
}

/// Non-entropy fragments must spend the scan budget. A Publish slot whose
/// only Entropy sits past more fragments than `max_pending * 4` must not
/// queue that Entropy on the first tick, and the cursor must move so the
/// next tick continues.
#[test_log::test(tokio::test)]
async fn publish_entropy_scan_bounds_non_entropy_fragments() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 24, 1);
    let sm = unpacked_sm_for_ledger(&config, 24, DataLedger::Publish);
    for offset in (0..23).step_by(2) {
        write_synced_chunk(&sm, offset, 32, ChunkType::Data);
    }
    write_synced_chunk(&sm, 23, 32, ChunkType::Entropy);
    assert!(
        interval_count(&sm) > 4,
        "the fixture must be more fragments than the interval budget"
    );
    let mut orch = make_orchestrator_with_ledger_chunks(Arc::clone(&sm), &config, 24);
    let entropy = PartitionChunkOffset::from(23_u32);

    orch.populate_request_queue();
    assert!(
        !orch.chunk_requests.contains_key(&entropy),
        "the first tick must not walk every non-entropy fragment to the entropy tail"
    );
    assert_eq!(
        orch.scan_cursor, 4,
        "the first tick spends an interval budget of 4"
    );

    let mut ticks = 0_u32;
    while !orch.chunk_requests.contains_key(&entropy) {
        let before = orch.scan_cursor;
        orch.populate_request_queue();
        ticks += 1;
        assert!(
            ticks < 30,
            "the scan did not reach offset 23; cursor stayed near {before}"
        );
        if !orch.chunk_requests.contains_key(&entropy) {
            assert_ne!(
                orch.scan_cursor, before,
                "a tick that finds no entropy must still advance the cursor"
            );
        }
    }
}

/// One Uninitialized span is one visit. Entropy after it, still inside the
/// interval budget, is queued on the same tick.
#[test_log::test(tokio::test)]
async fn publish_entropy_scan_crosses_one_uninitialized_span() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 24, 1);
    let sm = unpacked_sm_for_ledger(&config, 24, DataLedger::Publish);
    write_synced_chunk(&sm, 23, 32, ChunkType::Entropy);
    assert!(
        interval_count(&sm) <= 4,
        "a fresh reset plus one entropy tail must stay inside the interval budget, got {}",
        interval_count(&sm)
    );
    let mut orch = make_orchestrator_with_ledger_chunks(sm, &config, 24);
    orch.populate_request_queue();
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(23_u32)),
        "one uninitialized span must not hide later entropy in the same tick"
    );
}

/// A chunk-budget clip inside one Entropy interval resumes at the next
/// offset. The request budget and the probe budget both outlast the clip, so
/// the scan reads `interval_budget_spent` instead of stopping on a slot.
#[test_log::test(tokio::test)]
async fn publish_entropy_scan_keeps_the_cursor_inside_a_clipped_entropy_run() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 24, 1);
    let sm = packed_sm(&config, 24);
    let entropy = sm.get_intervals(ChunkType::Entropy);
    assert_eq!(entropy.len(), 1, "a packed module is one entropy interval");
    let far_end = entropy[0].end().0;
    assert!(
        far_end > 4,
        "the interval must be wider than the clip, got {far_end}"
    );
    let scan = entropy_scan_runs(&sm, 0, far_end, false, 4);
    assert_eq!(scan.runs, vec![(0, 3)]);
    assert_eq!(scan.visited_edge, Some(far_end));
    assert!(
        !scan.interval_budget_spent,
        "a chunk clip must leave the interval flag clear"
    );

    let mut orch = make_orchestrator_with_ledger_chunks(sm, &config, 24);
    let mut probes = 0_usize;
    let mut requests_to_add = 100_usize;
    let stop = orch.consume_entropy_scan(
        &[],
        0,
        far_end,
        false,
        4,
        far_end,
        Instant::now(),
        &mut probes,
        100,
        &mut requests_to_add,
    );
    assert!(
        !stop,
        "the clip must not stop the tick before the flag is read"
    );
    assert_eq!(
        orch.scan_cursor, 4,
        "the cursor stays on the next offset inside the entropy run"
    );
    for offset in 0..4 {
        assert!(
            orch.chunk_requests
                .contains_key(&PartitionChunkOffset::from(offset)),
            "offset {offset} is inside the clip"
        );
    }
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(4_u32)),
        "the offset past the clip waits"
    );
}

/// Term ledgers walk high to low. Data fragments at the write head must
/// spend the interval budget, so Entropy at offset 0 is not queued on the
/// first tick. The cursor moves down from the top.
#[test_log::test(tokio::test)]
async fn term_ledger_entropy_scan_bounds_high_fragments() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 24, 1);
    let sm = unpacked_sm_for_ledger(&config, 24, DataLedger::Submit);
    write_synced_chunk(&sm, 0, 32, ChunkType::Entropy);
    for offset in [16_u32, 18, 20, 22] {
        write_synced_chunk(&sm, offset, 32, ChunkType::Data);
    }
    assert_ne!(
        sm.get_chunk_type(&PartitionChunkOffset::from(23_u32)),
        Some(ChunkType::Entropy),
        "the write head must not be the special-cased entropy offset"
    );
    assert!(
        interval_count(&sm) > 4,
        "the high end must be more fragments than the interval budget"
    );
    let mut orch =
        make_orchestrator_with_ledger_total(Arc::clone(&sm), &config, DataLedger::Submit, 24);
    let entropy = PartitionChunkOffset::from(0_u32);

    orch.populate_request_queue();
    assert!(
        !orch.chunk_requests.contains_key(&entropy),
        "the first reverse tick must not walk every high fragment down to offset 0"
    );
    assert_eq!(
        orch.scan_cursor, 19,
        "the first reverse tick spends an interval budget of 4"
    );

    let mut ticks = 0_u32;
    while !orch.chunk_requests.contains_key(&entropy) {
        let before = orch.scan_cursor;
        orch.populate_request_queue();
        ticks += 1;
        assert!(
            ticks < 30,
            "the reverse scan did not reach offset 0; cursor stayed near {before}"
        );
        if !orch.chunk_requests.contains_key(&entropy) {
            assert_ne!(
                orch.scan_cursor, before,
                "a tick that finds no entropy must still advance the cursor"
            );
        }
    }
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
            sm.settle_pending_body_migration(key, job.data_commitment, job.block_height)
                .expect("settle")
        );
    }
}

fn cache_bodies_at(db: &DatabaseProvider, tx: &DataTransaction, data: &[u8], offsets: &[usize]) {
    db.update_eyre(|wtx| {
        cache_data_root(
            wtx,
            &tx.header,
            None,
            &irys_types::data_tree::Geometry::TESTING,
        )?;
        for &i in offsets {
            let node = &tx.chunks[i];
            let chunk = UnpackedChunk {
                data_tree_root: tx.header.data_tree_root,
                data_size: tx.header.data_size,
                data_path: Base64(tx.proofs[i].proof.clone()),
                bytes: Base64(data[node.min_byte_range..node.max_byte_range].to_vec()),
                tx_offset: TxChunkOffset::from(u32::try_from(i).expect("tx offset")),
            };
            cache_chunk(wtx, &chunk, &irys_types::data_tree::Geometry::TESTING)?;
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

/// Term ledgers queue offsets high to low. The run must still form, and each
/// offset must get its own placement: a body written at a mirrored offset
/// would fail its proof, or land on the wrong chunk.
#[test_log::test(tokio::test)]
async fn local_cache_descending_run_is_one_lookup() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 3);
    index_and_settle(&sm, &tx, 2);
    cache_bodies_at(&db, &tx, &data, &[1, 2]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    add_assigned_peer(&mut orch, &config, 13, 9113);
    for value in [2_u32, 1, 0] {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    assert!(orch.dispatch_next());
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(orch.cache_lookups_this_tick, 1);
    assert!(fetcher.request_log.read().unwrap().is_empty());
    assert!(sm.is_data_write_pending_at(PartitionChunkOffset::from(2)));
    assert!(sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));
    assert!(!sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
    assert!(orch.chunk_requests[&PartitionChunkOffset::from(0)].local_cache_miss);
    assert_eq!(
        orch.ready_offsets.front(),
        Some(&PartitionChunkOffset::from(0))
    );
}

/// The term-ledger tick itself: the write head goes to the front and the
/// scan queues the rest high to low. One lookup covers all of it.
#[test_log::test(tokio::test)]
async fn term_ledger_tick_writes_the_frontier_run_in_one_lookup() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm_for_ledger(&config, 4, DataLedger::Submit);
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 4);
    index_and_settle(&sm, &tx, 3);
    cache_bodies_at(&db, &tx, &data, &[0, 1, 2, 3]);
    let mut orch =
        make_orchestrator_with_ledger_total_and_db(sm.clone(), &config, DataLedger::Submit, 4, db);
    add_assigned_peer(&mut orch, &config, 14, 9114);

    orch.populate_request_queue_at(Instant::now());
    let queued: Vec<u32> = orch.ready_offsets.iter().map(|offset| offset.0).collect();
    assert_eq!(queued, vec![3, 2, 1, 0]);

    assert!(orch.dispatch_next());
    assert_eq!(orch.cache_lookups_this_tick, 1);
    for value in 0..4_u32 {
        assert!(
            sm.is_data_write_pending_at(PartitionChunkOffset::from(value)),
            "offset {value}"
        );
    }
}

/// A descending run that crosses a submodule boundary. The span joins two
/// index groups before the reversal; each offset must keep its own placement.
#[test_log::test(tokio::test)]
async fn local_cache_descending_run_crosses_a_submodule_boundary() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let info = StorageModuleInfo {
        id: 0,
        partition_assignment: Some(PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            miner_address: IrysAddress::from([1_u8; 20]),
            partition_hash: H256::random(),
        }),
        submodules: vec![
            (partition_chunk_offset_ie!(0, 2), "chunks_low".into()),
            (partition_chunk_offset_ie!(2, 4), "chunks_high".into()),
        ],
    };
    let sm = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
    sm.pack_with_zeros();
    let db = open_test_db(&config);
    let (tx, data) = signed_tx(&config, 4);
    index_and_settle(&sm, &tx, 3);
    cache_bodies_at(&db, &tx, &data, &[1, 2, 3]);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm.clone(), &config, db);
    add_assigned_peer(&mut orch, &config, 15, 9115);
    for value in [3_u32, 2, 1, 0] {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    assert!(orch.dispatch_next());
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(orch.cache_lookups_this_tick, 1);
    assert!(fetcher.request_log.read().unwrap().is_empty());
    for value in 1..4_u32 {
        assert!(
            sm.is_data_write_pending_at(PartitionChunkOffset::from(value)),
            "offset {value}"
        );
    }
    assert!(!sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
    assert!(orch.chunk_requests[&PartitionChunkOffset::from(0)].local_cache_miss);
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
            sm.settle_pending_body_migration(key, job.data_commitment, job.block_height)
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

/// A live body job covers far more entropy than the pending budget. Each tick
/// turns the new rows into holds. The map stays within the budget.
#[test_log::test(tokio::test)]
async fn live_body_range_does_not_grow_chunk_requests() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks: u64 = 200;
    let budget: u64 = 10;
    let ticks: usize = 15;
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), num_chunks, budget);
    let sm = unpacked_sm_for_ledger(&config, num_chunks, DataLedger::Publish);
    let chunk_size = usize::try_from(config.consensus.chunk_size).expect("chunk size");
    let zeros = vec![0_u8; chunk_size];
    for offset in 0..u32::try_from(num_chunks).expect("chunk count") {
        assert!(
            sm.write_chunk(
                PartitionChunkOffset::from(offset),
                zeros.clone(),
                ChunkType::Entropy,
            ),
            "queue entropy at {offset}"
        );
    }
    sm.force_sync_pending_chunks().expect("sync entropy");
    let (tx, _data) = signed_tx(&config, num_chunks);
    index_tx(&sm, &tx, num_chunks - 1);
    let mut orch = make_orchestrator_with_ledger_chunks(Arc::clone(&sm), &config, num_chunks);

    for _ in 0..ticks {
        orch.prepare_tick();
        let mut dispatched = 0_u32;
        while orch.dispatch_next() {
            dispatched += 1;
            assert!(
                dispatched < 8,
                "a fully covered range must not start peer fetches"
            );
        }
    }

    let tracked = orch.chunk_requests.len();
    assert!(
        tracked <= usize::try_from(budget).expect("budget"),
        "chunk_requests grew to {tracked} across {ticks} ticks; budget is {budget}"
    );
    assert!(tracked > 0, "the scan must queue the covered range");
    assert_eq!(
        count_state(&orch, &ChunkRequestState::HeldByBodyWorker),
        tracked,
        "every tracked offset inside the live job is a hold"
    );
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
    // Subtract from the timeout, not from Instant. Clippy denies the latter.
    orch.release_body_worker_holds(Instant::now() + timeout.saturating_sub(Duration::from_secs(1)));
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
            sm.settle_pending_body_migration(key, job.data_commitment, job.block_height)
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

/// A module with peers never scans its entropy intervals: on a fragmented
/// module that scan dominated every 250 ms tick.
#[test_log::test(tokio::test)]
async fn entropy_peer_check_skips_the_scan_when_peers_exist() {
    super::reset_entropy_fingerprint_calls();
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);
    add_assigned_peer(&mut orch, &config, 7, 19107);

    assert!(!orch.note_new_entropy_without_peers());
    assert!(
        !orch.entropy_peer_checked(),
        "an orchestrator with peers must not scan entropy intervals"
    );
    assert_eq!(
        super::entropy_fingerprint_calls(),
        0,
        "an orchestrator with peers must not walk entropy intervals"
    );
}

/// Without peers, the first check sees the packed interval as new and a
/// repeat inside the interval does not scan again.
#[test_log::test(tokio::test)]
async fn entropy_peer_check_without_peers_is_throttled() {
    super::reset_entropy_fingerprint_calls();
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let mut orch = make_orchestrator(sm, &config);

    assert!(
        orch.note_new_entropy_without_peers(),
        "packed entropy is new"
    );
    assert_eq!(
        super::entropy_fingerprint_calls(),
        1,
        "the first check walks the intervals once"
    );
    assert!(
        !orch.note_new_entropy_without_peers(),
        "a second check inside the throttle window does not resync"
    );
    assert_eq!(
        super::entropy_fingerprint_calls(),
        1,
        "a second check inside the throttle window must not walk again"
    );
}

/// Rebuild of every fragmented interval on a tick is the cost that stalled
/// data sync after a crash. The tick may visit only its probe budget.
#[test_log::test(tokio::test)]
async fn fragmented_entropy_tick_does_not_rebuild_every_interval() {
    const FRAGMENTS: u32 = 100_000;
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = u64::from(FRAGMENTS) * 2;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    // Do not pack. `pack_with_zeros` syncs every chunk in the partition.
    let info = StorageModuleInfo {
        id: 0,
        partition_assignment: Some(PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            miner_address: IrysAddress::from([1_u8; 20]),
            partition_hash: H256::random(),
        }),
        submodules: vec![(
            partition_chunk_offset_ie!(0, num_chunks as u32),
            "chunks".into(),
        )],
    };
    let sm = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
    {
        let mut intervals = sm.intervals().write().unwrap();
        let covered: Vec<_> = intervals.iter().map(|(interval, _)| *interval).collect();
        for interval in covered {
            let _ = intervals.cut(interval);
        }
        for index in 0..FRAGMENTS {
            let offset = PartitionChunkOffset::from(index * 2);
            intervals
                .insert_strict(ii(offset, offset), ChunkType::Entropy)
                .expect("disjoint entropy point");
        }
    }
    let mut orch = make_orchestrator_with_ledger_chunks(sm.clone(), &config, num_chunks);

    let _ = sm.get_intervals(ChunkType::Entropy);
    let started = Instant::now();
    let rebuilt = sm.get_intervals(ChunkType::Entropy);
    let rebuild = started.elapsed();
    assert_eq!(rebuilt.len(), usize::try_from(FRAGMENTS).unwrap());

    let started = Instant::now();
    let found_new = orch.note_new_entropy_without_peers();
    let note = started.elapsed();
    assert!(found_new, "the first peerless check must see the fragments");

    let started = Instant::now();
    orch.prepare_tick();
    let tick = started.elapsed();

    eprintln!(
        "scale_intervals fragments={FRAGMENTS} rebuild={rebuild:?} note_new_entropy={note:?} prepare_tick={tick:?} queued={}",
        orch.chunk_requests.len()
    );

    // Timings are printed, not asserted: get_intervals is itself linear now,
    // so a wall-clock ratio against it is noise on a loaded runner. The
    // bounded queue below is the structural check.
    assert!(
        orch.chunk_requests.len() <= 1_000,
        "a tick enqueues at most the pending budget, not every fragment"
    );
}

/// Missing-index blocks are retained so a later re-arm can retry them. The
/// retained set has to stay a small multiple of the pending budget: the
/// interval map is the source of the offsets that did not fit.
#[test_log::test(tokio::test)]
async fn blocked_offsets_stay_within_four_pending_budgets() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 64, 2);
    let sm = packed_sm(&config, 64);
    let mut orch = make_orchestrator_with_ledger_chunks(sm, &config, 64);
    let peer = IrysAddress::from([4_u8; 20]);

    for offset in 0..32_u32 {
        insert_requested(&mut orch, PartitionChunkOffset::from(offset), peer);
        orch.mark_chunk_blocked(
            PartitionChunkOffset::from(offset),
            ChunkBlockReason::MissingDataRootIndex,
        )
        .expect("block");
    }

    let blocked = orch
        .chunk_requests
        .values()
        .filter(|request| {
            matches!(
                request.request_state,
                ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
            )
        })
        .count();
    assert_eq!(blocked, 8, "2 pending slots * 4 blocked budgets");
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(0_u32)),
        "re-arm walks the lowest blocked offsets, so those stay"
    );
    assert!(
        !orch
            .chunk_requests
            .contains_key(&PartitionChunkOffset::from(31_u32)),
        "offsets past the blocked budget are rediscovered from the interval map"
    );
}

/// The term-ledger write head is re-queued every tick while it is Entropy.
/// Dropping it from a full blocked set would fetch it again on the next tick.
#[test_log::test(tokio::test)]
async fn blocked_cap_keeps_the_term_ledger_write_head() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config_with_pending_limit(tmp.path().to_path_buf(), 16, 2);
    let sm = packed_sm_for_ledger(&config, 16, DataLedger::Submit);
    let mut orch = make_orchestrator_with_ledger_total(sm, &config, DataLedger::Submit, 16);
    let peer = IrysAddress::from([5_u8; 20]);
    let frontier = PartitionChunkOffset::from(15_u32);

    for offset in 0..16_u32 {
        insert_requested(&mut orch, PartitionChunkOffset::from(offset), peer);
        orch.mark_chunk_blocked(
            PartitionChunkOffset::from(offset),
            ChunkBlockReason::MissingDataRootIndex,
        )
        .expect("block");
    }

    let blocked = orch
        .chunk_requests
        .values()
        .filter(|request| matches!(request.request_state, ChunkRequestState::Blocked(_)))
        .count();
    assert_eq!(blocked, 8);
    assert!(orch.chunk_requests.contains_key(&frontier));
    assert!(
        orch.chunk_requests
            .contains_key(&PartitionChunkOffset::from(0_u32))
    );
}

/// An assigned module still queues entropy. After the assignment is cleared,
/// the same populate returns without adding requests. The clear can land
/// after `prepare_tick`'s guard and before this read.
#[test_log::test(tokio::test)]
async fn assigned_populate_queues_and_cleared_assignment_returns() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 4;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm_for_ledger(&config, num_chunks, DataLedger::Publish);
    let mut orch =
        make_orchestrator_with_ledger_total(sm.clone(), &config, DataLedger::Publish, num_chunks);

    orch.populate_request_queue_at(Instant::now());
    assert!(
        !orch.chunk_requests.is_empty(),
        "an assigned ledger with entropy still queues chunk requests"
    );
    let queued = orch.chunk_requests.len();

    *sm.partition_assignment.write().unwrap() = None;
    orch.populate_request_queue_at(Instant::now());
    assert_eq!(
        orch.chunk_requests.len(),
        queued,
        "a cleared assignment adds no requests"
    );
}

/// A peer can still look assigned to the captured slot after the local module
/// loses its ledger range. The offset goes back to the ready queue. No fetch
/// is accounted.
#[test_log::test(tokio::test)]
async fn cleared_assignment_requeues_a_ledger_fetch() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm.clone(), &config);

    let live_peer = add_assigned_peer(&mut orch, &config, 41, 9041);
    let live_offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, live_offset);
    orch.ready_offsets.clear();
    assert_eq!(
        orch.dispatch_chunk_request(live_offset, live_peer),
        DispatchStart::Started,
        "a live assignment still starts a ledger fetch"
    );
    assert!(matches!(
        orch.chunk_requests[&live_offset].request_state,
        ChunkRequestState::Requested(..)
    ));

    let stale = PartitionAssignment {
        ledger_id: Some(orch.ledger_id()),
        slot_index: None,
        miner_address: IrysAddress::from([42_u8; 20]),
        partition_hash: H256::repeat_byte(9),
    };
    let peer = add_peer_with_assignment(&mut orch, &config, 42, 9042, stale);
    *sm.partition_assignment.write().unwrap() = None;
    let offset = PartitionChunkOffset::from(1);
    insert_pending(&mut orch, offset);
    orch.ready_offsets.clear();

    assert_eq!(
        orch.dispatch_chunk_request(offset, peer),
        DispatchStart::Stop,
        "the offset is not fetched when the local ledger range is gone"
    );
    assert!(
        matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::Pending
        ),
        "the request stays pending for a later assignment"
    );
    assert_eq!(orch.chunk_requests[&offset].attempt_count, 0);
    assert_eq!(orch.ready_offsets.front(), Some(&offset));
    let peers = orch.active_sync_peers.read().unwrap();
    assert_eq!(
        peers.get(&peer).unwrap().active_requests(),
        0,
        "a local addressing failure is not a peer request"
    );
}

/// `dispatch_next` pops the offset it just put back. Stopping the round is
/// required, or this one offset is fetched forever.
#[test_log::test(tokio::test)]
async fn dispatch_does_not_spin_when_ledger_offsets_are_gone() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm.clone(), &config);
    let stale = PartitionAssignment {
        ledger_id: Some(orch.ledger_id()),
        slot_index: None,
        miner_address: IrysAddress::from([43_u8; 20]),
        partition_hash: H256::repeat_byte(10),
    };
    let _peer = add_peer_with_assignment(&mut orch, &config, 43, 9043, stale);
    *sm.partition_assignment.write().unwrap() = None;
    let offset = PartitionChunkOffset::from(0);
    insert_pending(&mut orch, offset);

    let progressed = orch.dispatch_next();
    assert!(!progressed, "a missing ledger range must stop this round");
    assert!(
        matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::Pending
        ),
        "the offset stays pending"
    );
    assert!(
        orch.ready_offsets.contains(&offset),
        "the offset is back on the ready queue"
    );
}

/// The fetcher is created for one ledger, then the assignment is read again.
/// A match builds. A clear, or a different live ledger, aborts the build.
#[test_log::test(tokio::test)]
async fn cleared_assignment_does_not_panic_orchestrator_construction() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let publish = u32::from(DataLedger::Publish);

    let orch = try_make_orchestrator(sm.clone(), &config, publish)
        .expect("an assigned module builds for its ledger");
    assert_eq!(orch.ledger_id(), publish);
    drop(orch);

    *sm.partition_assignment.write().unwrap() = None;
    assert!(
        try_make_orchestrator(Arc::clone(&sm), &config, publish).is_none(),
        "a cleared assignment aborts the build"
    );

    let submit = u32::from(DataLedger::Submit);
    *sm.partition_assignment.write().unwrap() = Some(PartitionAssignment {
        ledger_id: Some(submit),
        slot_index: Some(0),
        miner_address: IrysAddress::from([1_u8; 20]),
        partition_hash: H256::repeat_byte(3),
    });
    assert!(
        try_make_orchestrator(sm, &config, publish).is_none(),
        "a live ledger that differs from the fetcher ledger aborts the build"
    );
}

fn peer_accounting(orch: &ChunkOrchestrator, peer: IrysAddress) -> (usize, u32, u32, u64) {
    let managers = orch.active_sync_peers.read().unwrap();
    let manager = managers.get(&peer).expect("peer manager");
    (
        manager.active_requests(),
        manager.available_concurrency(),
        manager.consecutive_failures(),
        manager.total_failures(),
    )
}

fn start_request(orch: &ChunkOrchestrator, peer: IrysAddress) {
    orch.active_sync_peers
        .write()
        .unwrap()
        .get_mut(&peer)
        .unwrap()
        .on_chunk_request_started();
}

/// Releasing `Requested` slots makes the peer selectable again. Rows that do
/// not hold a slot stay put. A slot count of zero does not underflow. A
/// delivered chunk still clears the failure streak and is not released again.
#[test_log::test(tokio::test)]
async fn release_in_flight_slots_restores_peer_selection() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let sm = packed_sm(&config, 64);
    let mut orch = make_orchestrator(sm, &config);
    let peer = add_assigned_peer(&mut orch, &config, 0xA8, 19080);
    let unknown = IrysAddress::from([0xA9; 20]);

    let (_, allowance, _, _) = peer_accounting(&orch, peer);
    assert!(allowance > 1, "baseline allowance, got {allowance}");
    for offset in 0..allowance {
        insert_requested(&mut orch, PartitionChunkOffset::from(offset), peer);
        start_request(&orch, peer);
    }
    assert_eq!(peer_accounting(&orch, peer).1, 0, "the peer is saturated");
    assert!(
        orch.find_best_peer(None).is_none(),
        "a saturated peer is not selected"
    );

    insert_pending(&mut orch, PartitionChunkOffset::from(1_000));
    orch.chunk_requests.insert(
        PartitionChunkOffset::from(1_001),
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss: false,
            body_in_ingress: false,
            request_state: ChunkRequestState::Completed,
            attempt_count: 1,
            retry_round: 0,
            ready_since: Instant::now(),
        },
    );
    orch.chunk_requests.insert(
        PartitionChunkOffset::from(1_002),
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss: false,
            body_in_ingress: false,
            request_state: ChunkRequestState::AwaitingDurability(Instant::now()),
            attempt_count: 1,
            retry_round: 0,
            ready_since: Instant::now(),
        },
    );
    orch.chunk_requests.insert(
        PartitionChunkOffset::from(1_003),
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss: false,
            body_in_ingress: false,
            request_state: ChunkRequestState::Delayed {
                retry_at: Instant::now(),
            },
            attempt_count: 1,
            retry_round: 0,
            ready_since: Instant::now(),
        },
    );
    orch.chunk_requests.insert(
        PartitionChunkOffset::from(1_004),
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss: false,
            body_in_ingress: false,
            request_state: ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex),
            attempt_count: 1,
            retry_round: 0,
            ready_since: Instant::now(),
        },
    );
    orch.chunk_requests.insert(
        PartitionChunkOffset::from(1_005),
        ChunkRequest {
            excluded: HashSet::new(),
            local_cache_miss: false,
            body_in_ingress: false,
            request_state: ChunkRequestState::HeldByBodyWorker,
            attempt_count: 1,
            retry_round: 0,
            ready_since: Instant::now(),
        },
    );
    // No manager for this address. The release must skip it.
    insert_requested(&mut orch, PartitionChunkOffset::from(1_006), unknown);
    // One extra start has no row. It belongs to another module.
    start_request(&orch, peer);

    orch.release_in_flight_peer_slots();
    assert_eq!(
        peer_accounting(&orch, peer),
        (1, allowance - 1, 0, 0),
        "only Requested rows release, and the sibling slot stays"
    );
    assert_eq!(
        orch.find_best_peer(None),
        Some(peer),
        "the peer is selectable after the slots are released"
    );
    assert!(
        matches!(
            orch.chunk_requests[&PartitionChunkOffset::from(0)].request_state,
            ChunkRequestState::Requested(_, _)
        ),
        "the release does not rewrite request state"
    );

    // A Requested row with no matching start must not underflow.
    let fresh_tmp = TempDirBuilder::new().build();
    let fresh_config = test_config(fresh_tmp.path().to_path_buf(), 64);
    let mut fresh = make_orchestrator(packed_sm(&fresh_config, 64), &fresh_config);
    let fresh_peer = add_assigned_peer(&mut fresh, &fresh_config, 0xAA, 19081);
    insert_requested(&mut fresh, PartitionChunkOffset::from(0), fresh_peer);
    fresh.release_in_flight_peer_slots();
    assert_eq!(
        peer_accounting(&fresh, fresh_peer).0,
        0,
        "releasing with no active request stays at zero"
    );
    assert_eq!(fresh.find_best_peer(None), Some(fresh_peer));

    // A live failure penalizes. A later delivery releases once; only its
    // acceptance clears the streak. The drop then ignores the row that left
    // Requested.
    let lived_tmp = TempDirBuilder::new().build();
    let lived_config = test_config(lived_tmp.path().to_path_buf(), 64);
    let mut lived = make_orchestrator(packed_sm(&lived_config, 64), &lived_config);
    let lived_peer = add_assigned_peer(&mut lived, &lived_config, 0xAB, 19082);
    insert_requested(&mut lived, PartitionChunkOffset::from(0), lived_peer);
    start_request(&lived, lived_peer);
    lived
        .on_chunk_failed(
            PartitionChunkOffset::from(0),
            lived_peer,
            ChunkFetchFailureKind::Timeout,
        )
        .expect("live failure");
    assert_eq!(peer_accounting(&lived, lived_peer).2, 1);
    insert_requested(&mut lived, PartitionChunkOffset::from(1), lived_peer);
    start_request(&lived, lived_peer);
    lived
        .on_chunk_fetched(PartitionChunkOffset::from(1), lived_peer)
        .expect("live delivery");
    assert_eq!(
        peer_accounting(&lived, lived_peer).0,
        0,
        "a delivery releases its slot once"
    );
    assert_eq!(
        peer_accounting(&lived, lived_peer).2,
        1,
        "an unchecked delivery does not clear the failure streak"
    );
    lived.credit_accepted_delivery(lived_peer);
    assert_eq!(
        peer_accounting(&lived, lived_peer).2,
        0,
        "an accepted delivery clears the failure streak"
    );
    lived
        .mark_chunk_awaiting_durability(PartitionChunkOffset::from(1))
        .expect("leave Requested");
    let after_delivery = peer_accounting(&lived, lived_peer);
    lived.release_in_flight_peer_slots();
    assert_eq!(
        peer_accounting(&lived, lived_peer),
        after_delivery,
        "a row that already left Requested is not released again"
    );
}

fn peer_active(orch: &ChunkOrchestrator, peer: IrysAddress) -> usize {
    orch.active_sync_peers.read().unwrap()[&peer].active_requests()
}

fn start_slot(orch: &ChunkOrchestrator, peer: IrysAddress) {
    orch.active_sync_peers
        .write()
        .unwrap()
        .get_mut(&peer)
        .unwrap()
        .on_chunk_request_started();
}

/// A delivered body frees its slot once. It stays `Requested` with the body
/// in ingress, so it is not dispatched again, and a second delivery for it
/// releases nothing.
#[test_log::test(tokio::test)]
async fn delivered_body_releases_its_slot_once_and_is_not_dispatched_again() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let num_chunks = 64;
    let config = test_config(tmp.path().to_path_buf(), num_chunks);
    let sm = packed_sm(&config, num_chunks);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm, &config, open_test_db(&config));
    let peer = add_assigned_peer(&mut orch, &config, 61, 9161);
    for value in 0..u32::try_from(num_chunks).expect("chunks") {
        insert_pending(&mut orch, PartitionChunkOffset::from(value));
    }

    let mut started = 0;
    while orch.dispatch_next() {
        started += 1;
    }
    assert_eq!(started, 20, "a fresh peer takes 20 fetches");
    assert_eq!(peer_active(&orch, peer), 20);
    wait_for_ledger_requests(&fetcher, 20).await;

    let delivered = PartitionChunkOffset::from(0);
    assert!(matches!(
        orch.chunk_requests[&delivered].request_state,
        ChunkRequestState::Requested(..)
    ));
    let attempt = orch
        .on_chunk_fetched(delivered, peer)
        .expect("delivery")
        .start_time;
    assert_eq!(peer_active(&orch, peer), 19);
    assert!(orch.body_awaiting_ingress(delivered, peer, attempt));
    assert!(
        !orch.body_awaiting_ingress(delivered, peer, Instant::now()),
        "a result for another attempt does not match"
    );
    assert!(
        orch.on_chunk_fetched(delivered, peer).is_err(),
        "a second delivery for the same body is rejected"
    );
    assert_eq!(peer_active(&orch, peer), 19, "and releases nothing");

    assert!(orch.dispatch_next(), "the released slot is used at once");
    assert_eq!(peer_active(&orch, peer), 20);
    assert!(orch.body_awaiting_ingress(delivered, peer, attempt));
    wait_for_ledger_requests(&fetcher, 21).await;
    let delivered_ledger = LedgerChunkOffset::from(0);
    let fetches = fetcher
        .request_log
        .read()
        .unwrap()
        .iter()
        .filter(|(offset, _)| *offset == delivered_ledger)
        .count();
    assert_eq!(fetches, 1, "the delivered offset is not fetched again");

    // The success path moves the offset on without touching the slot.
    orch.note_body_buffered(delivered).expect("buffered");
    assert!(matches!(
        orch.chunk_requests[&delivered].request_state,
        ChunkRequestState::AwaitingDurability(_)
    ));
    assert_eq!(peer_active(&orch, peer), 20);
}

/// A rejected body counts one peer failure and excludes the peer for that
/// offset. A local requeue blames nobody. Neither releases a slot.
#[test_log::test(tokio::test)]
async fn ingress_outcomes_do_not_release_the_slot_again() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm, &config);
    let peer_a = add_assigned_peer(&mut orch, &config, 62, 9162);
    let peer_b = add_assigned_peer(&mut orch, &config, 63, 9163);
    let rejected = PartitionChunkOffset::from(0);
    let requeued = PartitionChunkOffset::from(1);
    let sibling = PartitionChunkOffset::from(2);
    for offset in [rejected, requeued, sibling] {
        insert_requested(&mut orch, offset, peer_a);
        start_slot(&orch, peer_a);
    }
    orch.on_chunk_fetched(rejected, peer_a).expect("delivery");
    orch.on_chunk_fetched(requeued, peer_a).expect("delivery");
    assert_eq!(peer_active(&orch, peer_a), 1);

    assert!(
        orch.on_chunk_failed(rejected, peer_a, ChunkFetchFailureKind::Timeout)
            .is_err(),
        "a fetch failure cannot arrive for a delivered body"
    );
    orch.on_ingressed_chunk_rejected(rejected, peer_a)
        .expect("rejected");
    let request = &orch.chunk_requests[&rejected];
    assert!(matches!(request.request_state, ChunkRequestState::Pending));
    assert!(request.excluded.contains(&peer_a));
    assert_eq!(orch.find_best_peer(Some(&request.excluded)), Some(peer_b));

    orch.requeue_after_local_write_failure(requeued)
        .expect("requeued");
    let request = &orch.chunk_requests[&requeued];
    assert!(matches!(request.request_state, ChunkRequestState::Pending));
    assert!(!request.excluded.contains(&peer_a));

    let peers = orch.active_sync_peers.read().unwrap();
    assert_eq!(
        peers[&peer_a].active_requests(),
        1,
        "only the sibling holds a slot"
    );
    assert_eq!(peers[&peer_a].consecutive_failures(), 1);
    assert_eq!(peers[&peer_a].total_failures(), 1);
}

/// A peer disconnect and an orchestrator drop leave a body in ingress alone.
/// Its slot is already free and its ingress result still decides the offset.
#[test_log::test(tokio::test)]
async fn body_in_ingress_survives_peer_removal_and_slot_sweep() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 4);
    let sm = packed_sm(&config, 4);
    let mut orch = make_orchestrator(sm, &config);
    let peer = add_assigned_peer(&mut orch, &config, 64, 9164);
    let delivered = PartitionChunkOffset::from(0);
    let in_flight = PartitionChunkOffset::from(1);
    for offset in [delivered, in_flight] {
        insert_requested(&mut orch, offset, peer);
        start_slot(&orch, peer);
    }
    let attempt = orch
        .on_chunk_fetched(delivered, peer)
        .expect("delivery")
        .start_time;
    // Another storage module holds one more slot on this peer.
    start_slot(&orch, peer);
    assert_eq!(peer_active(&orch, peer), 2);

    orch.release_in_flight_peer_slots();
    assert_eq!(
        peer_active(&orch, peer),
        1,
        "only the fetch still on the network is released"
    );

    orch.remove_peer(peer);
    assert!(orch.body_awaiting_ingress(delivered, peer, attempt));
    assert!(matches!(
        orch.chunk_requests[&in_flight].request_state,
        ChunkRequestState::Pending
    ));
}

// === Assignee interval hints ===

fn submit_orchestrator_with_hint(
    config: &Config,
    covers: (u32, u32),
    data: Vec<(u32, u32)>,
) -> (ChunkOrchestrator, IrysAddress) {
    let sm = packed_sm_for_ledger(config, 64, DataLedger::Submit);
    let mut orch = make_orchestrator_with_ledger_total(sm, config, DataLedger::Submit, 64);
    let assignee = add_assigned_peer(&mut orch, config, 21, 9121);
    orch.assignee_intervals = AssigneeIntervals::new(4);
    orch.assignee_intervals
        .insert_known(assignee, covers, data, Instant::now());
    (orch, assignee)
}

fn tracked_in(orch: &ChunkOrchestrator, range: std::ops::RangeInclusive<u32>) -> Vec<u32> {
    let mut tracked: Vec<u32> = range
        .filter(|value| {
            orch.chunk_requests
                .contains_key(&PartitionChunkOffset::from(*value))
        })
        .collect();
    tracked.sort_unstable();
    tracked
}

/// The assignee answered for 0..=59 and holds only 0..=7. The scan queues the
/// unanswered frontier band and the claimed run, and jumps over 8..=59.
#[test_log::test(tokio::test)]
async fn the_scan_skips_entropy_no_assignee_holds() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let (mut orch, _) = submit_orchestrator_with_hint(&config, (0, 59), vec![(0, 7)]);
    let before = metrics::test_hint_skipped_count();

    orch.populate_request_queue_at(Instant::now());

    assert_eq!(tracked_in(&orch, 60..=63), vec![60, 61, 62, 63]);
    assert_eq!(tracked_in(&orch, 0..=7), (0..=7).collect::<Vec<_>>());
    assert!(
        tracked_in(&orch, 8..=59).is_empty(),
        "no assignee holds 8..=59 and no signer is in the peer set"
    );
    assert_eq!(orch.scan_cursor, u32::MAX, "the scan reached the bottom");
    assert!(metrics::test_hint_skipped_count() >= before + 52);
}

/// Without an answer from every assignee nothing is skipped.
#[test_log::test(tokio::test)]
async fn one_silent_assignee_means_every_offset_is_probed() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let (mut orch, _) = submit_orchestrator_with_hint(&config, (0, 59), vec![(0, 7)]);
    add_assigned_peer(&mut orch, &config, 22, 9122);

    orch.populate_request_queue_at(Instant::now());

    assert_eq!(tracked_in(&orch, 0..=63).len(), 64);
}

/// Dispatch picks another peer over an assignee that answered without the
/// offset, and never asks that assignee once it is the only one left.
#[test_log::test(tokio::test)]
async fn dispatch_does_not_ask_an_assignee_that_answered_without_the_offset() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let sm = packed_sm(&config, 64);
    let db = open_test_db(&config);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm, &config, db);
    let absent = add_assigned_peer(&mut orch, &config, 23, 9123);
    orch.assignee_intervals = AssigneeIntervals::new(0);
    orch.assignee_intervals
        .insert_known(absent, (0, 63), vec![(40, 63)], Instant::now());
    let other = add_assigned_peer(&mut orch, &config, 24, 9124);
    let offset = PartitionChunkOffset::from(5);
    insert_pending(&mut orch, offset);

    assert!(orch.dispatch_next());
    wait_for_ledger_requests(&fetcher, 1).await;
    let asked: Vec<u16> = fetcher
        .request_log
        .read()
        .unwrap()
        .iter()
        .map(|(_, api)| api.port())
        .collect();
    assert_eq!(asked, vec![9124]);
    assert!(matches!(
        orch.chunk_requests[&offset].request_state,
        ChunkRequestState::Requested(peer, _) if peer == other
    ));

    orch.remove_peer(other);
    orch.chunk_requests.get_mut(&offset).unwrap().request_state = ChunkRequestState::Pending;
    orch.chunk_requests
        .get_mut(&offset)
        .unwrap()
        .excluded
        .clear();
    orch.ready_offsets.push_back(offset);
    assert!(!orch.dispatch_next());
    assert!(
        matches!(
            orch.chunk_requests[&offset].request_state,
            ChunkRequestState::Delayed { .. }
        ),
        "the only peer left answered without it: delay, do not ask"
    );
    assert_eq!(fetcher.request_log.read().unwrap().len(), 1);
}

/// With an ingress-proof signer in the peer set, the scan skips nothing: the
/// signer may hold what no assignee does. Dispatch still keeps those offsets
/// away from the assignee that answered without them.
#[test_log::test(tokio::test)]
async fn with_a_signer_unclaimed_offsets_are_queued_for_it() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let (mut orch, assignee) = submit_orchestrator_with_hint(&config, (0, 59), vec![(0, 7)]);
    let other_slot = PartitionAssignment {
        ledger_id: Some(DataLedger::Submit.into()),
        slot_index: Some(9),
        miner_address: IrysAddress::from([25_u8; 20]),
        partition_hash: H256::random(),
    };
    add_peer_with_assignment(&mut orch, &config, 25, 9125, other_slot);

    orch.populate_request_queue_at(Instant::now());

    assert_eq!(tracked_in(&orch, 8..=59), (8..=59).collect::<Vec<_>>());
    assert!(orch.assignee_intervals.excludes(&assignee, 30));
}

fn fill_peer(orch: &ChunkOrchestrator, peer: IrysAddress) {
    while peer_accounting(orch, peer).1 > 0 {
        start_request(orch, peer);
    }
}

/// With no free slot on any peer, the offset at the head keeps its place:
/// dispatch stops without reordering the queue.
#[test_log::test(tokio::test)]
async fn with_no_free_slot_the_head_offset_keeps_its_place() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 8);
    let sm = packed_sm(&config, 8);
    let mut orch = make_orchestrator(sm, &config);
    let peer = add_assigned_peer(&mut orch, &config, 71, 9171);
    fill_peer(&orch, peer);
    for value in [2_u32, 3, 4] {
        insert_request(
            &mut orch,
            PartitionChunkOffset::from(value),
            ChunkRequestState::Pending,
            true,
        );
    }

    assert!(!orch.dispatch_next());
    let queued: Vec<u32> = orch.ready_offsets.iter().map(|offset| offset.0).collect();
    assert_eq!(queued, vec![2, 3, 4]);
}

/// An offset only a busy signer may serve does not hold up the next offset an
/// idle assignee can serve. It waits at the tail.
#[test_log::test(tokio::test)]
async fn an_offset_only_a_busy_signer_can_serve_does_not_block_the_queue() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 8);
    let sm = packed_sm(&config, 8);
    let db = open_test_db(&config);
    let (mut orch, fetcher) = make_orchestrator_with_db(sm, &config, db);
    let assignee = add_assigned_peer(&mut orch, &config, 72, 9172);
    let other_slot = PartitionAssignment {
        ledger_id: Some(DataLedger::Publish.into()),
        slot_index: Some(9),
        miner_address: IrysAddress::from([73_u8; 20]),
        partition_hash: H256::random(),
    };
    let signer = add_peer_with_assignment(&mut orch, &config, 73, 9173, other_slot);
    fill_peer(&orch, signer);
    orch.assignee_intervals = AssigneeIntervals::new(0);
    orch.assignee_intervals
        .insert_known(assignee, (0, 7), vec![(3, 3)], Instant::now());
    let (only_signer, claimed) = (PartitionChunkOffset::from(2), PartitionChunkOffset::from(3));
    insert_request(&mut orch, only_signer, ChunkRequestState::Pending, true);
    insert_request(&mut orch, claimed, ChunkRequestState::Pending, true);

    assert!(
        orch.dispatch_next(),
        "the claimed offset goes to the assignee"
    );
    wait_for_ledger_requests(&fetcher, 1).await;
    assert!(matches!(
        orch.chunk_requests[&claimed].request_state,
        ChunkRequestState::Requested(peer, _) if peer == assignee
    ));
    assert_eq!(
        orch.ready_offsets.iter().copied().collect::<Vec<_>>(),
        vec![only_signer]
    );
}

/// Hints answer for one slot. A module moved to another slot starts again.
#[test_log::test(tokio::test)]
async fn a_slot_change_drops_the_old_slots_hints() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let (mut orch, assignee) = submit_orchestrator_with_hint(&config, (0, 59), vec![(0, 7)]);
    assert!(orch.assignee_intervals.excludes(&assignee, 30));
    orch.hint_slot = Some(9);

    orch.populate_request_queue_at(Instant::now());

    assert_eq!(orch.hint_slot, Some(0));
    assert!(
        !orch.assignee_intervals.excludes(&assignee, 30),
        "the old slot's answer no longer applies"
    );
}

/// A slot change resets hints before the scan even while hints are idle, so
/// the new slot's Entropy is not skipped on the old slot's answers.
#[test_log::test(tokio::test)]
async fn a_slot_change_resets_hints_even_while_idle() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 64);
    let (mut orch, assignee) = submit_orchestrator_with_hint(&config, (0, 59), vec![(0, 7)]);
    orch.hint_slot = Some(9);
    orch.hints_idle = true;

    orch.populate_request_queue_at(Instant::now());

    assert!(!orch.assignee_intervals.excludes(&assignee, 30));
    assert_eq!(
        tracked_in(&orch, 8..=59),
        (8..=59).collect::<Vec<_>>(),
        "nothing was skipped on the old slot's answers"
    );
}

/// The frontier margin applies only while the ledger's head is inside this
/// partition. A partition the ledger has filled is hinted to its last offset.
#[test_log::test(tokio::test)]
async fn a_full_partition_has_no_frontier_margin() {
    for (total, full) in [(32_u64, false), (200, true)] {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = test_config(tmp.path().to_path_buf(), 64);
        let sm = packed_sm_for_ledger(&config, 64, DataLedger::Submit);
        let orch = make_orchestrator_with_ledger_total(sm, &config, DataLedger::Submit, total);
        let (max_offset, ledger_head) = orch.get_max_chunk_offset().expect("max offset");
        let expected = if full {
            max_offset.0 + HINT_FRONTIER_MARGIN
        } else {
            max_offset.0
        };
        assert_eq!(
            orch.hint_head(max_offset.0, ledger_head),
            expected,
            "total {total}"
        );
    }
}

/// A module with no Entropy left stops fetching hints. New Entropy starts it
/// again.
#[test_log::test(tokio::test)]
async fn hints_go_idle_once_the_partition_has_no_entropy() {
    let tmp = TempDirBuilder::new().with_tracing().build();
    let config = test_config(tmp.path().to_path_buf(), 8);
    let chunk_size = usize::try_from(config.consensus.chunk_size).expect("chunk size");
    let sm = packed_sm_for_ledger(&config, 8, DataLedger::Submit);
    for offset in 0..8_u32 {
        write_synced_chunk(&sm, offset, chunk_size, ChunkType::Data);
    }
    let mut orch = make_orchestrator_with_ledger_total(sm.clone(), &config, DataLedger::Submit, 8);
    add_assigned_peer(&mut orch, &config, 74, 9174);

    orch.populate_request_queue_at(Instant::now());
    assert!(orch.hints_idle, "a full pass found no Entropy");

    write_synced_chunk(&sm, 5, chunk_size, ChunkType::Entropy);
    orch.populate_request_queue_at(Instant::now());
    assert!(!orch.hints_idle, "new Entropy resumes the hints");
}
