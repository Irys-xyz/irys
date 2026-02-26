use crate::genesis_utilities::save_genesis_block_to_disk;
use crate::metrics;
use crate::peer_utilities::{fetch_genesis_block, fetch_genesis_commitments};
use actix_web::dev::Server;
use base58::ToBase58 as _;
use eyre::{ensure, Context as _};
use futures::FutureExt as _;
use irys_actors::{
    block_discovery::{
        BlockDiscoveryFacadeImpl, BlockDiscoveryMessage, BlockDiscoveryService,
        BlockDiscoveryServiceInner,
    },
    block_migration_service::BlockMigrationService,
    block_producer::BlockProducerCommand,
    block_tree_service::{BlockTreeService, BlockTreeServiceMessage},
    cache_service::ChunkCacheService,
    chunk_fetcher::{ChunkFetcherFactory, HttpChunkFetcher},
    chunk_migration_service::ChunkMigrationService,
    mempool_guard::MempoolReadGuard,
    mempool_service::MempoolServiceMessage,
    mempool_service::{MempoolService, MempoolServiceFacadeImpl},
    mining_bus::{MiningBus, MiningBusBroadcaster},
    packing_service::PackingRequest,
    partition_mining_service::{
        PartitionMiningController, PartitionMiningService, PartitionMiningServiceInner,
    },
    pledge_provider::MempoolPledgeProvider,
    reth_service::{ForkChoiceUpdateMessage, RethServiceMessage},
    services::ServiceSenders,
    validation_service::ValidationService,
    BlockValidationTracker, DataSyncService, StorageModuleService,
};
use irys_api_server::{create_listener, run_server, ApiState};
use irys_config::chain::chainspec::build_unsigned_irys_genesis_block;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::db::RethDbWrapper;
use irys_database::{add_genesis_commitments, database};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::forkchoice_markers::ForkChoiceMarkers;
use irys_domain::{
    reth_provider, BlockIndex, BlockIndexReadGuard, BlockTree, BlockTreeReadGuard, ChunkProvider,
    ChunkType, EpochReplayData, ExecutionPayloadCache, IrysRethProvider, IrysRethProviderInner,
    PeerList, StorageModule, StorageModuleInfo, StorageModulesReadGuard, SupplyState,
    SupplyStateReadGuard,
};
use irys_p2p::{
    spawn_peer_network_service, BlockPool, BlockStatusProvider, ChainSyncService,
    ChainSyncServiceInner, GossipDataHandler, GossipServer, P2PService,
    ServiceHandleWithShutdownSignal, SyncChainServiceFacade, SyncChainServiceMessage,
};
use irys_price_oracle::IrysPriceOracle;
use irys_price_oracle::SingleOracle;
use irys_reth_node_bridge::node::{NodeProvider, RethNode, RethNodeHandle};
pub use irys_reth_node_bridge::node::{RethNodeAddOns, RethNodeProvider};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_types::chainspec::irys_chain_spec;
use irys_types::BlockHash;
use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, BlockBody, CommitmentTransaction,
    Config, IrysBlockHeader, NodeConfig, NodeMode, OracleConfig, PartitionChunkRange,
    PeerNetworkSender, PeerNetworkServiceMessage, RethPeerInfo, SealedBlock, SendTraced as _,
    ServiceSet, SystemLedger, TokioServiceHandle, Traced, UnixTimestamp, UnixTimestampMs, H256,
    U256,
};
use irys_types::{NetworkConfigWithDefaults as _, ShutdownReason};
use irys_vdf::vdf::run_vdf_for_genesis_block;
use irys_vdf::{
    state::{AtomicVdfState, VdfStateReadonly},
    vdf::run_vdf,
    VdfStep,
};
use reth::{
    chainspec::ChainSpec,
    tasks::{RuntimeBuilder, RuntimeConfig, TaskExecutor, TokioConfig},
};
use reth_db::{transaction::DbTx as _, Database as _};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{
    net::TcpListener,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::{Handle, Runtime},
    sync::{
        mpsc,
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot::{self},
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn, Instrument as _};

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    // todo replace this with `IrysRethNodeAdapter` but that requires quite a bit of refactoring
    pub reth_handle: RethNodeProvider,
    pub reth_node_adapter: IrysRethNodeAdapter,
    pub reth_db: RethDbWrapper,

    pub db: DatabaseProvider,
    pub config: Config,
    pub genesis_hash: H256, // The actual genesis block hash for network consensus
    pub reward_curve: Arc<HalvingCurve>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub block_index_guard: BlockIndexReadGuard,
    pub block_tree_guard: BlockTreeReadGuard,
    pub mempool_guard: MempoolReadGuard,
    pub vdf_steps_guard: VdfStateReadonly,
    pub service_senders: ServiceSenders,
    pub partition_controllers: Vec<PartitionMiningController>,
    pub packing_waiter: irys_actors::packing_service::PackingIdleWaiter,
    // Shutdown channel — send a ShutdownReason to trigger graceful shutdown of the lifecycle task
    pub shutdown_sender: tokio::sync::mpsc::Sender<ShutdownReason>,
    // JoinHandle for the lifecycle task (replaces reth_done_rx)
    pub lifecycle_handle: Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<ShutdownReason>>>>,
    // Keeps the 2nd tokio runtime alive for the lifetime of the node
    pub tokio_runtime: Option<Arc<Runtime>>,
    // Top-level cancellation token for coordinated shutdown
    shutdown_token: CancellationToken,
    pub block_producer_inner: Arc<irys_actors::BlockProducerInner>,
    stop_guard: StopGuard,
    pub peer_list: PeerList,
    pub sync_state: ChainSyncState,
    pub validation_enabled: Arc<AtomicBool>,
    pub block_pool: Arc<BlockPool<BlockDiscoveryFacadeImpl, MempoolServiceFacadeImpl>>,
    pub gossip_data_handler:
        Arc<GossipDataHandler<MempoolServiceFacadeImpl, BlockDiscoveryFacadeImpl>>,
    pub storage_modules_guard: StorageModulesReadGuard,
    pub mempool_pledge_provider: Arc<MempoolPledgeProvider>,
    pub sync_service_facade: SyncChainServiceFacade,
    pub is_vdf_mining_enabled: Arc<AtomicBool>,
    pub started_at: Instant,
    pub supply_state_guard: Option<SupplyStateReadGuard>,
    pub chunk_ingress_state: irys_actors::ChunkIngressState,
    backfill_complete: Arc<tokio::sync::Notify>,
}

impl IrysNodeCtx {
    pub fn get_api_state(&self) -> ApiState {
        ApiState {
            mempool_service: self.service_senders.mempool.clone(),
            chunk_ingress: self.service_senders.chunk_ingress.clone(),
            mempool_guard: self.mempool_guard.clone(),
            chunk_provider: self.chunk_provider.clone(),
            peer_list: self.peer_list.clone(),
            db: self.db.clone(),
            config: self.config.clone(),
            reth_provider: self.reth_handle.clone(),
            reth_http_url: self.reth_handle.rpc_server_handle().http_url().unwrap(),
            block_tree: self.block_tree_guard.clone(),
            block_index: self.block_index_guard.clone(),
            supply_state: self.supply_state_guard.clone(),
            sync_state: self.sync_state.clone(),
            mempool_pledge_provider: self.mempool_pledge_provider.clone(),
            started_at: self.started_at,
            mining_address: self.config.node_config.miner_address(),
        }
    }

    pub fn get_gossip_server(
        &self,
    ) -> GossipServer<MempoolServiceFacadeImpl, BlockDiscoveryFacadeImpl> {
        GossipServer::new(
            self.gossip_data_handler.clone(),
            self.peer_list.clone(),
            self.config
                .node_config
                .p2p_gossip
                .max_concurrent_gossip_chunks,
        )
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn stop(self, reason: ShutdownReason) {
        info!("stop function called, shutting down due to: {}", reason);
        metrics::record_node_shutdown(reason.as_label());

        // Clone the inner DB Arc so we can wait for all references to drain
        // after dropping self.
        let db_inner = Arc::clone(&self.db.0);

        // Cancel all subsystems via token (VDF, actor, backfill all observe this)
        self.shutdown_token.cancel();

        // Wait for backfill task to complete (with timeout)
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.backfill_complete.notified(),
        )
        .await
        {
            Ok(()) => {
                debug!("Backfill task stopped cleanly");
            }
            Err(_) => {
                warn!("Backfill task did not stop within 5 second timeout");
            }
        }

        if let Err(e) = self.stop_mining() {
            error!("Failed to stop mining during shutdown: {:#}", e);
        }

        // Send shutdown reason to lifecycle task
        if let Err(e) = self.shutdown_sender.send(reason).await {
            debug!("Shutdown channel already closed: {}", e);
        }

        // Await lifecycle task completion (with timeout)
        let handle = self.lifecycle_handle.lock().unwrap().take();
        match handle {
            Some(jh) => match tokio::time::timeout(RETH_THREAD_STOP_TIMEOUT, jh).await {
                Ok(Ok(reason)) => info!("Lifecycle task stopped: {}", reason),
                Ok(Err(e)) => error!("Lifecycle task panicked: {:?}", e),
                Err(_) => {
                    error!("Lifecycle task did not stop within {RETH_THREAD_STOP_TIMEOUT:?}")
                }
            },
            None => debug!("Lifecycle handle already consumed"),
        }
        debug!("Lifecycle task stopped");

        // Flush telemetry before marking as stopped to ensure all logs are exported
        #[cfg(feature = "telemetry")]
        {
            match tokio::time::timeout(
                Duration::from_secs(15),
                tokio::task::spawn_blocking(irys_utils::flush_telemetry),
            )
            .await
            {
                Ok(Ok(Ok(_))) => {
                    // Successfully shut down telemetry - all logs exported
                }
                Ok(Ok(Err(e))) => {
                    error!("Telemetry shutdown error: {:?}", e);
                }
                Ok(Err(e)) => {
                    error!("Telemetry shutdown task panicked: {:?}", e);
                }
                Err(_) => {
                    error!("Telemetry shutdown timed out after 15s");
                }
            }
        }

        self.stop_guard.mark_stopped();

        // Drop self to release our references to all shared state (services,
        // block pool, gossip handler, etc. — all hold DatabaseProvider clones).
        drop(self);

        // Wait for all DatabaseProvider clones to be released so the MDBX
        // file lock is freed before the caller tries to re-open the DB.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&db_inner) > 1 {
            if Instant::now() > deadline {
                warn!(
                    refs = Arc::strong_count(&db_inner),
                    "DB still has outstanding references after 5s, proceeding"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // db_inner dropped here -> DatabaseEnv dropped -> MDBX lock released
    }

    pub fn get_http_port(&self) -> u16 {
        self.config.node_config.http.bind_port
    }

    /// Stop the VDF thread and send a message to all known partition actors to ignore any received VDF steps
    pub fn stop_mining(&self) -> eyre::Result<()> {
        // stop the VDF thread
        self.stop_vdf();
        self.set_partition_mining(false)
    }
    /// Start VDF thread and send a message to all known partition actors to begin mining when they receive a VDF step
    pub fn start_mining(&self) -> eyre::Result<()> {
        // start the VDF thread
        self.start_vdf();
        self.set_partition_mining(true)
    }
    // Send a custom control message to all known partition actors to enable/disable partition mining
    // does NOT modify the state of the  VDF thread!
    pub fn set_partition_mining(&self, should_mine: bool) -> eyre::Result<()> {
        // Send a control command to all partition mining services
        for ctrl in &self.partition_controllers {
            ctrl.set_mining(should_mine);
        }
        Ok(())
    }
    // starts the VDF thread
    pub fn start_vdf(&self) {
        self.vdf_state(true)
    }
    // stops the VDF thread
    pub fn stop_vdf(&self) {
        self.vdf_state(false)
    }
    // sets the running state of the VDF thread
    pub fn vdf_state(&self, running: bool) {
        self.is_vdf_mining_enabled.store(running, Ordering::Relaxed);
    }

    /// Sets whether the validation service should process incoming validation messages
    pub fn set_validation_enabled(&self, enabled: bool) {
        self.validation_enabled.store(enabled, Ordering::Relaxed);
    }
}

// Shared stop guard that can be cloned
#[derive(Debug)]
struct StopGuard(Arc<AtomicBool>);

impl StopGuard {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn mark_stopped(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        // Only check if this is the last reference to the guard
        if Arc::strong_count(&self.0) == 1 && !self.is_stopped() && !std::thread::panicking() {
            error!("\x1b[1;31m============================================================\x1b[0m");
            error!("\x1b[1;31mIrysNodeCtx must be stopped before all instances are dropped\x1b[0m");
            error!("\x1b[1;31m============================================================\x1b[0m");
        }
    }
}

impl Clone for StopGuard {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

async fn start_reth_node(
    task_executor: TaskExecutor,
    chainspec: Arc<ChainSpec>,
    config: Config,
    latest_block: u64,
) -> eyre::Result<(RethNodeHandle, RethNode)> {
    let random_ports = config.node_config.reth.network.use_random_ports;
    let (node_handle, _reth_node_adapter) = irys_reth_node_bridge::node::run_node(
        chainspec.clone(),
        task_executor.clone(),
        config.node_config.clone(),
        latest_block,
        random_ports,
    )
    .in_current_span()
    .await?;

    debug!("Reth node started");

    let reth_node = node_handle.node.clone();
    Ok((node_handle, reth_node))
}

/// Builder pattern for configuring and bootstrapping an Irys blockchain node.
pub struct IrysNode {
    pub config: Config,
    pub http_listener: TcpListener,
    pub gossip_listener: TcpListener,
    pub irys_db: DatabaseProvider,
}

/// Timeout for stopping the API server during graceful shutdown.
const API_SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for stopping the gossip service during graceful shutdown.
const GOSSIP_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for the VDF thread to finish during graceful shutdown.
const VDF_THREAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for the lifecycle task to complete its full shutdown sequence.
/// Budget: API(5s) + gossip(5s) + VDF(10s) + service_set(~10 services × 10s) + reth tasks(10s).
/// Using 60s to cover typical case with margin; worst-case depends on service count.
const RETH_THREAD_STOP_TIMEOUT: Duration = Duration::from_secs(60);

impl IrysNode {
    /// Binds HTTP and gossip TCP listeners, updates config with assigned ports.
    /// Call once, then pass results to [`new_with_listeners()`].
    pub fn bind_listeners(
        mut node_config: NodeConfig,
    ) -> eyre::Result<(NodeConfig, TcpListener, TcpListener)> {
        let http_listener = create_listener(
            format!(
                "{}:{}",
                node_config.http.bind_ip(&node_config.network_defaults),
                node_config.http.bind_port
            )
            .parse()
            .expect("A valid HTTP IP & port"),
        )?;
        let gossip_listener = create_listener(
            format!(
                "{}:{}",
                node_config.gossip.bind_ip(&node_config.network_defaults),
                node_config.gossip.bind_port
            )
            .parse()
            .expect("A valid gossip IP & port"),
        )?;
        let local_addr = http_listener
            .local_addr()
            .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?;
        let local_gossip = gossip_listener
            .local_addr()
            .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?;

        // if `config.port` == 0, the assigned port will be random (decided by the OS)
        // we re-assign the configuration with the actual port here.
        if node_config.http.bind_port == 0 {
            node_config.http.bind_port = local_addr.port();
        }

        // If the public port is not specified, use the same as the private one
        if node_config.http.public_port == 0 {
            node_config.http.public_port = node_config.http.bind_port;
        }

        if node_config.gossip.bind_port == 0 {
            node_config.gossip.bind_port = local_gossip.port();
        }

        if node_config.gossip.public_port == 0 {
            node_config.gossip.public_port = node_config.gossip.bind_port;
        }

        Ok((node_config, http_listener, gossip_listener))
    }

    /// Creates an IrysNode with pre-bound listeners.
    /// Performs peer_id creation, DB initialization, and config validation.
    pub fn new_with_listeners(
        node_config: NodeConfig,
        http_listener: TcpListener,
        gossip_listener: TcpListener,
    ) -> eyre::Result<Self> {
        let peer_id = get_or_create_peer_id(&node_config)?;
        let irys_db = init_irys_db(&node_config)?;
        let config = Config::new(node_config, peer_id);
        config.validate()?;

        Ok(Self {
            config,
            http_listener,
            gossip_listener,
            irys_db,
        })
    }

    async fn get_or_create_genesis_info(
        &self,
        node_mode: &NodeMode,
        irys_db: &DatabaseProvider,
        block_index: &BlockIndex,
    ) -> eyre::Result<(IrysBlockHeader, Vec<CommitmentTransaction>, Arc<ChainSpec>)> {
        info!(
            config.miner_address = ?self.config.node_config.miner_address(),
            "Starting Irys Node: {:?}", node_mode
        );

        // Check if blockchain data already exists
        let has_existing_data = block_index.num_blocks() > 0;

        if has_existing_data {
            // CASE 1: Load existing genesis block and commitments from database
            let (block, commitments) = self.load_existing_genesis(irys_db, block_index);
            let timestamp_secs = block.timestamp_secs().as_secs();
            return Ok((
                block,
                commitments,
                irys_chain_spec(
                    self.config.consensus.chain_id,
                    &self.config.consensus.reth,
                    &self.config.consensus.hardforks,
                    timestamp_secs,
                )?,
            ));
        }

        // CASE 2: No existing data - handle based on node mode
        match node_mode {
            NodeMode::Genesis => {
                // Create a new genesis block for network initialization
                self.create_new_genesis_block().await
            }
            NodeMode::Peer => {
                let expected_genesis_hash = self
                    .config
                    .consensus
                    .expected_genesis_hash
                    .expect("expected_genesis_hash must be configured for peer nodes");
                // Fetch genesis data from trusted peer when joining network
                let (block, commitments) = self
                    .fetch_genesis_from_trusted_peer(expected_genesis_hash)
                    .await;
                let timestamp_secs = block.timestamp_secs().as_secs();
                // TODO: we should enforce this
                // assert_eq!(
                //     timestamp_secs,
                //     (self.config.consensus.genesis.timestamp_millis / 1000) as u64
                // );
                Ok((
                    block,
                    commitments,
                    irys_chain_spec(
                        self.config.consensus.chain_id,
                        &self.config.consensus.reth,
                        &self.config.consensus.hardforks,
                        timestamp_secs,
                    )?,
                ))
            }
        }
    }

    // Helper methods to flatten the main function
    fn load_existing_genesis(
        &self,
        irys_db: &DatabaseProvider,
        block_index: &BlockIndex,
    ) -> (IrysBlockHeader, Vec<CommitmentTransaction>) {
        // Get the genesis block hash from index
        let block_item = block_index
            .get_item(0)
            .expect("a block index item at index 0 in the block_index");

        // Retrieve genesis block header from database
        let tx = irys_db.tx().unwrap();
        let genesis_block = database::block_header_by_hash(&tx, &block_item.block_hash, false)
            .unwrap()
            .expect("Expect to find genesis block header in irys_db");

        // Find commitment ledger in system ledgers
        let commitment_ledger = genesis_block
            .system_ledgers
            .iter()
            .find(|e| e.ledger_id == SystemLedger::Commitment)
            .expect("Commitment ledger should exist in the genesis block");

        // Load all commitment transactions referenced in the ledger
        let mut commitments = Vec::new();
        for commitment_txid in commitment_ledger.tx_ids.iter() {
            let commitment_tx = database::commitment_tx_by_txid(&tx, commitment_txid)
                .expect("Expect to be able to read tx_header from db")
                .expect("Expect commitment transaction to be present in irys_db");

            commitments.push(commitment_tx);
        }

        drop(tx);

        (genesis_block, commitments)
    }

    async fn create_new_genesis_block(
        &self,
    ) -> eyre::Result<(IrysBlockHeader, Vec<CommitmentTransaction>, Arc<ChainSpec>)> {
        // Create timestamp for genesis block (prefer configured value if provided)
        let configured_ts = self.config.consensus.genesis.timestamp_millis;
        let timestamp_millis = if configured_ts != 0 {
            configured_ts
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };

        // Convert to seconds for reth
        let timestamp_secs = Duration::from_millis(timestamp_millis.try_into()?).as_secs();

        let reth_chain_spec = irys_chain_spec(
            self.config.consensus.chain_id,
            &self.config.consensus.reth,
            &self.config.consensus.hardforks,
            timestamp_secs,
        )?;

        // Get hardfork params for genesis block using its timestamp
        let number_of_ingress_proofs_total = self
            .config
            .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(timestamp_secs));
        let mut genesis_block = build_unsigned_irys_genesis_block(
            &self.config.consensus.genesis,
            reth_chain_spec.genesis_hash(),
            number_of_ingress_proofs_total,
        );

        // Prefer configured last_epoch_hash if provided (builder already set this, this ensures consistency)
        if self.config.consensus.genesis.last_epoch_hash != H256::zero() {
            genesis_block.last_epoch_hash = self.config.consensus.genesis.last_epoch_hash;
        }
        genesis_block.timestamp = UnixTimestampMs::from_millis(timestamp_millis);
        genesis_block.last_diff_timestamp = UnixTimestampMs::from_millis(timestamp_millis);

        // Add commitment transactions to genesis block and get initial treasury
        let (commitments, initial_treasury) =
            add_genesis_commitments(&mut genesis_block, &self.config).await;

        // Calculate initial difficulty based on number of storage modules
        let storage_module_count = (commitments.len() - 1) as u64; // Subtract 1 for stake commitment
        let difficulty =
            calculate_initial_difficulty(&self.config.consensus, storage_module_count as f64)
                .expect("valid calculated initial difficulty");
        genesis_block.diff = difficulty;

        // Set the genesis treasury to the total value of all commitments
        genesis_block.treasury = initial_treasury;

        // Note: commitments are persisted to DB in `persist_genesis_block_and_commitments()` later on

        run_vdf_for_genesis_block(&mut genesis_block, &self.config.vdf);

        // Sign the genesis block with the node's miner key
        let signer = self.config.irys_signer();
        signer
            .sign_block_header(&mut genesis_block)
            .expect("Failed to sign genesis block");

        info!("=====================================");
        info!("GENESIS BLOCK CREATED");
        info!("Hash: {}", genesis_block.block_hash);
        info!("Add this to consensus configs:");
        info!(
            "consensus.expected_genesis_hash = \"{}\"",
            genesis_block.block_hash
        );
        info!("=====================================");

        Ok((genesis_block, commitments, reth_chain_spec))
    }

    #[tracing::instrument(level = "trace", skip_all, fields(expected_genesis_hash))]
    async fn fetch_genesis_from_trusted_peer(
        &self,
        expected_genesis_hash: H256,
    ) -> (IrysBlockHeader, Vec<CommitmentTransaction>) {
        tracing::Span::current().record(
            "expected_genesis_hash",
            format_args!("{}", expected_genesis_hash),
        );

        // Get trusted peer from config
        let trusted_peer = &self
            .config
            .node_config
            .trusted_peers
            .first()
            .expect("expected at least one trusted peer in config")
            .api;

        info!("Fetching genesis block from trusted peer: {}", trusted_peer);

        // Create HTTP client and fetch genesis block
        let http_client = reqwest::Client::new();
        let genesis_block = fetch_genesis_block(trusted_peer, &http_client)
            .await
            .expect("expected genesis block from http api");

        // Fetch associated commitment transactions
        let commitments = fetch_genesis_commitments(trusted_peer, &genesis_block)
            .await
            .expect("Must be able to read genesis commitment tx from trusted peer");

        // Validate the fetched genesis block
        if !genesis_block.is_signature_valid() {
            panic!(
                "FATAL: Invalid genesis block signature from trusted peer. Block hash: {} miner: {}",
                genesis_block.block_hash,
                genesis_block.miner_address
            );
        }
        if genesis_block.block_hash != expected_genesis_hash {
            panic!(
                "FATAL: Genesis block hash mismatch!\nExpected: {}\nReceived: {}\nCannot join network - wrong genesis block",
                expected_genesis_hash, genesis_block.block_hash
            );
        }

        (genesis_block, commitments)
    }

    /// Persists the genesis block and its associated commitment transactions to the database
    ///
    /// This function is called only during initial blockchain setup
    ///
    /// # Arguments
    /// * `genesis_block` - The genesis block header to persist
    /// * `genesis_commitments` - The commitment transactions associated with the genesis block
    ///
    /// # Returns
    /// * `eyre::Result<()>` - Success or error result of the database operations
    #[tracing::instrument(level = "trace", skip_all)]
    fn persist_genesis_block_and_commitments(
        &self,
        genesis_block: &IrysBlockHeader,
        genesis_commitments: &[CommitmentTransaction],
        irys_db: &DatabaseProvider,
    ) -> eyre::Result<()> {
        info!("Initializing database with genesis block and commitments");

        // Save genesis block to disk for reference
        if let Err(e) = save_genesis_block_to_disk(
            Arc::new(genesis_block.clone()),
            &self.config.node_config.base_directory,
        ) {
            warn!("Failed to save genesis block to disk: {}", e);
            // Continue even if saving to disk fails - not critical
        }

        // Open a database transaction
        let write_tx = irys_db.tx_mut()?;

        // Insert the genesis block header
        database::insert_block_header(&write_tx, genesis_block)?;

        // Insert all commitment transactions
        for commitment_tx in genesis_commitments {
            debug!("Persisting genesis commitment: {}", commitment_tx.id());
            database::insert_commitment_tx(&write_tx, commitment_tx)?;
        }

        // Insert the genesis block index entry.
        // Use full sealing here so we verify genesis commitments match the header.
        let genesis_body = BlockBody {
            block_hash: genesis_block.block_hash,
            data_transactions: vec![],
            commitment_transactions: genesis_commitments.to_vec(),
        };
        let genesis_sealed = SealedBlock::new(genesis_block.clone(), genesis_body)?;
        BlockIndex::push_block(&write_tx, &genesis_sealed, self.config.consensus.chunk_size)?;

        // Commit the database transaction
        write_tx.commit()?;

        info!("Genesis block and commitments successfully persisted");
        Ok(())
    }

    /// Initializes the node (genesis or non-genesis)
    #[tracing::instrument(level = "trace", skip_all, fields(node.mode = ?self.config.node_config.node_mode))]
    pub async fn start(mut self) -> eyre::Result<IrysNodeCtx> {
        // Determine node startup mode (Copy, avoids borrowing self.config)
        let node_mode = self.config.node_config.node_mode;

        // Use the irys_db already initialized in new()
        let irys_db = self.irys_db.clone();
        let block_index = BlockIndex::new(&self.config.node_config, irys_db.clone())
            .expect("initializing a new block index should be doable");

        // Gets or creates the genesis block and commitments regardless of node mode
        let (genesis_block, genesis_commitments, reth_chainspec) = self
            .get_or_create_genesis_info(&node_mode, &irys_db, &block_index)
            .await?;

        // Capture the genesis hash for network consensus
        let genesis_hash = genesis_block.block_hash;
        info!("Node starting with genesis hash: {}", genesis_hash);

        // Genesis node: now that the genesis hash is known, set expected_genesis_hash
        // so our consensus config hash matches peer nodes during P2P handshakes.
        if self.config.consensus.expected_genesis_hash.is_none() {
            info!(
                "Setting expected_genesis_hash to {} (was None)",
                genesis_hash
            );
            self.config = self.config.clone().with_expected_genesis_hash(genesis_hash);
            info!(
                "Consensus config hash after update: {}",
                self.config.consensus.keccak256_hash()
            );
        } else {
            info!(
                "expected_genesis_hash already set to {:?}, consensus hash: {}",
                self.config.consensus.expected_genesis_hash,
                self.config.consensus.keccak256_hash()
            );
        }

        // Persist the genesis block to the block_index and db if it's not there already
        if block_index.num_blocks() == 0 {
            self.persist_genesis_block_and_commitments(
                &genesis_block,
                &genesis_commitments,
                &irys_db,
            )?;
        }

        #[cfg(feature = "nvidia")]
        {
            use irys_packing::cuda_config::{CUDAConfig, NvidiaGpuConfig};
            let device_count =
                NvidiaGpuConfig::get_device_count().map_err(|e| eyre::eyre!("{}", &e))?;
            // only check GPU0, we can only use a single GPU right now.
            let gpu_info = NvidiaGpuConfig::query_device(0).map_err(|e| eyre::eyre!("{}", &e))?;
            if device_count > 1 {
                warn!("Unable to use multiple GPUs right now - using device 0")
            }
            info!("Found 1 CUDA GPUs - {}", &gpu_info.device_name);
            let gpu_config = CUDAConfig::from_device_default()?;
            info!(
                "Using blocks: {}, threads per block: {}",
                &gpu_config.blocks, &gpu_config.threads_per_block
            );
        }

        // all async tasks will be run on a new tokio runtime
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let reth_runtime = RuntimeBuilder::new(
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(tokio_runtime.handle().clone())),
        )
        .build()?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<ShutdownReason>(1);
        let (irys_node_ctx_tx, irys_node_ctx_rx) = oneshot::channel::<IrysNodeCtx>();
        let irys_provider = reth_provider::create_provider();
        let shutdown_token = CancellationToken::new();

        // read the latest block info
        let (latest_block_height, latest_block) = read_latest_block_data(&block_index, &irys_db);
        let task_executor = reth_runtime.clone();

        let lifecycle_handle = tokio_runtime.spawn(
            Self::node_lifecycle(
                self.config.clone(),
                Arc::clone(&latest_block),
                latest_block_height,
                genesis_hash,
                shutdown_rx,
                irys_node_ctx_tx,
                irys_provider.clone(),
                reth_runtime,
                reth_chainspec.clone(),
                task_executor.clone(),
                self.http_listener,
                irys_db,
                block_index,
                self.gossip_listener,
                tokio_runtime.handle().clone(),
                shutdown_token.clone(),
            )
            .in_current_span(),
        );

        let handle = tokio_runtime.handle().clone();
        let mut ctx = irys_node_ctx_rx.await?;
        ctx.lifecycle_handle = Arc::new(std::sync::Mutex::new(Some(lifecycle_handle)));
        ctx.shutdown_sender = shutdown_tx;
        ctx.shutdown_token = shutdown_token;
        ctx.tokio_runtime = Some(Arc::new(tokio_runtime));
        let node_config = &ctx.config.node_config;

        // Log startup information
        info!(
            target = "started-node",
            "Started node! ({:?})\nMining address: {}\nReth Peer ID: {}\nHTTP: {}:{},\nGossip: {}:{}\nReth peering: {}:{}",
            &node_mode,
            &ctx.config.node_config.miner_address().to_base58(),
            ctx.reth_handle.network.peer_id(),
            node_config.http.bind_ip(&node_config.network_defaults),
            &node_config.http.bind_port,
            node_config.gossip.bind_ip(&node_config.network_defaults),
            &node_config.gossip.bind_port,
            node_config.reth.network.bind_ip(&node_config.network_defaults),
            &node_config.reth.network.bind_port,

        );

        // Subscribe before initial_sync so the receiver captures all block
        // events produced during (and after) sync — prevents a race where
        // events fire before we start listening.
        let block_state_rx = ctx
            .config
            .node_config
            .stake_pledge_drives
            .then(|| ctx.service_senders.subscribe_block_state_updates());

        // This is going to resolve instantly for a genesis node with 0 blocks,
        //  going to wait for sync otherwise.
        ctx.sync_service_facade.initial_sync().await?;

        // Call stake_and_pledge after mempool service is initialized
        if ctx.config.node_config.stake_pledge_drives {
            let block_tree_guard = ctx.block_tree_guard.clone();
            let service_senders = ctx.service_senders.clone();
            let config = ctx.config.clone();
            let storage_modules = ctx.storage_modules_guard.clone();
            let mempool_pledge_provider = ctx.mempool_pledge_provider.clone();
            let latest_block = Arc::clone(&latest_block);
            let sync_state = ctx.sync_state.clone();
            // this is a task as we don't want to block startup, & it lets us gossip blocks to the peer in the auto_stake_pledge test so it syncs to the network tip
            handle.spawn(async move {
                // wait for sync to complete so gossiped blocks are pre-validated
                let _ = sync_state.wait_for_sync().await;
                // Wait until block events quiesce (no events for 500 ms, max 10 s).
                // The receiver was subscribed before initial_sync so no events
                // were missed.
                if let Some(mut rx) = block_state_rx {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    irys_actors::services::wait_until_broadcast_idle(
                        &mut rx,
                        Duration::from_millis(500),
                        deadline,
                    )
                    .await;
                }
                let config = config;
                let latest_block = latest_block;
                let mut validation_tracker =
                    BlockValidationTracker::new(block_tree_guard.clone(), service_senders);
                // wait for any pending blocks to finish validating
                let latest_hash = validation_tracker
                    .wait_for_validation()
                    .await
                    .context("Unable to wait for validation to finish")
                    .unwrap();

                {
                    let btrg = block_tree_guard.read();
                    debug!(
                        "Checking stakes & pledges at height {}, latest hash: {}",
                        btrg.get_canonical_chain().0.last().unwrap().height(),
                        &latest_hash
                    );
                };
                // TODO: add code to proactively grab the latest head block from peers
                // this only really affects tests, as in a network deployment other nodes will be continuously mining & gossiping, which will trigger a sync to the network head
                stake_and_pledge(
                    &config,
                    block_tree_guard,
                    storage_modules,
                    latest_block.block_hash,
                    mempool_pledge_provider,
                )
                .await
                .context("Unable to automatically stake & pledge")
                .unwrap()
            });
        }

        // spawn a task to periodically log system info
        {
            use irys_actors::MempoolServiceMessage;

            let block_index = ctx.block_index_guard.clone();
            let block_tree = ctx.block_tree_guard.clone();
            let peer_list = ctx.peer_list.clone();
            let sync_state = ctx.sync_state.clone();
            let started_at = ctx.started_at;
            let mining_address = ctx.config.node_config.miner_address();
            let chain_id = ctx.config.consensus.chain_id;
            // let mempool_tx = ctx.service_senders.mempool.clone();
            let (tx, rx) = oneshot::channel();
            ctx.service_senders
                .mempool
                .send_traced(MempoolServiceMessage::GetState(tx))?;
            let mempool = rx.await?;
            let config = ctx.config.clone();
            let is_vdf_mining_enabled = ctx.is_vdf_mining_enabled.clone();
            let storage_modules_guard = ctx.storage_modules_guard.clone();
            let chunk_ingress_state = ctx.chunk_ingress_state.clone();
            // use executor so we get automatic termination when the node starts to shut down
            task_executor.spawn_task(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(60));

                loop {
                    interval.tick().await;

                    let info = irys_domain::get_node_info(
                        &block_index,
                        &block_tree,
                        &peer_list,
                        &sync_state,
                        started_at,
                        mining_address,
                        chain_id,
                    )
                    .await;

                    let pl_info = peer_list
                        .all_peers_sorted_by_score()
                        .into_iter()
                        .map(|(_, i)| i)
                        .collect::<Vec<_>>();

                    let mempool_status = mempool.get_status(&config.node_config, &chunk_ingress_state).await;

                    info!(
                    target = "node-state",
                    "Info:\n{:#?}\nPeer List: {:#?}\nMempool: pending_chunks: {}, pending_submit_txs: {}, pending_pledges: {}", &info, &pl_info, mempool_status.pending_chunks_count, mempool_status.data_tx_count, mempool_status.pending_pledges_count
                );

                    metrics::record_block_height(info.height);
                    metrics::record_peer_count(info.peer_count as u64);
                    metrics::record_pending_chunks(mempool_status.pending_chunks_count as u64);
                    metrics::record_pending_data_txs(mempool_status.data_tx_count as u64);
                    metrics::record_sync_state(!info.is_syncing);
                    metrics::record_node_up();
                    metrics::record_node_uptime();
                    metrics::record_vdf_mining_enabled(
                        is_vdf_mining_enabled.load(std::sync::atomic::Ordering::Relaxed),
                    );
                    let modules = storage_modules_guard.read();
                    let total = modules.len() as u64;
                    let assigned = modules
                        .iter()
                        .filter(|sm| sm.partition_assignment().is_some())
                        .count() as u64;
                    drop(modules);
                    metrics::record_storage_modules_total(total);
                    metrics::record_partitions_assigned(assigned);
                    metrics::record_partitions_unassigned(total - assigned);
                }
            });
        }

        Ok(ctx)
    }

    /// Single async lifecycle task that replaces the old init_services_thread + init_reth_thread.
    /// Runs on the 2nd tokio runtime. Performs reth startup, service init, runs until exit, then
    /// performs ordered shutdown.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %latest_block.block_hash, block.height = %latest_block.height))]
    async fn node_lifecycle(
        config: Config,
        latest_block: Arc<IrysBlockHeader>,
        latest_block_height: u64,
        genesis_hash: H256,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<ShutdownReason>,
        irys_node_ctx_tx: oneshot::Sender<IrysNodeCtx>,
        irys_provider: IrysRethProvider,
        reth_runtime: reth::tasks::Runtime,
        reth_chainspec: Arc<ChainSpec>,
        task_exec: TaskExecutor,
        http_listener: TcpListener,
        irys_db: DatabaseProvider,
        block_index: BlockIndex,
        gossip_listener: TcpListener,
        runtime_handle: Handle,
        shutdown_token: CancellationToken,
    ) -> ShutdownReason {
        // Phase 1: Start reth (sequential)
        let exec = reth_runtime.clone();
        let (node_handle, reth_node) =
            start_reth_node(exec, reth_chainspec, config.clone(), latest_block_height)
                .in_current_span()
                .await
                .expect("to be able to start the reth node");

        // Phase 2: Init services (sequential, receives reth_node directly)
        let (irys_node_ctx, actix_server, vdf_done_rx, gossip_service_handle, service_set) =
            Self::init_services(
                &config,
                genesis_hash,
                reth_node,
                block_index,
                latest_block,
                irys_provider.clone(),
                &task_exec,
                http_listener,
                irys_db,
                gossip_listener,
                runtime_handle,
                shutdown_token.clone(),
            )
            .in_current_span()
            .await
            .expect("initializing services should not fail");

        // Send IrysNodeCtx back to start()
        irys_node_ctx_tx
            .send(irys_node_ctx)
            .expect("irys node ctx sender should not be dropped");

        // Phase 3: Run until exit signal
        let mut service_set = std::pin::pin!(service_set);
        let task_manager_handle = reth_runtime.take_task_manager_handle();
        let reth_exit = std::pin::pin!(node_handle.node_exit_future);

        let shutdown_reason = tokio::select! {
            _ = &mut service_set => {
                ShutdownReason::Signal("service_exited".to_string())
            },
            res = async {
                match task_manager_handle {
                    Some(handle) => handle.await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                ShutdownReason::Signal(format!("reth_task_manager: {:?}", res))
            },
            _ = reth_exit => {
                ShutdownReason::Signal("reth_exit".to_string())
            },
            _ = shutdown_token.cancelled() => {
                ShutdownReason::Signal("cancellation_token".to_string())
            },
            reason = shutdown_rx.recv() => {
                reason.unwrap_or(ShutdownReason::Signal("shutdown_channel_closed".to_string()))
            },
            _ = tokio::signal::ctrl_c() => {
                ShutdownReason::Signal("ctrl_c".to_string())
            },
        };

        info!("Lifecycle task shutting down: {}", shutdown_reason);

        // Phase 4: Ordered shutdown
        // Stop actix server
        let server_handle = actix_server.handle();
        debug!("Stopping API server");
        match tokio::time::timeout(API_SERVER_STOP_TIMEOUT, server_handle.stop(true)).await {
            Ok(()) => debug!("API server stopped"),
            Err(_) => error!("API server stop timed out after {API_SERVER_STOP_TIMEOUT:?}"),
        }

        // Stop gossip
        match tokio::time::timeout(GOSSIP_STOP_TIMEOUT, gossip_service_handle.stop()).await {
            Ok(Ok(())) => info!("Gossip service stopped"),
            Ok(Err(e)) => warn!("Gossip service already stopped: {:?}", e),
            Err(_) => error!("Gossip service stop timed out after {GOSSIP_STOP_TIMEOUT:?}"),
        }

        // Wait for VDF thread
        debug!("Waiting for VDF thread to finish");
        match tokio::time::timeout(VDF_THREAD_TIMEOUT, vdf_done_rx).await {
            Ok(Ok(())) => debug!("VDF thread finished"),
            Ok(Err(_)) => error!("VDF thread likely panicked (completion channel dropped)"),
            Err(_) => error!("VDF thread did not finish within {VDF_THREAD_TIMEOUT:?}"),
        }

        // Graceful shutdown of actor services
        service_set.graceful_shutdown().await;
        debug!("Shutting down the rest of the reth jobs in case there are unfinished ones");

        // Graceful shutdown of reth tasks
        reth_runtime.graceful_shutdown();

        // Close reth DB (MDBX close is sync, use spawn_blocking)
        let reth_node_for_close = node_handle.node;
        tokio::task::spawn_blocking(move || {
            reth_node_for_close.provider.database.db.close();
            reth_provider::cleanup_provider(&irys_provider);
        })
        .await
        .expect("DB close task should not panic");

        info!("Lifecycle task finished with reason: {}", shutdown_reason);
        shutdown_reason
    }

    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %latest_block.block_hash, block.height = %latest_block.height))]
    async fn init_services(
        config: &Config,
        genesis_hash: H256,
        reth_node: RethNode,
        block_index: BlockIndex,
        latest_block: Arc<IrysBlockHeader>,
        irys_provider: IrysRethProvider,
        task_exec: &TaskExecutor,
        http_listener: TcpListener,
        irys_db: DatabaseProvider,
        gossip_listener: TcpListener,
        runtime_handle: tokio::runtime::Handle,
        shutdown_token: CancellationToken,
    ) -> eyre::Result<(
        IrysNodeCtx,
        Server,
        oneshot::Receiver<()>,
        ServiceHandleWithShutdownSignal,
        ServiceSet,
    )> {
        // initialize the databases
        let (reth_node, reth_db) = init_reth_db(reth_node)?;
        debug!("Reth DB initialized");
        let reth_node_adapter = IrysRethNodeAdapter::new(reth_node.clone().into()).await?;

        // initialize packing service early
        let packing_service =
            irys_actors::packing_service::PackingService::new(Arc::new(config.clone()));
        // start service senders/receivers with packing sender
        let (service_senders, receivers) = ServiceSenders::new();
        // attach the receiver loop and obtain a handle for waiters/tests
        let packing_handle = packing_service.attach_receiver_loop(
            runtime_handle.clone(),
            receivers.packing,
            service_senders.packing_sender.clone(),
        );

        // Initialize supply state for tracking cumulative emissions
        let supply_state = Arc::new(SupplyState::new(&config.node_config)?);
        let supply_state_guard = SupplyStateReadGuard::new(supply_state.clone());

        // start reth service
        let reth_service_task = init_reth_service(
            &irys_db,
            reth_node_adapter.clone(),
            service_senders.mempool.clone(),
            receivers.reth_service,
            runtime_handle.clone(),
        );
        debug!("Reth service initialized");
        // Get the correct Reth peer info
        let (peering_tx, peering_rx) = oneshot::channel();
        service_senders
            .reth_service
            .send_traced(RethServiceMessage::GetPeeringInfo {
                response: peering_tx,
            })
            .expect("Reth service channel should be open");
        let reth_peering = peering_rx
            .await
            .expect("Reth service to respond with peering info")?;

        // overwrite config as we now have reth peering information
        // TODO: Consider if starting the reth service should happen outside of init_services() instead of overwriting config here
        let mut node_config = config.node_config.clone();
        node_config.reth.network.peer_id = reth_peering.peer_id;
        node_config.reth.network.bind_ip = Some(reth_peering.peering_tcp_addr.ip().to_string());
        node_config.reth.network.bind_port = reth_peering.peering_tcp_addr.port();

        if node_config.reth.network.public_port == 0 {
            node_config.reth.network.public_port = reth_peering.peering_tcp_addr.port();
        }

        let config = Config::new(node_config, config.peer_id());

        let block_index_guard = BlockIndexReadGuard::new(block_index.clone());

        // Create cancellation token as child of the top-level shutdown token
        let backfill_cancel = shutdown_token.child_token();

        // Notify to signal backfill completion for clean shutdown
        let backfill_complete = Arc::new(tokio::sync::Notify::new());

        // Spawn async task to backfill historical supply state
        {
            let supply_state_for_backfill = supply_state.clone();
            let block_index_for_backfill = block_index_guard.clone();
            let db_for_backfill = irys_db.clone();
            let cancel_for_task = backfill_cancel.clone();
            let backfill_complete_signal = backfill_complete.clone();
            let backfill_handle = runtime_handle.spawn(async move {
                irys_actors::supply_state_calculator::backfill_supply_state(
                    supply_state_for_backfill,
                    block_index_for_backfill,
                    db_for_backfill,
                    cancel_for_task,
                )
                .await
            });

            // Monitor backfill task - panic the node on failure.
            // Backfill errors indicate data integrity issues (missing blocks, database corruption)
            // that require investigation and won't self-resolve.
            runtime_handle.spawn(async move {
                let result = backfill_handle.await;
                // Signal completion before potential panic to allow clean shutdown path
                backfill_complete_signal.notify_one();
                match result {
                    Ok(Ok(())) => {
                        tracing::info!("Supply state backfill completed successfully");
                    }
                    Ok(Err(e)) => {
                        panic!(
                            "FATAL: Supply state backfill failed: {}. \
                            This indicates data integrity issues that require investigation.",
                            e
                        );
                    }
                    Err(join_error) => {
                        panic!(
                            "FATAL: Supply state backfill task panicked: {}. \
                            This indicates data integrity issues that require investigation.",
                            join_error
                        );
                    }
                }
            });
        }

        // use the Tokio-native mining bus from ServiceSenders
        let mining_bus = service_senders.mining_bus();

        // start the epoch service
        let replay_data =
            EpochReplayData::query_replay_data(&irys_db, &block_index_guard, &config).await?;

        let storage_submodules_config =
            StorageSubmodulesConfig::load(config.node_config.base_directory.clone())?;

        let p2p_service = P2PService::new(
            config.node_config.miner_address(),
            config.peer_id(),
            receivers.gossip_broadcast,
        );
        let sync_state = p2p_service.sync_state.clone();

        // Restore the block tree cache (synchronous DB read during startup)
        let block_tree_cache = Arc::new(RwLock::new(BlockTree::restore_from_db(
            block_index_guard.clone(),
            replay_data,
            irys_db.clone(),
            &storage_submodules_config,
            config.clone(),
        )?));

        // Construct the block migration service
        let block_migration_service = BlockMigrationService::new(
            irys_db.clone(),
            block_index_guard.clone(),
            Some(supply_state.clone()),
            config.consensus.chunk_size,
            Arc::clone(&block_tree_cache),
            service_senders.chunk_migration.clone(),
        );

        // Start the block tree service
        let block_tree_handle = BlockTreeService::spawn_service(
            receivers.block_tree,
            irys_db.clone(),
            block_index_guard.clone(),
            &config,
            &service_senders,
            sync_state.clone(),
            block_migration_service,
            block_tree_cache,
            runtime_handle.clone(),
        );

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let block_tree_sender = service_senders.block_tree.clone();
        if let Err(e) =
            block_tree_sender.send_traced(BlockTreeServiceMessage::GetBlockTreeReadGuard {
                response: oneshot_tx,
            })
        {
            error!(
                "Failed to send GetBlockTreeReadGuard message to block tree service: {}",
                e
            );
        }
        let block_tree_guard = oneshot_rx
            .await
            .expect("to receive BlockTreeReadGuard response from GetBlockTreeReadGuard Message");

        let chunk_cache_handle = ChunkCacheService::spawn_service(
            block_index_guard.clone(),
            block_tree_guard.clone(),
            irys_db.clone(),
            receivers.chunk_cache,
            config.clone(),
            service_senders.gossip_broadcast.clone(),
            service_senders.chunk_cache.clone(),
            runtime_handle.clone(),
        );
        debug!("Chunk cache initialized");

        let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
        let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();

        let storage_modules = Self::init_storage_modules(&config, storage_module_infos)?;
        let storage_modules_guard = StorageModulesReadGuard::new(storage_modules.clone());

        // Spawn peer list service
        let (peer_network_handle, peer_list_guard) = init_peer_list_service(
            &irys_db,
            &config,
            service_senders.reth_service.clone(),
            receivers.peer_network,
            service_senders.peer_network.clone(),
            service_senders.peer_events.clone(),
            runtime_handle.clone(),
        );

        let execution_payload_cache =
            ExecutionPayloadCache::new(peer_list_guard.clone(), reth_node_adapter.clone().into());

        // Spawn chunk ingress service
        let (chunk_ingress_handle, chunk_ingress_state) =
            irys_actors::chunk_ingress_service::ChunkIngressService::spawn_service(
                irys_db.clone(),
                storage_modules_guard.clone(),
                &block_tree_guard,
                receivers.chunk_ingress,
                &config,
                &service_senders,
                runtime_handle.clone(),
                task_exec.clone(),
            );

        // Spawn mempool service
        let mempool_handle = MempoolService::spawn_service(
            irys_db.clone(),
            reth_node_adapter.clone(),
            &block_tree_guard,
            receivers.mempool,
            &config,
            &service_senders,
            runtime_handle.clone(),
            chunk_ingress_state.clone(),
        )?;
        let mempool_facade = MempoolServiceFacadeImpl::from(&service_senders);

        // Get the mempool state to create the pledge provider
        let (tx, rx) = oneshot::channel();
        service_senders
            .mempool
            .send_traced(irys_actors::mempool_service::MempoolServiceMessage::GetState(tx))
            .map_err(|_| eyre::eyre!("Failed to send GetState message to mempool service"))?;

        let mempool_state = rx
            .await
            .map_err(|_| eyre::eyre!("Failed to receive mempool state from mempool service"))?;

        // Create the MempoolReadGuard for block discovery service
        let mempool_guard = MempoolReadGuard::new(mempool_state.clone());

        // Create the MempoolPledgeProvider
        let mempool_pledge_provider = Arc::new(MempoolPledgeProvider::new(
            mempool_state,
            block_tree_guard.clone(),
        ));

        // spawn the chunk migration service
        let chunk_migration_handle = ChunkMigrationService::spawn_service(
            receivers.chunk_migration,
            block_index.clone(),
            &storage_modules_guard,
            irys_db.clone(),
            service_senders.clone(),
            &config,
            runtime_handle.clone(),
        );

        let is_vdf_mining_enabled = Arc::new(AtomicBool::new(false));
        // Spawn VDF service
        let vdf_state = Arc::new(RwLock::new(irys_vdf::state::create_state(
            block_index.clone(),
            irys_db.clone(),
            Arc::clone(&is_vdf_mining_enabled),
            &config,
        )));
        let vdf_state_readonly = VdfStateReadonly::new(Arc::clone(&vdf_state));

        // Spawn the validation service
        let (validation_handle, validation_enabled) = ValidationService::spawn_service(
            block_index_guard.clone(),
            block_tree_guard.clone(),
            mempool_guard.clone(),
            vdf_state_readonly.clone(),
            &config,
            &service_senders,
            reth_node_adapter.clone(),
            irys_db.clone(),
            execution_payload_cache.clone(),
            receivers.validation_service,
            runtime_handle.clone(),
            sync_state.clone(),
        );

        // create the block reward curve
        let reward_curve = irys_reward_curve::HalvingCurve {
            inflation_cap: config.consensus.block_reward_config.inflation_cap,
            half_life_secs: config.consensus.block_reward_config.half_life_secs.into(),
        };
        let reward_curve = Arc::new(reward_curve);

        // spawn block discovery
        let block_discovery_handle = Self::init_block_discovery_service(
            &config,
            &irys_db,
            &service_senders,
            &block_index_guard,
            &block_tree_guard,
            &mempool_guard,
            &vdf_state_readonly,
            Arc::clone(&reward_curve),
            receivers.block_discovery,
            runtime_handle.clone(),
        );

        let block_discovery_facade =
            BlockDiscoveryFacadeImpl::new(service_senders.block_discovery.clone());

        let block_status_provider =
            BlockStatusProvider::new(block_index_guard.clone(), block_tree_guard.clone());

        // In case if you're wondering why this channel is not in the service senders:
        // It's because ChainSyncService depends on the BlockPool, and moving it to actors will
        // create a circular dependency, since BlockPool also depends on actors. This can be
        // resolved once all actors are converted to tokio services, and BlockPool is moved into
        // domain
        let (chain_sync_tx, chain_sync_rx) = mpsc::unbounded_channel();
        let (
            gossip_server,
            gossip_server_handle,
            broadcast_task_handle,
            block_pool,
            gossip_data_handler,
        ) = p2p_service.run(
            mempool_facade,
            block_discovery_facade.clone(),
            task_exec,
            peer_list_guard.clone(),
            irys_db.clone(),
            gossip_listener,
            block_status_provider.clone(),
            execution_payload_cache,
            config.clone(),
            service_senders.clone(),
            chain_sync_tx.clone(),
            mempool_guard.clone(),
            block_index_guard.clone(),
            block_tree_guard.clone(),
            std::time::Instant::now(),
        )?;

        // set up the price oracles (initial price(s) fetched during construction)
        let (price_oracle, price_oracle_handles) =
            Self::init_price_oracle(&config, &runtime_handle).await;

        // set up the block producer
        let (block_producer_inner, block_producer_handle) = Self::init_block_producer(
            &config,
            Arc::clone(&reward_curve),
            &irys_db,
            &service_senders,
            &block_tree_guard,
            &mempool_guard,
            &vdf_state_readonly,
            block_discovery_facade,
            mining_bus.clone(),
            price_oracle,
            reth_node_adapter.clone(),
            receivers.block_producer,
            reth_node.provider.clone(),
            block_index,
            runtime_handle.clone(),
        );

        let (global_step_number, last_step_hash) =
            vdf_state_readonly.read().get_last_step_and_seed();
        let initial_hash = last_step_hash.0;
        metrics::record_vdf_global_step(global_step_number);

        // spawn packing controllers and set global step number
        let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));
        let packing_controller_handles =
            packing_service.spawn_packing_controllers(runtime_handle.clone());

        // set up partition mining services (tokio)
        let (partition_controllers, partition_handles) = Self::init_partition_mining_services(
            &config,
            &storage_modules_guard,
            &vdf_state_readonly,
            &service_senders,
            &atomic_global_step_number,
            latest_block.diff,
            runtime_handle.clone(),
        );

        // set up the vdf thread
        let vdf_done_rx = Self::init_vdf_thread(
            &config,
            receivers.vdf_fast_forward,
            Arc::clone(&is_vdf_mining_enabled),
            latest_block,
            initial_hash,
            global_step_number,
            mining_bus.clone(),
            vdf_state,
            atomic_global_step_number,
            block_status_provider,
            sync_state.clone(),
            shutdown_token.clone(),
        );

        // set up chunk provider
        let chunk_provider = Self::init_chunk_provider(&config, storage_modules_guard.clone());

        // set up sync service
        let (sync_service_facade, sync_service_handle) = Self::init_sync_service(
            sync_state.clone(),
            peer_list_guard.clone(),
            config.clone(),
            block_index_guard.clone(),
            runtime_handle.clone(),
            Arc::clone(&block_pool),
            Arc::clone(&gossip_data_handler),
            (chain_sync_tx, chain_sync_rx),
            service_senders.reth_service.clone(),
            Arc::clone(&is_vdf_mining_enabled),
        );

        // set up initial FCU states on reth
        let fcu_markers = {
            let block_index = block_index_guard.read();
            ForkChoiceMarkers::from_index(
                block_index,
                &irys_db,
                config.consensus.block_migration_depth as usize,
                config.consensus.block_tree_depth as usize,
            )?
        };

        let (fcu_tx, fcu_rx) = oneshot::channel();
        service_senders
            .reth_service
            .send_traced(RethServiceMessage::ForkChoice {
                update: ForkChoiceUpdateMessage {
                    head_hash: fcu_markers.head.block_hash,
                    confirmed_hash: fcu_markers.migration_block.block_hash,
                    finalized_hash: fcu_markers.prune_block.block_hash,
                },
                response: fcu_tx,
            })
            .map_err(|err| eyre::eyre!("failed to enqueue initial FCU for reth service: {err}"))?;
        fcu_rx
            .await
            .map_err(|err| eyre::eyre!("reth service dropped initial FCU acknowledgment: {err}"))?;

        debug!(
            fcu.head = %fcu_markers.head.block_hash,
            fcu.confirmed = %fcu_markers.migration_block.block_hash,
            fcu.finalized = %fcu_markers.prune_block.block_hash,
            "Initial fork choice update applied to Reth"
        );
        irys_actors::record_reth_fcu_head_height(fcu_markers.head.height);

        // set up IrysNodeCtx
        let irys_node_ctx = IrysNodeCtx {
            reward_curve,
            reth_handle: reth_node.clone(),
            reth_db,
            db: irys_db.clone(),
            genesis_hash,
            chunk_provider: chunk_provider.clone(),
            block_index_guard: block_index_guard.clone(),
            vdf_steps_guard: vdf_state_readonly,
            mempool_guard: mempool_guard.clone(),
            service_senders: service_senders.clone(),
            partition_controllers,
            packing_waiter: packing_handle.waiter(),
            shutdown_sender: tokio::sync::mpsc::channel::<ShutdownReason>(1).0,
            lifecycle_handle: Arc::new(std::sync::Mutex::new(None)),
            tokio_runtime: None,
            shutdown_token: shutdown_token.clone(),
            block_tree_guard: block_tree_guard.clone(),
            config: config.clone(),
            stop_guard: StopGuard::new(),
            peer_list: peer_list_guard.clone(),
            sync_state: sync_state.clone(),
            reth_node_adapter,
            block_producer_inner,
            block_pool,
            gossip_data_handler,
            validation_enabled,
            storage_modules_guard,
            mempool_pledge_provider: mempool_pledge_provider.clone(),
            sync_service_facade,
            is_vdf_mining_enabled,
            started_at: Instant::now(),
            supply_state_guard: Some(supply_state_guard.clone()),
            chunk_ingress_state,
            backfill_complete,
        };

        // Spawn the StorageModuleService to manage the life-cycle of storage modules
        // This service:
        // - Monitors partition assignments from the network
        // - Initializes storage modules when they receive partition assignments
        // - Handles the dynamic addition/removal of storage modules
        // - Coordinates with the epoch service for runtime updates
        debug!("Starting StorageModuleService");
        let storage_module_handle = StorageModuleService::spawn_service(
            receivers.storage_modules,
            storage_modules.clone(),
            block_index_guard.clone(),
            block_tree_guard.clone(),
            service_senders.clone(),
            &config,
            runtime_handle.clone(),
        );

        // Production chunk fetcher is the HTTP chunk fetcher
        let http_factory: ChunkFetcherFactory =
            Box::new(|ledger_id| Arc::new(HttpChunkFetcher::new(ledger_id)));

        let data_sync_handle = DataSyncService::spawn_service(
            receivers.data_sync,
            block_tree_guard.clone(),
            storage_modules.clone(),
            peer_list_guard.clone(),
            http_factory,
            &service_senders,
            &config,
            runtime_handle.clone(),
        );

        let mut services = Vec::new();
        {
            // Services are shut down in FIFO order (first added = first to shut down)
            // 1. Mining operations
            services.extend(price_oracle_handles.into_iter());
            services.extend(partition_handles.into_iter());
            // Add packing controllers to services
            services.extend(packing_controller_handles.into_iter());

            // 2. Block production flow
            services.push(block_producer_handle);
            services.push(block_discovery_handle);

            // 3. Validation
            services.push(validation_handle);

            // 4. Storage operations
            services.push(chunk_cache_handle);
            services.push(storage_module_handle);
            services.push(data_sync_handle);
            services.push(chunk_migration_handle);

            // 5. Sync operations
            services.push(sync_service_handle);

            // 6. Chain management
            services.push(block_tree_handle);

            // 7. State management
            services.push(mempool_handle);
            services.push(chunk_ingress_handle);

            // 8. Core infrastructure (shutdown last)
            services.push(peer_network_handle);
            services.push(reth_service_task);
        }

        let server = run_server(
            ApiState {
                mempool_service: service_senders.mempool.clone(),
                chunk_ingress: service_senders.chunk_ingress.clone(),
                mempool_guard: mempool_guard.clone(),
                chunk_provider: chunk_provider.clone(),
                peer_list: peer_list_guard,
                db: irys_db,
                reth_provider: reth_node.clone(),
                block_tree: block_tree_guard.clone(),
                block_index: block_index_guard.clone(),
                supply_state: Some(supply_state_guard),
                config: config.clone(),
                reth_http_url: reth_node
                    .rpc_server_handle()
                    .http_url()
                    .expect("Missing reth rpc url!"),
                sync_state,
                mempool_pledge_provider,
                started_at: irys_node_ctx.started_at,
                mining_address: irys_node_ctx.config.node_config.miner_address(),
            },
            http_listener,
        );

        let p2p_service_handle = irys_p2p::spawn_p2p_server_watcher_task(
            gossip_server,
            gossip_server_handle,
            broadcast_task_handle,
            task_exec,
        );

        // this OnceLock is due to the cyclic chain between Reth & the Irys node, where the IrysRethProvider requires both
        // this is "safe", as the OnceLock is always set before this start function returns
        let mut w = irys_provider
            .write()
            .map_err(|_| eyre::eyre!("lock poisoned"))?;
        *w = Some(IrysRethProviderInner { chunk_provider });

        Ok((
            irys_node_ctx,
            server,
            vdf_done_rx,
            p2p_service_handle,
            ServiceSet::new(services),
        ))
    }

    fn init_chunk_provider(
        config: &Config,
        storage_modules_guard: StorageModulesReadGuard,
    ) -> Arc<ChunkProvider> {
        let chunk_provider = ChunkProvider::new(config.clone(), storage_modules_guard);

        Arc::new(chunk_provider)
    }

    #[expect(clippy::path_ends_with_ext, reason = "Core pinning logic")]
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %latest_block.block_hash, block.height = %latest_block.height, custom.global_step_number = global_step_number))]
    fn init_vdf_thread(
        config: &Config,
        vdf_fast_forward_receiver: UnboundedReceiver<Traced<VdfStep>>,
        is_vdf_mining_enabled: Arc<AtomicBool>,
        latest_block: Arc<IrysBlockHeader>,
        initial_hash: H256,
        global_step_number: u64,
        mining_bus: MiningBus,
        vdf_state: AtomicVdfState,
        atomic_global_step_number: Arc<AtomicU64>,
        block_status_provider: BlockStatusProvider,
        chain_sync_state: ChainSyncState,
        shutdown_token: CancellationToken,
    ) -> oneshot::Receiver<()> {
        let next_canonical_vdf_seed = latest_block.vdf_limiter_info.next_seed;
        // FIXME: this should be controlled via a config parameter rather than relying on test-only artifact generation
        // we can't use `cfg!(test)` to detect integration tests, so we check that the path is of form `(...)/.tmp/<random folder>`
        let is_test_based_on_base_dir = config
            .node_config
            .base_directory
            .parent()
            .is_some_and(|p| p.ends_with(".tmp"));
        let is_test_based_on_cfg_flag = cfg!(test);
        if is_test_based_on_cfg_flag && !is_test_based_on_base_dir {
            panic!("VDF core pinning: cfg!(test) is true but the base_dir .tmp check is false - please make sure you are using a temporary directory for testing (This is because integration tests are not considered 'tests', and so the only way we can detect them to disable core pinning is using the base directory test are run from.)")
        }
        let span = tracing::Span::current();
        let (vdf_done_tx, vdf_done_rx) = oneshot::channel::<()>();

        std::thread::spawn({
            let vdf_config = config.vdf.clone();
            move || {
                let _span = span.enter();

                // Setup core affinity in prod only (perf gain shouldn't matter for tests, and we don't want pinning overlap)
                if is_test_based_on_base_dir || is_test_based_on_cfg_flag {
                    info!("Disabling VDF core pinning")
                } else {
                    let core_ids = core_affinity::get_core_ids().expect("Failed to get core IDs");

                    for core in core_ids {
                        let success = core_affinity::set_for_current(core);
                        if success {
                            info!("VDF thread pinned to core {:?}", core);
                            break;
                        }
                    }
                }

                run_vdf(
                    &vdf_config,
                    global_step_number,
                    initial_hash,
                    next_canonical_vdf_seed,
                    vdf_fast_forward_receiver,
                    is_vdf_mining_enabled,
                    MiningBusBroadcaster::from(mining_bus.clone()),
                    vdf_state.clone(),
                    atomic_global_step_number.clone(),
                    block_status_provider,
                    chain_sync_state,
                    shutdown_token,
                );
                let _ = vdf_done_tx.send(());
            }
        });
        vdf_done_rx
    }

    fn init_partition_mining_services(
        config: &Config,
        storage_modules_guard: &StorageModulesReadGuard,
        vdf_steps_guard: &VdfStateReadonly,
        service_senders: &ServiceSenders,
        atomic_global_step_number: &Arc<AtomicU64>,
        initial_difficulty: U256,
        runtime_handle: tokio::runtime::Handle,
    ) -> (Vec<PartitionMiningController>, Vec<TokioServiceHandle>) {
        let mut controllers = Vec::new();
        let mut handles = Vec::new();
        for sm in storage_modules_guard.read().iter() {
            let inner = PartitionMiningServiceInner::new(
                config,
                service_senders.clone(),
                sm.clone(),
                false, // do not start mining automatically
                vdf_steps_guard.clone(),
                atomic_global_step_number.clone(),
                initial_difficulty,
            );
            let (controller, handle) =
                PartitionMiningService::spawn_service(inner, runtime_handle.clone());
            controllers.push(controller);
            handles.push(handle);
        }

        // request packing for uninitialized ranges of assigned storage modules
        for sm in storage_modules_guard
            .read()
            .iter()
            .filter(|sm| sm.partition_assignment().is_some())
        {
            let uninitialized = sm.get_intervals(ChunkType::Uninitialized);
            let sender = service_senders.packing_sender();
            let sm = sm.clone();

            debug!(
                "SM {} has {} Uninitialized intervals",
                &sm.id,
                &uninitialized.len()
            );
            // spawn a thread per SM (as packing queues are per-partition)
            // this is to prevent packing requests getting dropped
            // note: once we support submodules, this will probably need to change
            runtime_handle.spawn(async move {
                for interval in uninitialized {
                    if let Ok(req) = PackingRequest::new(sm.clone(), PartitionChunkRange(interval))
                    {
                        if let Err(e) = sender.send(req).await {
                            tracing::event!(
                                target: "irys::packing",
                                tracing::Level::ERROR,
                                storage_module.id = %sm.id,
                                packing.interval = ?interval,
                                "Packing channel closed - {e}; failed to enqueue repacking request"
                            );
                            return; // assume this is unrecoverable
                        }
                    }
                }
            });
        }
        (controllers, handles)
    }

    fn init_block_producer(
        config: &Config,
        reward_curve: Arc<HalvingCurve>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_tree_guard: &BlockTreeReadGuard,
        mempool_guard: &MempoolReadGuard,
        vdf_steps_guard: &VdfStateReadonly,
        block_discovery: BlockDiscoveryFacadeImpl,
        mining_bus: MiningBus,
        price_oracle: Arc<IrysPriceOracle>,
        reth_node_adapter: IrysRethNodeAdapter,
        block_producer_rx: UnboundedReceiver<Traced<BlockProducerCommand>>,
        reth_provider: NodeProvider,
        block_index: BlockIndex,
        runtime_handle: tokio::runtime::Handle,
    ) -> (Arc<irys_actors::BlockProducerInner>, TokioServiceHandle) {
        let block_producer_inner = Arc::new(irys_actors::BlockProducerInner {
            db: irys_db.clone(),
            config: config.clone(),
            reward_curve,
            mining_broadcaster: mining_bus,
            block_discovery,
            vdf_steps_guard: vdf_steps_guard.clone(),
            block_tree_guard: block_tree_guard.clone(),
            mempool_guard: mempool_guard.clone(),
            price_oracle,
            service_senders: service_senders.clone(),
            reth_payload_builder: reth_node_adapter.inner.payload_builder_handle.clone(),
            reth_provider,
            consensus_engine_handle: reth_node_adapter.inner.beacon_engine_handle.clone(),
            block_index,
        });

        // Spawn the service and get the handle
        let tokio_service_handle = irys_actors::BlockProducerService::spawn_service(
            block_producer_inner.clone(),
            None, // blocks_remaining_for_test
            block_producer_rx,
            runtime_handle,
        );

        (block_producer_inner, tokio_service_handle)
    }

    async fn init_price_oracle(
        config: &Config,
        runtime_handle: &tokio::runtime::Handle,
    ) -> (Arc<IrysPriceOracle>, Vec<irys_types::TokioServiceHandle>) {
        // Use configured oracles (must be provided in config)
        let oracle_cfgs: Vec<OracleConfig> = config.node_config.oracles.clone();

        let mut instances: Vec<Arc<SingleOracle>> = Vec::new();
        let mut handles: Vec<irys_types::TokioServiceHandle> = Vec::new();

        for oc in oracle_cfgs {
            let oracle = match oc {
                OracleConfig::Mock {
                    initial_price,
                    incremental_change,
                    smoothing_interval,
                    initial_direction_up,
                    poll_interval_ms,
                } => SingleOracle::new_mock(
                    initial_price,
                    incremental_change,
                    smoothing_interval,
                    initial_direction_up,
                    poll_interval_ms,
                ),
                OracleConfig::CoinMarketCap {
                    api_key,
                    id,
                    poll_interval_ms,
                } => SingleOracle::new_coinmarketcap(api_key, id, poll_interval_ms)
                    .await
                    .expect("coinmarketcap initial price"),
                OracleConfig::CoinGecko {
                    api_key,
                    coin_id,
                    demo_api_key,
                    poll_interval_ms,
                } => SingleOracle::new_coingecko(api_key, coin_id, demo_api_key, poll_interval_ms)
                    .await
                    .expect("coingecko initial price"),
            };
            let handle = Arc::clone(&oracle).spawn_poller(runtime_handle);
            handles.push(handle);
            instances.push(oracle);
        }

        (IrysPriceOracle::new(instances), handles)
    }

    fn init_block_discovery_service(
        config: &Config,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_index_guard: &BlockIndexReadGuard,
        block_tree_guard: &BlockTreeReadGuard,
        mempool_guard: &MempoolReadGuard,
        vdf_steps_guard: &VdfStateReadonly,
        reward_curve: Arc<HalvingCurve>,
        block_discovery_rx: UnboundedReceiver<Traced<BlockDiscoveryMessage>>,
        runtime_handle: Handle,
    ) -> TokioServiceHandle {
        let block_discovery_inner = BlockDiscoveryServiceInner {
            block_index_guard: block_index_guard.clone(),
            block_tree_guard: block_tree_guard.clone(),
            mempool_guard: mempool_guard.clone(),
            db: irys_db.clone(),
            config: config.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            service_senders: service_senders.clone(),
            reward_curve,
        };
        BlockDiscoveryService::spawn_service(
            Arc::new(block_discovery_inner),
            block_discovery_rx,
            runtime_handle,
        )
    }

    fn init_storage_modules(
        config: &Config,
        storage_module_infos: Vec<StorageModuleInfo>,
    ) -> eyre::Result<Arc<RwLock<Vec<Arc<StorageModule>>>>> {
        let mut storage_modules = Vec::new();
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(&info, config)?);
            storage_modules.push(arc_module.clone());
        }

        Ok(Arc::new(RwLock::new(storage_modules)))
    }

    fn init_sync_service(
        sync_state: ChainSyncState,
        peer_list: PeerList,
        config: Config,
        block_index_guard: BlockIndexReadGuard,
        runtime_handle: tokio::runtime::Handle,
        block_pool: Arc<BlockPool<BlockDiscoveryFacadeImpl, MempoolServiceFacadeImpl>>,
        gossip_data_handler: Arc<
            GossipDataHandler<MempoolServiceFacadeImpl, BlockDiscoveryFacadeImpl>,
        >,
        (tx, rx): (
            UnboundedSender<SyncChainServiceMessage>,
            UnboundedReceiver<SyncChainServiceMessage>,
        ),
        reth_service: UnboundedSender<Traced<RethServiceMessage>>,
        is_vdf_mining_enabled: Arc<AtomicBool>,
    ) -> (SyncChainServiceFacade, TokioServiceHandle) {
        let facade = SyncChainServiceFacade::new(tx);

        let inner = ChainSyncServiceInner::new(
            sync_state,
            peer_list,
            config,
            block_index_guard,
            block_pool,
            gossip_data_handler,
            Some(reth_service),
            is_vdf_mining_enabled,
        );

        let handle = ChainSyncService::spawn_service(inner, rx, runtime_handle);

        (facade, handle)
    }
}

fn read_latest_block_data(
    block_index: &BlockIndex,
    irys_db: &DatabaseProvider,
) -> (u64, Arc<IrysBlockHeader>) {
    // Read latest from the block index; if no entries, panic
    let latest_block_index = block_index
        .get_latest_item()
        .expect("block index must have at least one entry");
    let latest_block_height = block_index.latest_height();
    let latest_block = Arc::new(
        database::block_header_by_hash(
            &irys_db.tx().unwrap(),
            &latest_block_index.block_hash,
            false,
        )
        .unwrap()
        .unwrap(),
    );
    (latest_block_height, latest_block)
}

fn init_peer_list_service(
    irys_db: &DatabaseProvider,
    config: &Config,
    reth_service: UnboundedSender<Traced<RethServiceMessage>>,
    service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
    service_sender: PeerNetworkSender,
    peer_events: tokio::sync::broadcast::Sender<irys_domain::PeerEvent>,
    runtime_handle: Handle,
) -> (TokioServiceHandle, PeerList) {
    let reth_peer_sender = {
        let reth_service = reth_service;
        Arc::new(move |reth_peer_info: RethPeerInfo| {
            let reth_service = reth_service.clone();
            async move {
                let (response_tx, response_rx) = oneshot::channel();

                if let Err(send_error) = reth_service.send_traced(RethServiceMessage::ConnectToPeer {
                    peer: reth_peer_info,
                    response: response_tx,
                }) {
                    error!(
                        custom.error = %send_error,
                        "Failed to enqueue connect-to-peer request for reth service"
                    );
                    return;
                }

                match response_rx.await {
                    Ok(Ok(())) => {
                        debug!("Successfully connected to reth peer");
                    }
                    Ok(Err(err)) => {
                        error!(custom.error = %err, "Reth service failed to connect to peer");
                    }
                    Err(recv_error) => {
                        error!(custom.error = %recv_error, "Reth service connect-to-peer response channel closed");
                    }
                }
            }
            .boxed()
        })
    };

    spawn_peer_network_service(
        irys_db.clone(),
        config,
        reth_peer_sender,
        service_receiver,
        service_sender,
        peer_events,
        runtime_handle,
    )
}

fn init_reth_service(
    irys_db: &DatabaseProvider,
    reth_node_adapter: IrysRethNodeAdapter,
    mempool_sender: UnboundedSender<Traced<MempoolServiceMessage>>,
    reth_rx: UnboundedReceiver<Traced<RethServiceMessage>>,
    runtime_handle: tokio::runtime::Handle,
) -> TokioServiceHandle {
    irys_actors::reth_service::RethService::spawn_service(
        reth_node_adapter,
        irys_db.clone(),
        mempool_sender,
        reth_rx,
        runtime_handle,
    )
}

fn init_reth_db(
    reth_node: RethNode,
) -> Result<(RethNodeProvider, irys_database::db::RethDbWrapper), eyre::Error> {
    let reth_node = RethNodeProvider(Arc::new(reth_node));
    let reth_db = reth_node.provider.database.db.clone();
    Ok((reth_node, reth_db))
}

#[tracing::instrument(level = "trace", skip_all)]
fn init_irys_db(node_config: &NodeConfig) -> Result<DatabaseProvider, eyre::Error> {
    let irys_db_env =
        open_or_create_irys_consensus_data_db(&node_config.irys_consensus_data_dir())?;
    let irys_db = DatabaseProvider(Arc::new(irys_db_env));
    debug!("Irys DB initialized");
    Ok(irys_db)
}

/// Gets the peer_id from the peer key file, or generates a new keypair and stores it.
///
/// The private key is stored as raw 32 bytes in `<peer_info_dir>/peer_key.bin`.
/// The PeerId is derived from the key using standard secp256k1 address derivation.
pub fn get_or_create_peer_id(node_config: &NodeConfig) -> eyre::Result<irys_types::IrysPeerId> {
    let peer_info_dir = node_config.peer_info_dir();
    let key_path = peer_info_dir.join("peer_key.bin");

    let signing_key = match std::fs::read(&key_path) {
        Ok(bytes) => {
            let key = k256::ecdsa::SigningKey::from_slice(&bytes)
                .with_context(|| "Failed to parse peer key file as secp256k1 private key")?;
            let peer_id =
                irys_types::IrysPeerId::from(irys_types::IrysAddress::from_private_key(&key));
            info!("Loaded peer_id from {}: {:?}", key_path.display(), peer_id);
            key
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use rand::rngs::OsRng;
            let key = k256::ecdsa::SigningKey::random(&mut OsRng);
            std::fs::create_dir_all(&peer_info_dir).with_context(|| {
                format!(
                    "Failed to create peer info directory {}",
                    peer_info_dir.display()
                )
            })?;
            std::fs::write(&key_path, key.to_bytes().as_slice())
                .with_context(|| format!("Failed to write peer key to {}", key_path.display()))?;
            let peer_id =
                irys_types::IrysPeerId::from(irys_types::IrysAddress::from_private_key(&key));
            info!(
                "Generated new peer_id, key saved to {}: {:?}",
                key_path.display(),
                peer_id
            );
            key
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to read peer key from {}", key_path.display()));
        }
    };

    let peer_id =
        irys_types::IrysPeerId::from(irys_types::IrysAddress::from_private_key(&signing_key));
    Ok(peer_id)
}

/// This function is used by the node to automatically stake & pledge on startup, if the `node_config.stake_pledge_drives` option is `true`
/// method:
/// 1) check if we have an existing stake - historic (already in an epoch) or pending (ready to be rolled up at the end of the current epoch)
/// 2) if we have no existing stake, submit a stake to the local mempool
/// 3) check all local storage modules for partition assignments against all known partition pledges - historic or pending
/// 4) post enough pledges so that there are enough pledges for all local storage modules
#[instrument(skip_all)]
async fn stake_and_pledge(
    config: &Config,
    block_tree_guard: BlockTreeReadGuard,
    storage_modules_guard: StorageModulesReadGuard,
    latest_block_hash: BlockHash,
    mempool_pledge_provider: Arc<MempoolPledgeProvider>,
) -> eyre::Result<()> {
    // get all SMs with and without a partition assignment
    let (assigned_modules, unassigned_modules): (Vec<Arc<StorageModule>>, Vec<Arc<StorageModule>>) = {
        let sms = storage_modules_guard.read();
        sms.iter()
            .cloned()
            .partition(|sm| sm.partition_assignment().is_some())
    };

    if unassigned_modules.is_empty() {
        debug!("No unassigned modules locally, skipping...");
        return Ok(());
    }

    debug!("Checking Stake & Pledge status");
    // NOTE: this assumes we're caught up with the chain
    // primarily for the anchor used for the produced txs
    // if we aren't caught up, the txs will be rejected
    let signer = config.irys_signer();
    let address = signer.address();

    let api_uri = config.node_config.local_api_url();

    let post_commitment_tx = async |commitment_tx: &CommitmentTransaction| {
        let client = reqwest::Client::new();
        let url = format!("{}/v1/commitment-tx", api_uri);

        client.post(url).json(commitment_tx).send().await
    };

    // now check the canonical state

    let (is_historically_staked, epoch_snapshot, commitment_snapshot) = {
        let block_tree_guard = block_tree_guard.read();
        let epoch_snapshot = block_tree_guard.canonical_epoch_snapshot();
        let is_historically_staked = epoch_snapshot.is_staked(address);
        let commitment_snapshot = (*block_tree_guard.canonical_commitment_snapshot()).clone();
        (is_historically_staked, epoch_snapshot, commitment_snapshot)
    };

    // check the commitment snapshot (pending commitment txs for the next epoch rollup)
    // to see if we have pending stakes/commitments already
    // if we do, don't submit any more than we need to (i.e if we have added some extra drives etc)
    let pending_commitments = commitment_snapshot.commitments.get(&address);
    let has_pending_stake =
        pending_commitments.is_some_and(|commitments| commitments.stake.is_some());

    let mut total_cost = U256::zero();

    // if we have a historic or pending stake, don't send another
    let is_staked = is_historically_staked || has_pending_stake;
    if !is_staked {
        debug!(
            "Local mining address {:?} is not staked, staking...",
            &address
        );

        // post a stake tx
        let mut stake_tx = CommitmentTransaction::new_stake(&config.consensus, latest_block_hash);
        signer.sign_commitment(&mut stake_tx)?;

        total_cost += stake_tx.total_cost();

        post_commitment_tx(&stake_tx).await.unwrap();
        debug!(
            "Posted stake tx {:?} (value: {}, fee: {})",
            &stake_tx.id(),
            &stake_tx.value(),
            &stake_tx.fee()
        );
        stake_tx.id()
    } else {
        debug!("Local mining address {:?} is staked", &address);
        // latest_block.previous_block_hash
        latest_block_hash
    };

    // get the number of pending & historic commitment txs for partitions, if the count is >= the unassigned len, do nothing
    let pending_pledge_count = pending_commitments.map(|pc| pc.pledges.len()).unwrap_or(0);
    let historic_pledge_count = epoch_snapshot.get_partition_assignments(address).len();
    let to_pledge_count = unassigned_modules
        .len()
        .saturating_sub(pending_pledge_count);

    debug!(
        "Found {} SMs without partition assignments ({} pending pledges, {} historic, {} assigned SMs) - sending {} pledges",
        &unassigned_modules.len(), &pending_pledge_count, &historic_pledge_count, &assigned_modules.len(), &to_pledge_count
    );

    ensure!(historic_pledge_count == assigned_modules.len(), "Historic pledge count ({}) and assigned module count ({}) are different! this indicates an issue with storage module partition assignment logic!\nDEBUG\n historic_pledges {:?}, assigned_modules: {:?}, unassigned modules: {:?}",
&historic_pledge_count, assigned_modules.len(), epoch_snapshot.get_partition_assignments(address), assigned_modules, unassigned_modules  );

    for idx in 0..to_pledge_count {
        // post a pledge tx
        let mut pledge_tx = CommitmentTransaction::new_pledge(
            &config.consensus,
            latest_block_hash,
            mempool_pledge_provider.as_ref(),
            address,
        )
        .await;

        signer.sign_commitment(&mut pledge_tx)?;

        post_commitment_tx(&pledge_tx).await.unwrap();
        total_cost += pledge_tx.total_cost();
        metrics::record_pledge_tx_posted();

        debug!(
            "Posted pledge tx {}/{} {:?} (value: {}, fee {})",
            idx + 1,
            to_pledge_count,
            &pledge_tx.id(),
            &pledge_tx.value(),
            &pledge_tx.fee()
        );
    }
    debug!(
        "Stake & Pledge check complete - total cost: {}",
        &total_cost
    );

    Ok(())
}
