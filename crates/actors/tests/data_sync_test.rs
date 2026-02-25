use irys_actors::{
    chunk_fetcher::{ChunkFetchError, ChunkFetcher, ChunkFetcherFactory, MockChunkFetcher},
    peer_bandwidth_manager::PeerBandwidthManager,
    services::{ServiceReceivers, ServiceSenders},
    DataSyncServiceInner, DataSyncServiceMessage,
};
use irys_domain::{
    BlockTree, BlockTreeReadGuard, ChunkType, PeerList, StorageModule, StorageModuleInfo,
};
use irys_packing::{capacity_single::compute_entropy_chunk, packing_xor_vec_u8};
use irys_testing_utils::setup_tracing_and_temp_dir;
use irys_types::{
    irys::IrysSigner, ledger_chunk_offset_ie, partition::PartitionAssignment,
    partition_chunk_offset_ie, Base64, Config, ConsensusConfig, DataLedger, DataSyncServiceConfig,
    DataTransaction, IrysAddress, IrysBlockHeader, IrysPeerId, LedgerChunkOffset, LedgerChunkRange,
    NodeConfig, PackedChunk, PartitionChunkOffset, PeerAddress, PeerListItem, PeerScore,
    ProtocolVersion, StorageSyncConfig, TxChunkOffset, UnpackedChunk, H256,
};
use nodit::Interval;
use rust_decimal::prelude::ToPrimitive as _;
use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, RwLock},
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{debug, error};

#[ignore = "flaky, non critical function test"]
#[tokio::test]
async fn slow_heavy_test_data_sync_with_different_peer_performance() {
    std::env::set_var("RUST_LOG", "debug,storage=off");
    let tmp_dir = setup_tracing_and_temp_dir(None, false);

    let setup = TestSetup::new(100, Duration::from_secs(5), &tmp_dir);
    let storage_modules = Arc::new(RwLock::new(vec![setup.storage_module.clone()]));
    debug!("Creating chunk_fetcher_factory");
    let chunk_fetcher_factory = setup.create_chunk_fetcher_factory();

    // Extract the values we need before moving setup
    let peer_addresses = (
        setup.slow_peer_addr,
        setup.stable_peer_addr,
        setup.fast_peer_addr,
    );
    let mock_fetchers = setup.mock_fetchers.clone();
    let storage_module_ref = setup.storage_module.clone();

    debug!("Setting up test harness");
    // Create the test harness instead of spawning the full service
    let mut harness = DataSyncServiceTestHarness::new(
        setup.block_tree,
        storage_modules,
        setup.peer_list,
        chunk_fetcher_factory,
        setup.service_senders.clone(),
        setup.service_receivers.data_sync, // Pass the receiver directly
        setup.config.clone(),
    );

    debug!("Starting sync process...");
    harness.start_sync().expect("Failed to start sync");

    debug!("Running controlled sync with message processing...");

    // Run 75 ticks with snapshots at tick 30 and final snapshot at the end
    harness
        .run_ticks_with_messages(
            75,
            Duration::from_millis(250),
            &peer_addresses,
            &mock_fetchers,
            &storage_module_ref,
            vec![34], // Take snapshot at tick 34
        )
        .await
        .expect("Failed during sync execution");

    // Give any active requests some seconds to resolve
    debug!("waiting for additional requests to complete...");
    harness
        .process_messages_for_duration(Duration::from_secs(20))
        .await
        .expect("should process messages for duration");
    debug!("done waiting.");

    // Process any final pending messages
    harness
        .process_pending_messages()
        .expect("Failed to process final messages");

    // Take final performance snapshot
    println!("\nGetting final performance results...");
    harness
        .take_performance_snapshot(
            "Final Results",
            &peer_addresses,
            &mock_fetchers,
            &storage_module_ref,
        )
        .expect("Failed to take final snapshot");

    let _data_intervals = storage_module_ref.get_intervals(ChunkType::Data);
    let entropy_intervals = storage_module_ref.get_intervals(ChunkType::Entropy);
    debug!("{:#?}", entropy_intervals);

    // Storage module should be fully synced (no entropy)
    // TODO: Fix this.. there will always be one left when the max_chunk_offset of the block and the partition align
    assert!(entropy_intervals.is_empty());
}

fn format_intervals(intervals: &[Interval<PartitionChunkOffset>]) -> String {
    let total_count: u64 = intervals
        .iter()
        .map(|interval| {
            let start = interval.start().to_u64().unwrap();
            let end = interval.end().to_u64().unwrap();
            end - start + 1 // +1 because intervals are inclusive
        })
        .sum();

    let ranges_str = intervals
        .iter()
        .map(|interval| {
            let start = interval.start();
            let end = interval.end();
            if start == end {
                start.to_string()
            } else {
                format!("{}-{}", start, end)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("{} (total: {})", ranges_str, total_count)
}

//==============================================================================
// DataSyncService Test Harness
//==============================================================================
struct DataSyncServiceTestHarness {
    inner: DataSyncServiceInner,
    msg_rx: UnboundedReceiver<irys_types::Traced<DataSyncServiceMessage>>,
}

impl DataSyncServiceTestHarness {
    fn new(
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        chunk_fetcher_factory: ChunkFetcherFactory,
        service_senders: ServiceSenders,
        rx: UnboundedReceiver<irys_types::Traced<DataSyncServiceMessage>>,
        config: Config,
    ) -> Self {
        let inner = DataSyncServiceInner::new(
            block_tree,
            storage_modules,
            peer_list,
            chunk_fetcher_factory,
            service_senders,
            config,
        );

        Self { inner, msg_rx: rx }
    }

    /// Process all pending messages in the queue
    fn process_pending_messages(&mut self) -> eyre::Result<usize> {
        let mut count = 0;
        while let Ok(traced) = self.msg_rx.try_recv() {
            self.inner.handle_message(traced.inner)?;
            self.inner.storage_modules.read().unwrap()[0]
                .sync_pending_chunks()
                .expect("sync pending chunks to disk");
            count += 1;
        }
        if count > 0 {
            debug!("Processed {} pending messages", count);
        }
        Ok(count)
    }

    /// Wait for messages for a specific duration, processing all that arrive
    async fn process_messages_for_duration(&mut self, duration: Duration) -> eyre::Result<usize> {
        let start = tokio::time::Instant::now();
        let mut count = 0;

        while start.elapsed() < duration {
            let remaining = duration - start.elapsed();
            match tokio::time::timeout(
                remaining.min(Duration::from_millis(100)),
                self.msg_rx.recv(),
            )
            .await
            {
                Ok(Some(traced)) => {
                    self.inner.handle_message(traced.inner)?;
                    self.inner.storage_modules.read().unwrap()[0]
                        .sync_pending_chunks()
                        .expect("sync pending chunks to disk");
                    count += 1;
                }
                Ok(None) => {
                    debug!("Message channel closed");
                    break;
                }
                Err(_) => {
                    // Timeout - no more messages for now
                    break;
                }
            }
        }

        if count > 0 {
            debug!(
                "Processed {} messages during {}ms wait (elapsed: {}ms)",
                count,
                duration.as_millis(),
                start.elapsed().as_millis()
            );
        } else {
            debug!("elapsed {}ms", start.elapsed().as_millis())
        }

        Ok(count)
    }

    /// Trigger a tick manually
    fn tick(&mut self) -> eyre::Result<()> {
        self.inner.storage_modules.read().unwrap()[0]
            .sync_pending_chunks()
            .expect("sync pending chunks should work");
        self.inner.tick()
    }

    /// Handle a message directly
    fn handle_message(&mut self, message: DataSyncServiceMessage) -> eyre::Result<()> {
        self.inner.handle_message(message)
    }

    /// Convenience method to start syncing
    fn start_sync(&mut self) -> eyre::Result<()> {
        self.handle_message(DataSyncServiceMessage::SyncPartitions)
    }

    /// Get peers list
    fn get_active_peers(
        &mut self,
    ) -> eyre::Result<Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>> {
        let (tx, mut rx) = oneshot::channel();
        self.handle_message(DataSyncServiceMessage::GetActivePeersList(tx))?;
        rx.try_recv()
            .map_err(|e| eyre::eyre!("Failed to receive peers: {}", e))
    }

    /// Take a performance snapshot and print results
    fn take_performance_snapshot(
        &mut self,
        label: &str,
        peer_addresses: &(IrysAddress, IrysAddress, IrysAddress),
        mock_fetchers: &HashMap<SocketAddr, Arc<MockChunkFetcher>>,
        storage_module: &Arc<StorageModule>,
    ) -> eyre::Result<()> {
        println!("\n=== {} Performance Snapshot ===", label);

        let active_peers = self.get_active_peers()?;
        let mut total_requests = 0;
        let active_peers = active_peers.read().unwrap();

        for (addr, peer_manager) in active_peers.iter() {
            let peer_name = match *addr {
                addr if addr == peer_addresses.0 => "Slow Peer",
                addr if addr == peer_addresses.1 => "Stable Peer",
                addr if addr == peer_addresses.2 => "Fast Peer",
                _ => "Unknown Peer",
            };

            let peer_fetcher = &mock_fetchers.get(&peer_manager.peer_address.api).unwrap();

            let request_count = peer_fetcher.request_log.read().unwrap().len();
            total_requests += request_count;

            println!("{}: Health={:.3}, Requests={}, Failures={}, Short-term BW={}, Medium-term BW={}, Stable={}, Improving={} Max Concurrency={}",
                peer_name,
                peer_manager.health_score(),
                request_count,
                peer_manager.total_failures(),
                peer_manager.short_term_bandwidth_bps(),
                peer_manager.medium_term_bandwidth_bps(),
                peer_manager.is_throughput_stable(),
                peer_manager.is_throughput_improving(),
                peer_manager.max_concurrency()
            );
        }

        println!("Total requests: {}", total_requests);

        // Also show current sync status
        let data_intervals = storage_module.get_intervals(ChunkType::Data);
        let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);

        println!("Data chunks synced: {}", format_intervals(&data_intervals));
        println!(
            "Entropy chunks remaining: {}",
            format_intervals(&entropy_intervals)
        );

        Ok(())
    }

    /// Run multiple ticks with message processing and optional performance snapshots
    async fn run_ticks_with_messages(
        &mut self,
        num_ticks: u32,
        tick_interval: Duration,
        peer_addresses: &(IrysAddress, IrysAddress, IrysAddress),
        mock_fetchers: &HashMap<SocketAddr, Arc<MockChunkFetcher>>,
        storage_module: &Arc<StorageModule>,
        snapshot_ticks: Vec<u32>, // Specify which ticks to take snapshots at
    ) -> eyre::Result<()> {
        for i in 0..num_ticks {
            let current_tick = i + 1;
            debug!("=== Tick {}/{} ===", current_tick, num_ticks);

            // Process any pending messages first
            self.process_pending_messages()?;

            // Run the tick
            debug!("Running tick...");
            self.tick()?;

            // Take performance snapshot if requested for this tick
            if snapshot_ticks.contains(&current_tick) {
                self.take_performance_snapshot(
                    &format!("Tick {}", current_tick),
                    peer_addresses,
                    mock_fetchers,
                    storage_module,
                )?;
            }

            if i < num_ticks - 1 {
                debug!("Waiting for messages...");
                // Wait for and process any messages that come in
                self.process_messages_for_duration(tick_interval).await?;
            }
        }
        Ok(())
    }
}

//==============================================================================
// TestSetup Framework
//==============================================================================
struct TestSetup {
    // Test peer addresses
    slow_peer_addr: IrysAddress,
    stable_peer_addr: IrysAddress,
    fast_peer_addr: IrysAddress,

    // Only one real storage module - the one that needs to sync data
    storage_module: Arc<StorageModule>,

    // Mapping between peer addresses and MockChunkFetchers to simulate peer chunks
    mock_fetchers: HashMap<SocketAddr, Arc<MockChunkFetcher>>,

    // Test infrastructure
    peer_list: PeerList,
    config: Config,
    service_senders: ServiceSenders,
    service_receivers: ServiceReceivers,
    block_tree: BlockTreeReadGuard,
}

impl TestSetup {
    fn new(num_chunks: u64, timeout: Duration, tmp_dir: &TempDir) -> Self {
        let base_path = tmp_dir.path().to_path_buf();

        debug!("tmp_dir: {:?}", tmp_dir);

        let chunk_size = 256 * 1024; // 256KB chunks

        // Create test config
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: num_chunks,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                block_migration_depth: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            data_sync: DataSyncServiceConfig {
                max_pending_chunk_requests: 100,
                max_storage_throughput_bps: 1000 * 1024 * 1024, // 100 MB/s as BPS
                bandwidth_adjustment_interval: Duration::from_secs(1),
                chunk_request_timeout: timeout,
            },
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        // Create mining addresses for all the mocked up peers
        let slow_peer_addr = IrysAddress::from([1_u8; 20]);
        let stable_peer_addr = IrysAddress::from([2_u8; 20]);
        let fast_peer_addr = IrysAddress::from([3_u8; 20]);
        let sync_peer_addr = IrysAddress::from([4_u8; 20]);

        // Create partition assignments for each of the peers, assigning them to the same
        // ledger_id(DataLedger::Publish) and slot_index(0)
        let ledger_id = DataLedger::Publish.into();
        let slot_index = 0;
        let partition_assignments = [
            PartitionAssignment {
                ledger_id: Some(ledger_id),
                slot_index: Some(slot_index),
                miner_address: slow_peer_addr,
                partition_hash: H256::random(),
            },
            PartitionAssignment {
                ledger_id: Some(ledger_id),
                slot_index: Some(slot_index),
                miner_address: stable_peer_addr,
                partition_hash: H256::random(),
            },
            PartitionAssignment {
                ledger_id: Some(ledger_id),
                slot_index: Some(slot_index),
                miner_address: fast_peer_addr,
                partition_hash: H256::random(),
            },
            PartitionAssignment {
                ledger_id: Some(ledger_id),
                slot_index: Some(slot_index),
                miner_address: sync_peer_addr,
                partition_hash: H256::random(),
            },
        ];

        // Create test socket addresses for API endpoints
        let slow_api_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8001);
        let stable_api_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8002);
        let fast_api_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8003);

        // Create peer list with performance characteristics
        let peers_data: Vec<PeerListItem> = vec![
            PeerListItem {
                peer_id: IrysPeerId::from(slow_peer_addr),
                mining_address: slow_peer_addr,
                address: PeerAddress {
                    gossip: slow_api_addr,
                    api: slow_api_addr,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
            },
            PeerListItem {
                peer_id: IrysPeerId::from(stable_peer_addr),
                mining_address: stable_peer_addr,
                address: PeerAddress {
                    gossip: stable_api_addr,
                    api: stable_api_addr,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
            },
            PeerListItem {
                peer_id: IrysPeerId::from(fast_peer_addr),
                mining_address: fast_peer_addr,
                address: PeerAddress {
                    gossip: fast_api_addr,
                    api: fast_api_addr,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
            },
        ];

        // Create service senders to finish initializing the PeerList
        let (service_senders, service_receivers) =
            irys_actors::test_helpers::build_test_service_senders();

        let peer_list = PeerList::from_peers(
            peers_data,
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("Failed to create peer list from peers");

        // Create data chunks for this partition to store
        let mut data = Vec::with_capacity((chunk_size * num_chunks) as usize);
        for chunk_index in 0..num_chunks {
            let chunk_data = vec![chunk_index as u8; chunk_size as usize];
            data.extend(chunk_data);
        }

        // Use a data transaction to create well formed UnpackedChunks for the partition
        debug!("Initializing partition data...");
        let signer = IrysSigner::random_signer(&config.consensus);

        // Make sure the genesis block track the ledger size
        let mut fake_genesis = IrysBlockHeader::new_mock_header();
        fake_genesis.data_ledgers[DataLedger::Publish].total_chunks = num_chunks;

        let data_tx = signer
            .create_transaction(data, fake_genesis.block_hash)
            .expect("To make a data transaction");
        let chunks = Self::create_test_chunks(&data_tx, chunk_size);

        // Create an pack a storage module for the data_sync_service to sync
        debug!("Initializing storage module");
        let storage_module =
            Self::create_and_pack_storage_module(&partition_assignments[3], &data_tx, &config);

        // Initialize the block_tree, a dependency of the data_sync_service
        debug!("Initializing block_tree...");
        let block_tree_guard =
            Self::initialize_block_tree(&partition_assignments, &fake_genesis, &config);

        // Make a MockChunkFetcher for each peer SocketAddr using the UnpackedChunk data
        debug!("create chunk fetchers");
        let pa = &partition_assignments;
        let mock_fetchers = Self::create_mock_chunk_fetchers(
            (&slow_api_addr, &pa[0]),
            (&stable_api_addr, &pa[1]),
            (&fast_api_addr, &pa[2]),
            chunks,
            &config,
        );

        // Return the TestSetup context
        debug!("returning context");
        Self {
            slow_peer_addr,
            stable_peer_addr,
            fast_peer_addr,
            storage_module: Arc::new(storage_module),
            mock_fetchers,
            peer_list,
            config,
            service_senders,
            service_receivers,
            block_tree: block_tree_guard,
        }
    }

    fn create_and_pack_storage_module(
        pa: &PartitionAssignment,
        data_tx: &DataTransaction,
        config: &Config,
    ) -> StorageModule {
        let num_chunks = data_tx.chunks.len() as u64;
        let storage_module_info = StorageModuleInfo {
            id: DataLedger::Publish as usize,
            partition_assignment: Some(*pa),
            submodules: vec![(
                partition_chunk_offset_ie!(0, num_chunks as u32),
                "chunks".into(),
            )],
        };
        let storage_module =
            StorageModule::new(&storage_module_info, config).expect("To create a storage module");

        let chunk_size = 256 * 1024; // 256KiB

        // Populate the storage module with empty/fake packing entropy chunks
        let mock_entropy_bytes: Vec<u8> = vec![0; chunk_size];
        for i in 0..num_chunks {
            storage_module.write_chunk(
                PartitionChunkOffset::from(i),
                mock_entropy_bytes.clone(),
                irys_domain::ChunkType::Entropy,
            );
            storage_module
                .sync_pending_chunks()
                .expect("pending entropy chunks should write to storage_module");
        }

        // Create test data and chunks
        let num_chunks = 256;
        let total_data_size = chunk_size * num_chunks;

        let mut data = Vec::with_capacity(total_data_size);
        for chunk_index in 0_u8..=255_u8 {
            let chunk_data = vec![chunk_index; chunk_size];
            data.extend(chunk_data);
        }

        // Assign the transactions data_root to the storage module
        let tx_path = vec![];
        storage_module
            .index_transaction_data(
                &data_tx.header,
                &tx_path,
                LedgerChunkRange(ledger_chunk_offset_ie!(0, num_chunks as u64)),
            )
            .map_err(|e| {
                error!(
                    "Failed to add tx path + data_root + start_offset to index: {}",
                    e
                );
            })
            .unwrap();

        debug!("=== INITIAL ENTROPY INTERVALS ===");
        let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
        let total_entropy: u64 = entropy_intervals
            .iter()
            .map(|i| i.end().to_u64().unwrap() - i.start().to_u64().unwrap() + 1)
            .sum();
        debug!("Initial entropy intervals: {:?}", entropy_intervals);
        debug!("Total entropy chunks: {}", total_entropy);

        storage_module
    }

    fn initialize_block_tree(
        partition_assignments: &[PartitionAssignment; 4],
        fake_genesis: &IrysBlockHeader,
        config: &Config,
    ) -> BlockTreeReadGuard {
        // Set up block tree with partition assignments
        let mut block_tree = BlockTree::new(fake_genesis, config.consensus.clone());
        debug!("Block tree instantiated");
        let mut partition_map: BTreeMap<H256, PartitionAssignment> = partition_assignments
            .iter()
            .map(|assignment| (assignment.partition_hash, *assignment))
            .collect();
        let mut partition_hashes: Vec<H256> = partition_assignments
            .iter()
            .map(|pa| pa.partition_hash)
            .collect();

        debug!("modifying fake genesis block in block_tree");
        block_tree
            .blocks
            .entry(fake_genesis.block_hash)
            .and_modify(|md| {
                let mut epoch_snapshot = (*md.epoch_snapshot).clone();
                epoch_snapshot
                    .partition_assignments
                    .data_partitions
                    .append(&mut partition_map);
                epoch_snapshot
                    .all_active_partitions
                    .append(&mut partition_hashes);
                md.epoch_snapshot = Arc::new(epoch_snapshot);
            });

        debug!("creating block_tree_read_guard");
        BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)))
    }

    #[must_use]
    fn create_test_chunks(tx: &DataTransaction, chunk_size: u64) -> Vec<UnpackedChunk> {
        let mut chunks = Vec::new();
        let chunk_size = chunk_size as usize;
        for (chunk_index, _chunk_node) in tx.chunks.iter().enumerate() {
            let data_root = tx.header.data_root;
            let data_size = tx.header.data_size;

            let proof = &tx.proofs[chunk_index];
            let data_path = Base64(proof.proof.clone());

            // Calculate chunk boundaries safely
            let start_idx = chunk_index * chunk_size;
            let end_idx = ((chunk_index + 1) * chunk_size).min(tx.data.as_ref().unwrap().len());

            let bytes = Base64(tx.data.as_ref().unwrap().0[start_idx..end_idx].to_vec());

            let chunk = UnpackedChunk {
                data_root,
                data_size,
                data_path,
                bytes,
                tx_offset: TxChunkOffset::from(chunk_index as u32),
            };

            chunks.push(chunk);
        }

        chunks
    }

    fn create_mock_chunk_fetchers(
        slow: (&SocketAddr, &PartitionAssignment),
        stable: (&SocketAddr, &PartitionAssignment),
        fast: (&SocketAddr, &PartitionAssignment),
        chunks: Vec<UnpackedChunk>,
        config: &Config,
    ) -> HashMap<SocketAddr, Arc<MockChunkFetcher>> {
        let ledger_id: u32 = DataLedger::Publish.into();

        let mut mock_fetchers = HashMap::default();
        let config = &config.consensus;

        mock_fetchers.insert(
            *slow.0,
            Arc::new(Self::create_slow_mock_fetcher(
                slow.1, &chunks, ledger_id, config,
            )),
        );

        mock_fetchers.insert(
            *stable.0,
            Arc::new(Self::create_stable_mock_fetcher(
                stable.1, &chunks, ledger_id, config,
            )),
        );

        mock_fetchers.insert(
            *fast.0,
            Arc::new(Self::create_stable_mock_fetcher(
                fast.1, &chunks, ledger_id, config,
            )),
        );

        mock_fetchers
    }

    fn create_slow_mock_fetcher(
        pa: &PartitionAssignment,
        unpacked_chunks: &Vec<UnpackedChunk>,
        ledger_id: u32,
        config: &ConsensusConfig,
    ) -> MockChunkFetcher {
        let mut fetcher = MockChunkFetcher::new(ledger_id as usize);

        // Load up the mocked up chunk responses so that about 30% of them
        // respond with timeouts
        for chunk in unpacked_chunks {
            let ledger_offset = LedgerChunkOffset::from(chunk.tx_offset.to_u64().unwrap());

            let offset_mod = ledger_offset.to_u64().unwrap() % 10;
            fetcher = match offset_mod {
                0..=2 => fetcher.with_timeout(ledger_offset), // 30% timeout
                7..9 => fetcher.with_not_found(ledger_offset), // 30% not found
                _ => {
                    // 60% success
                    let chunk = Self::pack_chunk(pa, chunk, config);
                    fetcher.with_chunk(ledger_offset, chunk)
                }
            };
        }

        fetcher
    }

    fn create_stable_mock_fetcher(
        pa: &PartitionAssignment,
        unpacked_chunks: &Vec<UnpackedChunk>,
        ledger_id: u32,
        config: &ConsensusConfig,
    ) -> MockChunkFetcher {
        let mut fetcher = MockChunkFetcher::new(ledger_id as usize);

        // All chunks succeed for stable/fast peer
        for chunk in unpacked_chunks {
            let ledger_offset = LedgerChunkOffset::from(chunk.tx_offset.to_u64().unwrap());
            let chunk = Self::pack_chunk(pa, chunk, config);
            fetcher = fetcher.with_chunk(ledger_offset, chunk);
        }

        fetcher
    }

    fn create_chunk_fetcher_factory(&self) -> ChunkFetcherFactory {
        let mock_fetchers = self.mock_fetchers.clone();
        Box::new(move |_ledger_id| -> Arc<dyn ChunkFetcher> {
            Arc::new(PeerAwareChunkFetcher {
                mock_fetchers: mock_fetchers.clone(),
            })
        })
    }

    fn pack_chunk(
        pa: &PartitionAssignment,
        chunk: &UnpackedChunk,
        consensus_config: &ConsensusConfig,
    ) -> PackedChunk {
        // With how our test is structured, tx_offset, partition_offset, and ledger_offset all overlap
        let partition_offset = PartitionChunkOffset::from(chunk.tx_offset.to_u64().unwrap());
        let mut entropy_chunk = Vec::<u8>::with_capacity(consensus_config.chunk_size as usize);
        compute_entropy_chunk(
            pa.miner_address,
            partition_offset.to_u64().unwrap(),
            pa.partition_hash.into(),
            consensus_config.entropy_packing_iterations,
            consensus_config.chunk_size as usize,
            &mut entropy_chunk,
            consensus_config.chain_id,
        );

        let packed_data = packing_xor_vec_u8(entropy_chunk, &chunk.bytes.0);
        PackedChunk {
            data_root: chunk.data_root,
            data_size: chunk.data_size,
            data_path: chunk.data_path.clone(),
            bytes: irys_types::Base64(packed_data),
            packing_address: pa.miner_address,
            partition_offset,
            tx_offset: chunk.tx_offset,
            partition_hash: pa.partition_hash,
        }
    }
}

//==============================================================================
// PeerAwareChunkFetcher
//==============================================================================

/// Routes chunk fetch requests to the appropriate mock fetcher based on peer API address.
///
/// This fetcher simulates a network of peers with different performance characteristics:
/// - Each `api_addr` consistently maps to the same `MockChunkFetcher`
/// - Mock fetchers are pre-configured with chunk responses and failure patterns
/// - Network delays are simulated based on the peer's port number
///
/// Used in integration tests to verify peer selection and bandwidth management
/// without requiring actual network calls.
#[derive(Debug)]
struct PeerAwareChunkFetcher {
    mock_fetchers: HashMap<SocketAddr, Arc<MockChunkFetcher>>,
}

#[async_trait::async_trait]
impl ChunkFetcher for PeerAwareChunkFetcher {
    async fn fetch_chunk(
        &self,
        ledger_chunk_offset: LedgerChunkOffset,
        api_addr: SocketAddr,
        timeout: Duration,
    ) -> Result<PackedChunk, ChunkFetchError> {
        let chunk_fetcher_for_peer =
            self.mock_fetchers
                .get(&api_addr)
                .ok_or_else(|| ChunkFetchError::NetworkError {
                    message: format!("No mock fetcher for peer: {}", api_addr),
                })?;

        // Simulate network delays based on peer characteristics
        let (_name, delay) = match api_addr.port() {
            8001 => ("slow", Duration::from_millis(500)),
            8002 => ("stable", Duration::from_millis(250)),
            8003 => ("fast", Duration::from_millis(150)),
            _ => ("default", Duration::from_millis(250)),
        };

        tokio::time::sleep(delay).await;

        // Delegate to the appropriate peers mock chunk fetcher
        let result = chunk_fetcher_for_peer
            .fetch_chunk(ledger_chunk_offset, api_addr, timeout)
            .await;

        // Simulate a timeout when there is one
        if let Err(err) = result.clone() {
            if err == ChunkFetchError::Timeout {
                tokio::time::sleep(timeout - delay * 2).await;
                debug!("Timing out request: {ledger_chunk_offset} {api_addr} ");
            }
        }

        result
    }
}
