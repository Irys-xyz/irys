use crate::block_pool::{BlockRemovalReason, FailureReason};
use crate::gossip_data_handler::GossipDataHandler;
use crate::{BlockPool, GossipClient, GossipError, GossipResult};
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::reth_service::RethServiceMessage;
use irys_actors::MempoolFacade;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{BlockIndexReadGuard, PeerList};
use irys_types::Traced;
use irys_types::{
    BlockHash, BlockIndexItem, BlockIndexQuery, Config, EvmBlockHash, IrysPeerId, NodeMode,
    PeerListItem, SyncMode, TokioServiceHandle, U256,
};
use rand::prelude::SliceRandom as _;
use reth::tasks::shutdown::Shutdown;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    sync::{mpsc, oneshot},
    time::{interval, timeout},
};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

/// Grace period after triggering a retry block request, giving validation time to start
/// before the sync loop continues.
const RETRY_BLOCK_VALIDATION_GRACE: Duration = Duration::from_millis(500);

/// Number of retry attempts when fetching a block index from peers.
const BLOCK_INDEX_RETRIES: usize = 5;

/// Fewer retries for the best-effort block index fetch inside queue-slot recovery.
const QUEUE_SLOT_RETRY_BLOCK_INDEX_RETRIES: usize = 3;

/// Sync service errors
#[derive(Debug, thiserror::Error)]
pub enum ChainSyncError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("Service communication error: {0}")]
    ServiceCommunication(String),
    #[error("Internal sync error: {0}")]
    Internal(String),
}

/// Type alias for sync service results
pub type ChainSyncResult<T> = Result<T, ChainSyncError>;

impl From<GossipError> for ChainSyncError {
    fn from(err: GossipError) -> Self {
        match err {
            GossipError::Advisory(err) => Self::Internal(format!("Advisory error: {}", &err)),
            GossipError::Network(msg) => Self::Network(msg),
            GossipError::CircuitBreakerOpen(peer_id) => {
                Self::Network(format!("Circuit breaker open for peer {}", peer_id))
            }
            GossipError::InvalidPeer(msg) => Self::Network(format!("Invalid peer: {}", msg)),
            GossipError::Cache(msg) => Self::Internal(format!("Cache error: {}", msg)),
            GossipError::Internal(internal_err) => {
                Self::Internal(format!("Internal gossip error: {}", internal_err))
            }
            GossipError::InvalidData(data_err) => {
                Self::Network(format!("Invalid data: {}", data_err))
            }
            GossipError::BlockPool(pool_err) => {
                Self::Internal(format!("Block pool error: {:?}", pool_err))
            }
            GossipError::TransactionIsAlreadyHandled => {
                Self::Internal("Transaction already handled".to_string())
            }
            GossipError::CommitmentValidation(commit_err) => {
                Self::Network(format!("Commitment validation error: {}", commit_err))
            }
            GossipError::PeerNetwork(peer_network_err) => {
                Self::Network(format!("Peer network error: {}", peer_network_err))
            }
            GossipError::RateLimited => Self::Network("Rate limited".to_string()),
        }
    }
}

/// Messages that can be sent to the SyncService
#[derive(Debug)]
pub enum SyncChainServiceMessage {
    /// Request an initial sync operation
    InitialSync(oneshot::Sender<ChainSyncResult<()>>),
    /// Check if we need periodic sync (internal message)
    PeriodicSyncCheck,
    /// The block has been processed by the BlockPool, check for unprocessed descendants
    BlockProcessedByThePool {
        block_hash: BlockHash,
        response: Option<oneshot::Sender<ChainSyncResult<()>>>,
    },
    /// Request parent block from the network
    RequestBlockFromTheNetwork {
        block_hash: BlockHash,
        response: Option<oneshot::Sender<ChainSyncResult<()>>>,
    },
    /// Forcefully pulls payload from the network and adds it to payload cache -
    /// doesn't perform any validation except hash check.
    PullPayloadFromTheNetwork {
        evm_block_hash: EvmBlockHash,
        use_trusted_peers_only: bool,
        response: oneshot::Sender<GossipResult<()>>,
    },
    AttemptReprocessingBlock(BlockHash),
}

/// Inner service containing the sync logic
#[derive(Debug, Clone)]
pub struct ChainSyncServiceInner<B: BlockDiscoveryFacade, M: MempoolFacade> {
    sync_state: ChainSyncState,
    peer_list: PeerList,
    config: Config,
    block_index: BlockIndexReadGuard,
    block_pool: Arc<BlockPool<B, M>>,
    /// This field signifies when the sync task is already spawned, but the sync has not started yet.
    ///  The time gap between spawning the task and starting the sync can be significant. The node
    ///  needs to fetch the tip of the block index from the network to figure out how
    ///  much behind the network we are, if at all.
    is_sync_task_spawned: Arc<AtomicBool>,
    gossip_data_handler: Arc<GossipDataHandler<M, B>>,
    reth_service: Option<mpsc::UnboundedSender<Traced<RethServiceMessage>>>,
    /// An atomic bool to enable or disable VDF mining when sync is in progress
    is_vdf_mining_enabled: Arc<AtomicBool>,
    is_update_whitelist_task_running: Arc<AtomicBool>,
}

/// Main sync service that runs in its own tokio task
#[derive(Debug)]
pub struct ChainSyncService<B: BlockDiscoveryFacade, M: MempoolFacade> {
    shutdown: Shutdown,
    msg_rx: mpsc::UnboundedReceiver<SyncChainServiceMessage>,
    inner: ChainSyncServiceInner<B, M>,
}

/// Facade for interacting with the sync service
#[derive(Debug, Clone)]
pub struct SyncChainServiceFacade {
    pub sender: mpsc::UnboundedSender<SyncChainServiceMessage>,
}

impl SyncChainServiceFacade {
    pub fn new(sender: mpsc::UnboundedSender<SyncChainServiceMessage>) -> Self {
        Self { sender }
    }

    /// Request an initial sync and wait for completion
    pub async fn initial_sync(&self) -> ChainSyncResult<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(SyncChainServiceMessage::InitialSync(tx))
            .map_err(|_| {
                ChainSyncError::ServiceCommunication("Failed to send a sync request".to_string())
            })?;

        rx.await.map_err(|_| {
            ChainSyncError::ServiceCommunication("Failed to receive a sync response".to_string())
        })?
    }
}

impl<B: BlockDiscoveryFacade, M: MempoolFacade> ChainSyncServiceInner<B, M> {
    pub fn new(
        sync_state: ChainSyncState,
        peer_list: PeerList,
        config: Config,
        block_index: BlockIndexReadGuard,
        block_pool: Arc<BlockPool<B, M>>,
        gossip_data_handler: Arc<GossipDataHandler<M, B>>,
        reth_service: Option<mpsc::UnboundedSender<Traced<RethServiceMessage>>>,
        is_vdf_mining_enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sync_state,
            peer_list,
            config,
            block_index,
            block_pool,
            is_sync_task_spawned: Arc::new(AtomicBool::new(false)),
            gossip_data_handler,
            reth_service,
            is_vdf_mining_enabled,
            is_update_whitelist_task_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Perform a sync operation
    fn spawn_chain_sync_task(
        &self,
        response: Option<oneshot::Sender<ChainSyncResult<()>>>,
        is_initial_sync: bool,
    ) {
        let is_already_syncing = !is_initial_sync && self.sync_state.is_syncing();
        let is_task_spawned_but_has_not_started_syncing_yet =
            self.is_sync_task_spawned.load(Ordering::Relaxed);

        if is_already_syncing || is_task_spawned_but_has_not_started_syncing_yet {
            debug!("Sync task: Sync already in progress, skipping the new sync request");
            if let Some(response_sender) = response {
                if let Err(e) = response_sender.send(Err(ChainSyncError::Internal(
                    "Sync already in progress".to_string(),
                ))) {
                    error!("Failed to send the sync response: {:?}", e);
                }
            }
            return;
        }

        // Check if we have any new whitelist updates
        self.spawn_stake_and_pledge_update_task();
        self.is_sync_task_spawned.store(true, Ordering::Relaxed);

        let config = self.config.clone();
        let peer_list = self.peer_list.clone();
        let sync_state = self.sync_state.clone();
        let gossip_data_handler = self.gossip_data_handler.clone();
        let is_sync_task_spawned = self.is_sync_task_spawned.clone();
        let block_pool = self.block_pool.clone();
        let reth_service = self.reth_service.clone();
        let is_vdf_mining_enabled = Arc::clone(&self.is_vdf_mining_enabled);
        let start_sync_from_height = self.block_index.read().latest_height();

        tokio::spawn(
            async move {
                debug!("Starting sync from height: {}", start_sync_from_height);

                gossip_data_handler
                    .gossip_client
                    .hydrate_peers_online_status(&peer_list)
                    .await;

                debug!("Sync task: Disabling VDF mining before starting sync");
                let was_mining_enabled_before_sync = is_vdf_mining_enabled.load(Ordering::Relaxed);
                // Disable VDF mining when sync is in progress
                if was_mining_enabled_before_sync {
                    is_vdf_mining_enabled.store(false, Ordering::Relaxed);
                } else {
                    debug!("Sync task: VDF mining was already disabled before sync, not sending disable signal");
                }

                if let Err(err) = block_pool
                    .repair_missing_payloads_if_any(reth_service, Arc::clone(&gossip_data_handler))
                    .await
                {
                    error!(
                        "Sync task: Failed to repair missing payloads before starting sync: {:?}",
                        err
                    );
                }

                let res = sync_chain(
                    sync_state.clone(),
                    &peer_list,
                    start_sync_from_height
                        .try_into()
                        .expect("Expected to be able to convert u64 to usize"),
                    &config,
                    gossip_data_handler,
                )
                .await;

                debug!("Sync task: Enabling VDF mining after sync");
                if was_mining_enabled_before_sync {
                    is_vdf_mining_enabled.store(was_mining_enabled_before_sync, Ordering::Relaxed);
                } else {
                    debug!("Sync task: VDF mining was disabled before sync, not sending enable signal");
                }

                is_sync_task_spawned.store(false, Ordering::Relaxed);

                match &res {
                    Ok(()) => info!("Sync task completed successfully"),
                    Err(e) => {
                        error!("Sync task failed: {}", e);
                        sync_state.finish_sync();
                    }
                }

                if let Some(response_sender) = response {
                    if let Err(e) = response_sender.send(res) {
                        error!("Failed to send the sync response: {:?}", e);
                    }
                }
            }
            .in_current_span(),
        );
    }

    /// Process orphaned ancestor block - moved from OrphanBlockProcessingService
    async fn process_orphaned_ancestors(
        &self,
        block_hash: BlockHash,
    ) -> Result<(), ChainSyncError> {
        let maybe_orphaned_blocks = self
            .block_pool
            .get_orphaned_blocks_by_parent(&block_hash)
            .await;

        if let Some(orphaned_blocks) = maybe_orphaned_blocks {
            let mut futures = Vec::new();
            for orphaned_block in orphaned_blocks {
                info!(
                    "Start processing orphaned ancestor block: {:?}",
                    orphaned_block.block.header().block_hash
                );
                let block_pool = self.block_pool.clone();
                let is_fast_tracking = orphaned_block.is_fast_tracking;
                let orphaned_block_arc = Arc::clone(&orphaned_block.block);
                futures.push(async move {
                    debug!(
                        "Using cached block for orphaned ancestor: {:?}",
                        orphaned_block_arc.header().block_hash
                    );

                    block_pool
                        .process_block(orphaned_block_arc, is_fast_tracking)
                        .await
                        .map_err(|e| {
                            ChainSyncError::Internal(format!("Block processing error: {:?}", e))
                        })
                });
            }
            let results = futures::future::join_all(futures).await;
            let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
            if !errors.is_empty() {
                for err in &errors {
                    error!("Error processing orphaned ancestor block: {}", err);
                }
                return Err(ChainSyncError::Internal(format!(
                    "Errors occurred processing orphaned ancestor blocks: {:?}",
                    errors
                )));
            }
            Ok(())
        } else {
            info!(
                "No orphaned ancestor block found for block: {:?}",
                block_hash
            );
            Ok(())
        }
    }

    fn spawn_stake_and_pledge_update_task(&self) {
        let stake_and_pledge_whitelist_running_flag = self.is_update_whitelist_task_running.clone();
        let is_task_already_running =
            stake_and_pledge_whitelist_running_flag.load(Ordering::Relaxed);
        if is_task_already_running {
            debug!("Stake and pledge whitelist update task is already running, skipping new task spawn");
            return;
        } else {
            stake_and_pledge_whitelist_running_flag.store(true, Ordering::Relaxed);
        }
        let handler = self.gossip_data_handler.clone();
        tokio::spawn(async move {
            match handler.pull_and_process_stake_and_pledge_whitelist().await {
                Ok(()) => {
                    info!("Successfully updated stake and pledge whitelist",);
                }
                Err(e) => {
                    error!("Failed to update stake and pledge whitelist: {:?}", e);
                }
            }
            stake_and_pledge_whitelist_running_flag.store(false, Ordering::Relaxed);
        });
    }

    /// Request parent block from network - moved from OrphanBlockProcessingService
    async fn request_parent_block(
        &self,
        parent_block_hash: BlockHash,
    ) -> Result<(), ChainSyncError> {
        let parent_is_already_in_the_pool =
            self.block_pool.contains_block(&parent_block_hash).await;

        // If the parent is also in the cache, it's likely that processing has already started
        if !parent_is_already_in_the_pool {
            debug!(
                "Orphan service: Parent block {:?} not found in the cache, requesting it from the network",
                parent_block_hash
            );
            self.request_block_from_the_network(parent_block_hash).await
        } else {
            debug!(
                "Parent block {:?} is already in the cache, skipping get data request",
                parent_block_hash
            );
            Ok(())
        }
    }

    /// Request block from network - moved from OrphanBlockProcessingService
    async fn request_block_from_the_network(
        &self,
        block_hash: BlockHash,
    ) -> Result<(), ChainSyncError> {
        self.block_pool.mark_block_as_requested(block_hash).await;
        if let Err(err) = self
            .gossip_data_handler
            .pull_and_process_block(block_hash, self.sync_state.is_trusted_sync())
            .await
        {
            error!(
                "Failed to pull and process block {:?}: {:?}",
                block_hash, err
            );

            if !err.is_advisory() {
                self.block_pool.remove_requested_block(&block_hash).await;
                self.block_pool
                    .remove_block_from_cache(
                        &block_hash,
                        BlockRemovalReason::FailedToProcess(FailureReason::FailedToPull(
                            err.clone(),
                        )),
                    )
                    .await;
            }

            Err(ChainSyncError::Internal(format!(
                "Network error: {:?}",
                err
            )))
        } else {
            Ok(())
        }
    }
}

impl<B: BlockDiscoveryFacade, M: MempoolFacade> ChainSyncService<B, M> {
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn spawn_service(
        inner: ChainSyncServiceInner<B, M>,
        rx: mpsc::UnboundedReceiver<SyncChainServiceMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning sync service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(
            async move {
                let service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner,
                };
                service
                    .start()
                    .await
                    .expect("Sync service encountered an irrecoverable error")
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "sync_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn start(mut self) -> ChainSyncResult<()> {
        info!("Starting the sync service");
        let period_secs = self
            .inner
            .config
            .node_config
            .sync
            .periodic_sync_check_interval_secs;
        let is_periodic_check_enabled = self
            .inner
            .config
            .node_config
            .sync
            .enable_periodic_sync_check;

        let periodic_sync_check_interval = if is_periodic_check_enabled && period_secs > 0 {
            Duration::from_secs(period_secs)
        } else {
            Duration::MAX
        };
        // Set up a periodic sync check timer
        let mut periodic_timer = interval(periodic_sync_check_interval);
        periodic_timer.tick().await; // Consume the first immediate tick

        loop {
            tokio::select! {
                biased; // enable bias so polling happens in definition order

                // Check for a shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for sync service");
                    break;
                }

                // Handle periodic sync checks
                _ = periodic_timer.tick() => {
                    self.handle_periodic_sync_check().await;
                }

                // Handle commands
                cmd = self.msg_rx.recv() => {
                    debug!("SyncChainService: Received a command from the channel");
                    match cmd {
                        Some(cmd) => {
                            self.handle_message(cmd).await?;
                        }
                        None => {
                            warn!("Sync service command channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("Shutting down sync service gracefully");
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn handle_message(&self, msg: SyncChainServiceMessage) -> ChainSyncResult<()> {
        match msg {
            SyncChainServiceMessage::InitialSync(response_sender) => {
                self.inner
                    .spawn_chain_sync_task(Some(response_sender), true);
            }
            SyncChainServiceMessage::PeriodicSyncCheck => {
                self.handle_periodic_sync_check().await;
            }
            SyncChainServiceMessage::BlockProcessedByThePool {
                block_hash,
                response,
            } => {
                debug!("SyncChainService: Received a signal that block {:?} has been processed by the pool, looking for unprocessed descendants", block_hash);
                let inner = self.inner.clone();
                // Check for whitelist updates after every block validation
                self.inner.spawn_stake_and_pledge_update_task();
                tokio::spawn(async move {
                    let result = inner.process_orphaned_ancestors(block_hash).await;
                    if let Some(sender) = response {
                        if let Err(e) = sender.send(result) {
                            tracing::error!("Failed to send response: {:?}", e);
                        }
                    }
                });
            }
            SyncChainServiceMessage::RequestBlockFromTheNetwork {
                block_hash: parent_block_hash,
                response,
            } => {
                debug!(
                    "SyncChainService: Received a request to fetch block {:?} from the network",
                    parent_block_hash
                );
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    let result = inner.request_parent_block(parent_block_hash).await;
                    if let Some(sender) = response {
                        if let Err(e) = sender.send(result) {
                            tracing::error!("Failed to send response: {:?}", e);
                        }
                    }
                });
            }
            SyncChainServiceMessage::PullPayloadFromTheNetwork {
                evm_block_hash,
                response,
                use_trusted_peers_only,
            } => {
                debug!(
                    "SyncChainService: Received a request to force pull an execution payload for evm block hash {:?}",
                    evm_block_hash
                );
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    let result = inner
                        .gossip_data_handler
                        .pull_and_add_execution_payload_to_cache(
                            evm_block_hash,
                            use_trusted_peers_only,
                        )
                        .await;
                    if let Err(e) = response.send(result) {
                        tracing::error!("Failed to send response: {:?}", e);
                    }
                });
            }
            SyncChainServiceMessage::AttemptReprocessingBlock(block_hash) => {
                debug!(
                    "SyncChainService: Received a request to attempt reprocessing block {:?}",
                    block_hash
                );
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    // Get cached block header from block_pool
                    if let Some(cached_block) = inner.block_pool.get_cached_block(&block_hash).await
                    {
                        if let Err(e) = inner
                            .block_pool
                            .process_block(
                                Arc::clone(&cached_block.block),
                                cached_block.is_fast_tracking,
                            )
                            .await
                        {
                            error!("Failed to reprocess block {:?}: {:?}", block_hash, e);
                        }
                    } else {
                        error!(
                            "Cannot reprocess block {:?} - not found in cache",
                            block_hash
                        );
                    }
                });
            }
        }
        Ok(())
    }

    async fn handle_periodic_sync_check(&self) {
        self.inner
            .gossip_data_handler
            .gossip_client
            .hydrate_peers_online_status(&self.inner.peer_list)
            .await;
        debug!("Starting a periodic sync check routine");
        // Check if we're behind the network
        match is_local_index_is_behind_trusted_peers(
            &self.inner.config,
            &self.inner.peer_list,
            &self.inner.gossip_data_handler.gossip_client,
            &self.inner.block_index,
        )
        .await
        {
            Ok(true) => {
                info!("Periodic sync check: We're behind the network, starting sync");
                self.inner.spawn_chain_sync_task(None, false);
            }
            Ok(false) => {
                debug!("Periodic sync check: We're up to date with the network");
            }
            Err(e) => {
                error!(
                    "Periodic sync check: Failed to check if behind network: {:?}, trusted peers are likely offline",
                    e
                );
            }
        }
    }
}

/// Groups the sync-related configuration values used throughout `sync_chain`.
struct SyncParams {
    migration_depth: usize,
    sync_mode: SyncMode,
    is_trusted_mode: bool,
    is_a_genesis_node: bool,
    block_batch_size: usize,
    retry_block_request_timeout_secs: u64,
    wait_queue_slot_timeout_secs: u64,
    wait_queue_slot_max_attempts: usize,
    genesis_peer_discovery_timeout_millis: u64,
}

impl SyncParams {
    fn from_config(config: &Config) -> Self {
        let sync_mode = config.node_config.sync_mode;
        Self {
            migration_depth: config.consensus.block_migration_depth as usize,
            sync_mode,
            is_trusted_mode: matches!(sync_mode, SyncMode::Trusted),
            is_a_genesis_node: matches!(config.node_config.node_mode, NodeMode::Genesis),
            block_batch_size: config.node_config.sync.block_batch_size,
            retry_block_request_timeout_secs: config
                .node_config
                .sync
                .retry_block_request_timeout_secs,
            wait_queue_slot_timeout_secs: config.node_config.sync.wait_queue_slot_timeout_secs,
            wait_queue_slot_max_attempts: config.node_config.sync.wait_queue_slot_max_attempts,
            genesis_peer_discovery_timeout_millis: config
                .node_config
                .genesis_peer_discovery_timeout_millis,
        }
    }
}

#[instrument(skip_all, err)]
async fn sync_chain<B: BlockDiscoveryFacade, M: MempoolFacade>(
    sync_state: ChainSyncState,
    peer_list: &PeerList,
    mut start_sync_from_height: usize,
    config: &irys_types::Config,
    gossip_data_handler: Arc<GossipDataHandler<M, B>>,
) -> ChainSyncResult<()> {
    let gossip_client = &gossip_data_handler.gossip_client;
    let params = SyncParams::from_config(config);

    // If the peer doesn't have any blocks, it should start syncing from 1, as the genesis block
    // should always be present
    if start_sync_from_height == 0 {
        start_sync_from_height = 1;
    }

    if !check_sync_preconditions(&sync_state, &params, start_sync_from_height) {
        return Ok(());
    }

    if !initialize_sync_mode(
        &sync_state,
        &params,
        config,
        peer_list,
        gossip_client,
        start_sync_from_height,
    )
    .await?
    {
        return Ok(());
    }

    let block_queue =
        fetch_initial_block_index(&sync_state, &params, peer_list, gossip_client).await?;

    if !block_queue.is_empty() {
        run_sync_loop(
            &sync_state,
            &params,
            config,
            peer_list,
            gossip_client,
            &gossip_data_handler,
            block_queue,
            start_sync_from_height,
        )
        .await?;
    }

    finalize_sync(
        &sync_state,
        &params,
        peer_list,
        gossip_client,
        &gossip_data_handler,
    )
    .await;

    info!("Sync task: Gossip service sync task completed");
    Ok(())
}

/// Checks early-exit conditions: gossip reception disabled, genesis node with no blocks.
/// Returns `true` if sync should proceed, `false` if it should be skipped.
fn check_sync_preconditions(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    start_sync_from_height: usize,
) -> bool {
    if !sync_state.is_gossip_reception_enabled() {
        debug!("Sync task: Gossip reception is disabled, skipping sync");
        sync_state.finish_sync();
        return false;
    }

    sync_state.set_syncing_from(start_sync_from_height);
    sync_state.set_trusted_sync(params.is_trusted_mode);

    if params.is_a_genesis_node && sync_state.sync_target_height() <= 1 {
        debug!("Sync task: The node is a genesis node with no blocks, skipping the sync task");
        sync_state.finish_sync();
        return false;
    }

    true
}

/// Waits for active peers and initializes trusted sync mode if applicable.
/// Returns `true` if sync should proceed, `false` if it should be skipped (genesis node cases).
async fn initialize_sync_mode(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    config: &Config,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    start_sync_from_height: usize,
) -> ChainSyncResult<bool> {
    debug!("Sync task: Starting a chain sync task, waiting for active peers. Mode: {:?}, starting from height: {}, trusted mode: {}", params.sync_mode, start_sync_from_height, sync_state.is_trusted_sync());

    if params.is_a_genesis_node {
        warn!("Sync task: Because the node is a genesis node, waiting for active peers for {}, and if no peers are added, then skipping the sync task", params.genesis_peer_discovery_timeout_millis);
        match timeout(
            Duration::from_millis(params.genesis_peer_discovery_timeout_millis),
            peer_list.wait_for_active_peers(),
        )
        .await
        {
            Ok(()) => {
                info!("Genesis node has active peers");
            }
            Err(elapsed) => {
                warn!("Sync task: Due to the node being in genesis mode, after waiting for active peers for {} and no peers showing up, skipping the sync task", elapsed);
                sync_state.finish_sync();
                return Ok(false);
            }
        };
    } else {
        sync_state.set_is_syncing(true);
        peer_list.wait_for_active_peers().await;
    }

    debug!("Sync task: Syncing started");

    if params.is_trusted_mode {
        let trusted_peers = peer_list.online_trusted_peers();
        if trusted_peers.is_empty() {
            return if params.is_a_genesis_node {
                sync_state.mark_processed(sync_state.sync_target_height());
                sync_state.finish_sync();
                Ok(false)
            } else {
                Err(ChainSyncError::Network(
                    "No trusted peers available".to_string(),
                ))
            };
        }

        if let Err(err) = check_and_update_full_validation_switch_height(
            config,
            peer_list,
            gossip_client,
            sync_state,
        )
        .await
        {
            if params.is_a_genesis_node {
                warn!("Since the node is a genesis node, skipping the sync task due to being unable to verify the full validation switch height from trusted peers: {}", err);
            } else {
                return Err(err);
            }
        }
    }

    Ok(true)
}

/// Fetches the initial block index from the network and prepares the block queue.
/// Returns an empty queue if no blocks are available; lifecycle completion is handled by `finalize_sync`.
async fn fetch_initial_block_index(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
) -> ChainSyncResult<VecDeque<BlockIndexItem>> {
    let block_index = match get_block_index(
        peer_list,
        gossip_client,
        // +1, because the index endpoint is inclusive, and we don't want to fetch the last block again
        sync_state.sync_target_height() + 1,
        params.block_batch_size,
        BLOCK_INDEX_RETRIES,
        params.is_trusted_mode,
    )
    .await
    {
        Ok(index) => {
            debug!("Sync task: Fetched block index: {:?}", index);
            index
        }
        Err(err) => {
            error!("Sync task: Failed to fetch block index: {}", err);
            if params.is_a_genesis_node {
                warn!("Sync task: Because the node is a genesis node, skipping the sync task due to being unable to fetch the index from peers");
                vec![]
            } else {
                return Err(err);
            }
        }
    };

    let block_queue = VecDeque::from(block_index);

    // If no new blocks were added to the index, nothing is going to mark
    //  the tip as processed
    if block_queue.is_empty() {
        debug!("Sync task: No new blocks to process, marking the current sync target height as processed");
        sync_state.mark_processed(sync_state.sync_target_height());
    } else {
        sync_state.set_is_syncing(true);
    }

    Ok(block_queue)
}

/// Main sync loop: pulls blocks from the network in batches and processes them.
async fn run_sync_loop<B: BlockDiscoveryFacade, M: MempoolFacade>(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    config: &Config,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    gossip_data_handler: &Arc<GossipDataHandler<M, B>>,
    mut block_queue: VecDeque<BlockIndexItem>,
    start_sync_from_height: usize,
) -> ChainSyncResult<()> {
    let mut blocks_to_request = block_queue.len();
    let mut sync_target = sync_state.sync_target_height() + block_queue.len();
    sync_state.set_sync_target_height(sync_target);

    while let Some(block) = block_queue.pop_front() {
        if sync_state.is_queue_full() {
            wait_for_queue_slot(
                sync_state,
                params,
                peer_list,
                gossip_client,
                gossip_data_handler,
            )
            .await?;
        }

        spawn_block_pull_task(sync_state, gossip_data_handler, block.block_hash);

        blocks_to_request -= 1;
        if blocks_to_request == 0 {
            match replenish_block_batch(
                sync_state,
                params,
                config,
                peer_list,
                gossip_client,
                &mut block_queue,
                &mut sync_target,
                start_sync_from_height,
            )
            .await?
            {
                BatchReplenishResult::Replenished { new_count } => {
                    blocks_to_request = new_count;
                }
                BatchReplenishResult::SyncComplete => break,
            }
        }
    }

    Ok(())
}

/// Waits for all blocks to be processed, marks sync as finished, and pulls highest blocks from peers.
async fn finalize_sync<B: BlockDiscoveryFacade, M: MempoolFacade>(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    gossip_data_handler: &Arc<GossipDataHandler<M, B>>,
) {
    debug!("Sync task: Block queue is empty, waiting for the highest processed block to reach the target sync height");
    sync_state
        .wait_for_processed_block_to_reach_target()
        .in_current_span()
        .await;
    sync_state.finish_sync();

    // After finishing primary sync, best-effort pull of all unique highest blocks announced by peers.
    match pull_unique_highest_blocks(
        peer_list,
        gossip_client,
        gossip_data_handler.clone(),
        params.is_trusted_mode,
        params.retry_block_request_timeout_secs,
    )
    .await
    {
        Ok(()) => {
            debug!("Post-sync: Pulled unique highest blocks, if any");
        }
        Err(e) => {
            warn!("Post-sync: Failed to pull unique highest blocks: {}", e);
        }
    }
}

/// Spawns a tokio task that pulls and processes a single block from the network.
fn spawn_block_pull_task<B: BlockDiscoveryFacade, M: MempoolFacade>(
    sync_state: &ChainSyncState,
    gossip_data_handler: &Arc<GossipDataHandler<M, B>>,
    block_hash: BlockHash,
) {
    let sync_state = sync_state.clone();
    let data_handler = gossip_data_handler.clone();

    tokio::spawn(
        async move {
            debug!(
                "Sync task: Requesting block {:?} (sync target height is {}) from the network",
                block_hash,
                sync_state.sync_target_height()
            );

            match data_handler
                .pull_and_process_block(block_hash, sync_state.is_syncing_from_a_trusted_peer())
                .await
            {
                Ok(()) => {
                    debug!("Sync task: Successfully processed block {:?}", block_hash);
                }
                Err(e) => {
                    error!("Sync task: Failed to process block {:?}: {}", block_hash, e);
                }
            }
        }
        .in_current_span(),
    );
}

enum BatchReplenishResult {
    Replenished { new_count: usize },
    SyncComplete,
}

/// Fetches the next batch of blocks from the network when the current batch is exhausted.
/// Returns `SyncComplete` if no more blocks are available, or `Replenished` with the new count.
async fn replenish_block_batch(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    config: &Config,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    block_queue: &mut VecDeque<BlockIndexItem>,
    sync_target: &mut usize,
    start_sync_from_height: usize,
) -> ChainSyncResult<BatchReplenishResult> {
    if params.is_trusted_mode {
        check_and_update_full_validation_switch_height(
            config,
            peer_list,
            gossip_client,
            sync_state,
        )
        .await?;
    }

    let additional_index = get_block_index(
        peer_list,
        gossip_client,
        *sync_target,
        params.block_batch_size,
        BLOCK_INDEX_RETRIES,
        params.is_trusted_mode,
    )
    .await?;

    *sync_target += additional_index.len();
    sync_state.set_sync_target_height(*sync_target);

    block_queue.extend(additional_index);
    let new_count = block_queue.len();

    if new_count == 0 {
        let estimated_height =
            estimate_canonical_height(peer_list, gossip_client, start_sync_from_height as u64)
                .await;
        let safe_threshold = (sync_state
            .sync_target_height()
            .saturating_add(params.migration_depth))
        .saturating_sub(1) as u64;
        if estimated_height > safe_threshold {
            error!(
                "block index request for entries >{} returned no extra results, but the estimated network height is {} (threshold: {})",
                sync_target, estimated_height, safe_threshold
            );
            return Err(ChainSyncError::Internal(format!(
                "Block index request for entries >{} returned no extra results, but the estimated network height is {} (threshold: {})",
                sync_target, estimated_height, safe_threshold
            )));
        }
        debug!(
            "block index request for entries >{} returned no extra results",
            sync_target
        );
        return Ok(BatchReplenishResult::SyncComplete);
    }

    Ok(BatchReplenishResult::Replenished { new_count })
}

/// Waits for a queue slot to become available, with retry logic that attempts
/// to trigger block processing if the queue remains full with no active validations.
async fn wait_for_queue_slot<B: BlockDiscoveryFacade, M: MempoolFacade>(
    sync_state: &ChainSyncState,
    params: &SyncParams,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    gossip_data_handler: &GossipDataHandler<M, B>,
) -> ChainSyncResult<()> {
    debug!("Sync task: Block queue is full, waiting for an empty slot");

    match sync_state
        .wait_for_an_empty_queue_slot_with_validation_awareness(
            Duration::from_secs(params.wait_queue_slot_timeout_secs),
            params.wait_queue_slot_max_attempts,
        )
        .await
    {
        Ok(()) => {
            debug!("Sync task: Queue slot became available");
            return Ok(());
        }
        Err(e) => {
            warn!(
                "Sync task: Wait failed ({}), attempting to trigger block processing",
                e
            );
        }
    }

    // All attempts failed with no active validations - try to trigger processing
    let retry_height = sync_state.highest_processed_block() + 1;
    debug!(
        "Sync task: Attempting to request block at height {} to trigger processing",
        retry_height
    );

    match get_block_index(
        peer_list,
        gossip_client,
        retry_height,
        1, // Just get one block
        QUEUE_SLOT_RETRY_BLOCK_INDEX_RETRIES,
        params.is_trusted_mode,
    )
    .await
    {
        Ok(retry_index) if !retry_index.is_empty() => {
            let retry_block = &retry_index[0];
            debug!(
                "Sync task: Got retry block {:?} for height {}, requesting from network",
                retry_block.block_hash, retry_height
            );

            match timeout(
                Duration::from_secs(params.retry_block_request_timeout_secs),
                gossip_data_handler.pull_and_process_block(
                    retry_block.block_hash,
                    sync_state.is_syncing_from_a_trusted_peer(),
                ),
            )
            .await
            {
                Ok(Ok(())) => {
                    debug!(
                        "Sync task: Successfully requested retry block {:?} from network",
                        retry_block.block_hash
                    );
                    // Give some time for validation to start, then continue the loop
                    tokio::time::sleep(RETRY_BLOCK_VALIDATION_GRACE).await;
                }
                Ok(Err(e)) => {
                    warn!(
                        "Sync task: Failed to request retry block {:?} from network: {}",
                        retry_block.block_hash, e
                    );
                }
                Err(_) => {
                    warn!(
                        "Sync task: Timeout ({:?}s) while requesting retry block {:?} from network",
                        params.retry_block_request_timeout_secs, retry_block.block_hash
                    );
                }
            }
        }
        Ok(_) => {
            warn!(
                "Sync task: Retry attempt returned empty index for height {}",
                retry_height
            );
        }
        Err(err) => {
            warn!(
                "Sync task: Failed to get retry block index for height {}: {}",
                retry_height, err
            );
        }
    }

    // Final check: if still no active validations and queue is still full, fail
    if sync_state.queue_full_with_no_active_validations() {
        error!("Sync task: Queue still full with no active validations after retry, exiting sync");
        return Err(ChainSyncError::Internal(
            "Failed to get queue slot after timeout and retries with no active validations"
                .to_string(),
        ));
    }

    Ok(())
}

/// Collects unique highest block hashes reported by peers and associates them
/// with the peers that reported each hash.
///
/// Selection strategy:
/// - If `use_trusted_peers_only` is true, query all trusted peers.
/// - Otherwise, query the top `top_n` active peers (default 10 if None).
///
/// For each selected peer, it requests `/info` and collects the `block_hash` and the peer's
/// reported `block_index_height`.
/// Returns a map of `block_hash -> (reported_height, Vec<(miner_address, PeerListItem)>)`.
/// If multiple peers report the same hash, the max height observed is retained and all reporting
/// peers are grouped together.
async fn pull_highest_blocks(
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    use_trusted_peers_only: bool,
    top_n: Option<usize>,
) -> ChainSyncResult<HashMap<BlockHash, (u64, Vec<(IrysPeerId, PeerListItem)>)>> {
    // Pick peers: trusted or top N active
    let peers: Vec<(IrysPeerId, irys_types::PeerListItem)> = if use_trusted_peers_only {
        debug!("Post-sync: Collecting the highest blocks from trusted peers");
        peer_list.online_trusted_peers()
    } else {
        let limit = top_n.unwrap_or(10);
        debug!(
            "Post-sync: Collecting the highest blocks from top {} active peers",
            limit
        );
        peer_list.top_active_peers(Some(limit), None)
    };

    if peers.is_empty() {
        warn!("Post-sync: No peers available to pull the highest blocks from");
        return Err(ChainSyncError::Network(
            "No online peers available".to_string(),
        ));
    }

    let mut peers_by_top_block_hash: HashMap<BlockHash, (u64, Vec<(IrysPeerId, PeerListItem)>)> =
        HashMap::new();

    for (miner_address, peer) in peers {
        match gossip_client.get_info(peer.address).await {
            Ok(info) => {
                let hash = info.block_hash;
                let height = info.height;
                let entry = peers_by_top_block_hash
                    .entry(hash)
                    .or_insert_with(|| (height, Vec::new()));
                // Retain the max reported height for this hash
                if height > entry.0 {
                    entry.0 = height;
                }
                // Avoid duplicates if same miner appears more than once (shouldn't, but safe)
                if !entry.1.iter().any(|(addr, _)| addr == &miner_address) {
                    entry.1.push((miner_address, peer.clone()));
                }
                debug!(
                    "Post-sync: Peer {:?} reported highest block hash {:?} (gossip: {})",
                    miner_address, hash, peer.address.gossip
                );
            }
            Err(err) => {
                warn!(
                    "Post-sync: Failed to fetch node info from peer {:?} (gossip: {}): {}",
                    miner_address, peer.address.gossip, err
                );
            }
        }
    }

    Ok(peers_by_top_block_hash)
}

/// Collects unique highest block hashes from peers and pulls each of them in parallel.
/// Each pull uses the same timeout logic as used elsewhere in the sync task.
async fn pull_unique_highest_blocks<B: BlockDiscoveryFacade, M: MempoolFacade>(
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    gossip_data_handler: Arc<GossipDataHandler<M, B>>,
    use_trusted_peers_only: bool,
    retry_block_request_timeout_secs: u64,
) -> ChainSyncResult<()> {
    // Collect unique hashes with their reporting peers (trusted or top-N active peers)
    let peers_by_top_block_hash =
        pull_highest_blocks(peer_list, gossip_client, use_trusted_peers_only, None).await?;

    if peers_by_top_block_hash.is_empty() {
        debug!("Post-sync: No unique highest blocks reported by peers");
        return Ok(());
    }

    let mut futures = Vec::new();
    for (block_hash, (reported_height, peers)) in peers_by_top_block_hash {
        debug!(
            "Post-sync: Considering pull of block {:?} (height {}) reported by {} peer(s)",
            block_hash,
            reported_height,
            peers.len()
        );
        // Filter out blocks that are already processed or currently being processed
        if gossip_data_handler
            .block_pool
            .is_block_processing_or_processed(&block_hash, reported_height)
            .await
        {
            debug!(
                "Post-sync: Skipping already-processed block {:?} (height {})",
                block_hash, reported_height
            );
            continue;
        }

        if peers.is_empty() {
            warn!(
                "Post-sync: No peers associated with the reported block hash {:?} (skipping)",
                block_hash
            );
            continue;
        }

        let handler = gossip_data_handler.clone();
        // Retry sequentially across available peers for this hash until success or exhaustion
        futures.push(async move {
            let mut success = false;
            for (idx, peer) in peers.into_iter().enumerate() {
                let attempt_num = idx + 1;
                match tokio::time::timeout(
                    Duration::from_secs(retry_block_request_timeout_secs),
                    handler.pull_and_process_block_from_peer(block_hash, &peer),
                )
                .await
                {
                    Ok(Ok(())) => {
                        debug!(
                            "Post-sync: Pulled block {:?} from peer {:?} on attempt {}",
                            block_hash, peer.0, attempt_num
                        );
                        success = true;
                        break;
                    }
                    Ok(Err(e)) => {
                        warn!(
                            "Post-sync: Attempt {} failed to pull block {:?} from peer {:?}: {}",
                            attempt_num, block_hash, peer.0, e
                        );
                    }
                    Err(_) => {
                        warn!(
                            "Post-sync: Attempt {} timed out ({}s) pulling block {:?} from peer {:?}",
                            attempt_num, retry_block_request_timeout_secs, block_hash, peer.0
                        );
                    }
                }

                // Small backoff before trying another peer
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            if !success {
                warn!(
                    "Post-sync: Exhausted all peers without pulling block {:?}",
                    block_hash
                );
            }
        });
    }

    // Run all pulls in parallel and wait for completion
    futures::future::join_all(futures).await;
    Ok(())
}

async fn check_and_update_full_validation_switch_height(
    config: &Config,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    sync_state: &ChainSyncState,
) -> ChainSyncResult<()> {
    // We should enable full validation when the index nears the (tip - migration depth)
    let migration_depth = config.consensus.block_migration_depth as usize;
    let trusted_peers = peer_list.online_trusted_peers();
    if trusted_peers.is_empty() {
        return Err(ChainSyncError::Network(
            "No trusted peers available".to_string(),
        ));
    }

    let mut switch_height_set = false;
    let max_retries = 5;

    for retry_attempt in 0..max_retries {
        if retry_attempt > 0 {
            debug!(
                "Sync task: Retry attempt {} for fetching node info from trusted peers",
                retry_attempt + 1
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        for (_, peer) in trusted_peers.iter() {
            debug!("Sync task: Trusted peer: {:?}", peer);
            let node_info = match gossip_client.get_info(peer.address).await {
                Ok(info) => info,
                Err(err) => {
                    warn!(
                            "Sync task: Failed to fetch node info from trusted peer {}: {}, trying another peer",
                            peer.address.gossip, err
                        );
                    continue;
                }
            };

            let index_tip = node_info.block_index_height;
            if index_tip > migration_depth as u64 {
                let switch_height = index_tip as usize - migration_depth;
                sync_state.set_switch_to_full_validation_at_height(Some(switch_height));
                debug!(
                    "Sync task: Setting switch to full validation at height {}",
                    switch_height
                );
                switch_height_set = true;
                break; // Exit the inner loop as soon as we successfully get data from a peer
            } else {
                warn!(
                    "Sync task: Not enough blocks in the index to switch to full validation, index tip: {}, migration depth: {}",
                    index_tip, migration_depth
                );
                switch_height_set = true;
                break; // Exit the inner loop as we got valid data, even if we can't set switch height
            }
        }

        if switch_height_set {
            break; // Exit the outer retry loop if we successfully got data
        }
    }

    if !switch_height_set {
        Err(ChainSyncError::Network(
            "Failed to get node info from any trusted peer after 5 retry attempts".to_string(),
        ))
    } else {
        Ok(())
    }
}

#[instrument(skip_all, fields(
    index_request.start = start,
    index_request.limit = limit,
    index_request.retries = retries,
    index_request.fetch_from_trusted_peers_only = fetch_from_trusted_peers_only
), err)]
async fn get_block_index(
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    start: usize,
    limit: usize,
    retries: usize,
    fetch_from_trusted_peers_only: bool,
) -> ChainSyncResult<Vec<BlockIndexItem>> {
    debug!(
        "Fetching block index starting from height {} with limit {}",
        start, limit
    );
    // Ideally, if we fetch an index batch, we should wait until the whole batch is processed
    //  before fetching the next batch to avoid errors in case if a batch had a faulty block.
    //  However, in the first version it doesn't matter as much as the first release, since
    //  at first the network is going to run in a controlled environment with trusted nodes.
    //  However, we should make a more sophisticated algorithm for a follow-up release.
    let peers = synced_peers_sorted_by_cumulative_diff(
        peer_list,
        gossip_client,
        fetch_from_trusted_peers_only,
    )
    .await?;

    if peers.is_empty() {
        return Err(ChainSyncError::Network("No peers available".to_string()));
    }

    for _ in 0..retries {
        let mut peers = peers.clone();

        while let Some((diff, mut peers_to_fetch_index_from)) = peers.pop_last() {
            debug!("Peers with cumulative diff {:?}: {:?}", diff, peers.len());
            peers_to_fetch_index_from.shuffle(&mut rand::thread_rng());

            for (mining_addr, peer) in &peers_to_fetch_index_from {
                debug!(
                    peer.address = ?peer.address.gossip,
                    index.start = start,
                    index.limit = limit,
                    "Fetching block index from peer {:?}",
                    mining_addr
                );

                match gossip_client
                    .get_block_index(
                        peer.address,
                        BlockIndexQuery {
                            height: start,
                            limit,
                        },
                    )
                    .await
                {
                    Ok(index) => {
                        debug!(
                            peer.address = ?peer.address.gossip,
                            index.start = start,
                            index.limit = limit,
                            "Fetched block index from peer {:?}: {:?}",
                            mining_addr, index
                        );
                        return Ok(index);
                    }
                    Err(error) => {
                        warn!(
                            peer.address = ?peer.address.gossip,
                            index.start = start,
                            index.limit = limit,
                            "Failed to fetch a block index from peer {:?}: {:?}",
                            mining_addr, error
                        );
                    }
                }
            }
        }
    }

    Err(ChainSyncError::Network(
        "Failed to fetch block index from peer".to_string(),
    ))
}

/// Check if the local index is behind trusted peers. Returns true if behind, false if up to date.
async fn is_local_index_is_behind_trusted_peers(
    config: &Config,
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    block_index: &BlockIndexReadGuard,
) -> ChainSyncResult<bool> {
    let migration_depth = u64::from(config.consensus.block_migration_depth);

    let mut highest_trusted_peer_height = None;

    let trusted_peers = peer_list.online_trusted_peers();
    if trusted_peers.is_empty() {
        return Err(ChainSyncError::Network(
            "No trusted peers available that are online".to_string(),
        ));
    }

    for (_, peer) in trusted_peers.iter() {
        debug!("Sync task: Trusted peer: {:?}", peer);
        let node_info = match gossip_client.get_info(peer.address).await {
            Ok(info) => info,
            Err(err) => {
                warn!("Sync task: Failed to fetch node info from trusted peer {}: {}, trying another peer", peer.address.gossip, err);
                continue;
            }
        };

        let index_tip = node_info.block_index_height;

        if let Some(peer_height) = highest_trusted_peer_height {
            if index_tip > peer_height {
                highest_trusted_peer_height = Some(index_tip);
            }
        } else {
            highest_trusted_peer_height = Some(index_tip);
        }
    }

    if let Some(highest_trusted_peer_height) = highest_trusted_peer_height {
        Ok((block_index.read().latest_height() + (migration_depth * 2))
            < highest_trusted_peer_height)
    } else {
        Err(ChainSyncError::Network(
            "Wasn't able to fetch node info from any of the trusted peers".to_string(),
        ))
    }
}

/// This function estimates the canonical height by querying trusted peers for their block index height.
/// It returns the highest block index height reported by trusted peers.
async fn estimate_canonical_height(
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    mut highest_trusted_peer_height: u64,
) -> u64 {
    let mut highest_difficulty = U256::zero();
    // Don't wait for hydration, since we're just asking the API endpoints of trusted peers
    let trusted_peers = peer_list.all_trusted_peers();
    if trusted_peers.is_empty() {
        warn!("The node has no trusted peers configured, falling back to local index height for canonical height estimation");
        return highest_trusted_peer_height;
    }

    let futures = trusted_peers.iter().map(|(_, peer)| {
        debug!("Sync task: Trusted peer: {:?}", peer);
        let gossip_client = gossip_client.clone();
        async move {
            match gossip_client.get_info(peer.address).await {
                Ok(info) => Some((info.block_index_height, info.cumulative_difficulty)),
                Err(err) => {
                    warn!("Sync task: Failed to fetch node info from trusted peer {}: {}, trying another peer", peer.address.gossip, err);
                    None
                }
            }
        }
    });

    let infos = futures::future::join_all(futures).await;

    for info in infos.into_iter().flatten() {
        let difficulty = info.1;
        if difficulty > highest_difficulty {
            debug!(
                "Sync task: Updating the highest trusted peer height from {} to the value from Info {:?}",
                highest_trusted_peer_height, info
            );
            highest_difficulty = difficulty;
            highest_trusted_peer_height = info.0;
        }
    }

    highest_trusted_peer_height
}

async fn synced_peers_sorted_by_cumulative_diff(
    peer_list: &PeerList,
    gossip_client: &GossipClient,
    trusted_peers_only: bool,
) -> ChainSyncResult<BTreeMap<U256, Vec<(IrysPeerId, PeerListItem)>>> {
    let peers = if trusted_peers_only {
        peer_list.online_trusted_peers()
    } else {
        peer_list.top_active_peers(None, None)
    };
    if peers.is_empty() {
        return Err(ChainSyncError::Network(
            "No online peers available".to_string(),
        ));
    }

    let peers_and_diffs_futures = peers.into_iter().map(|(addr, peer)| {
        let gossip_client = gossip_client.clone();
        async move {
            match gossip_client.get_info(peer.address).await {
                Ok(info) => {
                    if !info.is_syncing {
                        Ok((addr, peer, info.cumulative_difficulty))
                    } else {
                        Err(ChainSyncError::Network(format!(
                            "Peer {addr:?} is syncing, skipping"
                        )))
                    }
                }
                Err(err) => Err(ChainSyncError::Network(format!(
                    "Failed to fetch node info from peer {:?}: {}",
                    addr, err
                ))),
            }
        }
    });

    let mut peers_and_diffs = BTreeMap::new();
    for res in futures::future::join_all(peers_and_diffs_futures).await {
        match res {
            Ok((addr, peer, diff)) => {
                peers_and_diffs
                    .entry(diff)
                    .or_insert_with(Vec::new)
                    .push((addr, peer));
            }
            Err(err) => warn!("{}", err),
        }
    }

    if peers_and_diffs.is_empty() {
        return Err(ChainSyncError::Network(
            "No peers available after fetching cumulative difficulties".to_string(),
        ));
    }

    Ok(peers_and_diffs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_network_service::spawn_peer_network_service_with_client;
    use crate::tests::util::FakeGossipServer;
    use futures::future::BoxFuture;
    use futures::{future, FutureExt as _};
    use irys_types::RethPeerInfo;
    use std::sync::Arc;

    fn noop_reth_peer_sender() -> Arc<dyn Fn(RethPeerInfo) -> BoxFuture<'static, ()> + Send + Sync>
    {
        Arc::new(|peer_info: RethPeerInfo| {
            let _ = peer_info;
            future::ready(()).boxed()
        })
    }
    use irys_types::BlockHash;

    mod catch_up_task {
        use super::*;
        use crate::tests::util::data_handler_stub;
        use crate::types::GossipResponse;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
        use irys_types::{
            Config, DatabaseProvider, IrysAddress, IrysBlockHeader, IrysPeerId, NodeConfig,
            PeerAddress, PeerListItem, PeerNetworkSender, PeerScore,
        };
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};

        #[tokio::test]
        async fn should_sync_and_change_status() -> eyre::Result<()> {
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let start_from = 10;
            let sync_state = ChainSyncState::new(true, false);

            let db = DatabaseProvider(Arc::new(
                open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                    .expect("can't open temp dir"),
            ));

            let block_requests = Arc::new(Mutex::new(vec![]));
            let block_requests_clone = block_requests.clone();
            let fake_gossip_server = FakeGossipServer::new();
            let sync_state_clone = sync_state.clone();
            fake_gossip_server.set_on_pull_data_request(move |data_request| {
                match data_request {
                    GossipDataRequestV2::ExecutionPayload(_) => GossipResponse::Accepted(None),
                    GossipDataRequestV2::BlockHeader(block_hash) => {
                        info!("Fake server pull data request: {block_hash:?}");
                        let mut block_requests = block_requests.lock().unwrap();
                        let requests_len = block_requests.len();
                        block_requests.push(block_hash);

                        // Simulating one false response so the block gets requested again
                        if requests_len == 0 {
                            GossipResponse::Accepted(None)
                        } else {
                            sync_state_clone.mark_processed(start_from + requests_len);
                            let random_signer = NodeConfig::testing().new_random_signer();
                            let mut mock_header = IrysBlockHeader::new_mock_header();
                            random_signer
                                .sign_block_header(&mut mock_header)
                                .expect("to sign mock header");
                            GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::new(
                                mock_header,
                            ))))
                        }
                    }
                    GossipDataRequestV2::Chunk(_) => GossipResponse::Accepted(None),
                    GossipDataRequestV2::BlockBody(_) => GossipResponse::Accepted(None),
                    GossipDataRequestV2::Transaction(_) => GossipResponse::Accepted(None),
                }
            });

            let calls = Arc::new(Mutex::new(vec![]));
            let block_index_requests = calls.clone();
            fake_gossip_server.set_on_block_index_request(move |query| {
                let mut calls_ref = calls.lock().unwrap();
                let calls_len = calls_ref.len();
                calls_ref.push(query);

                // Simulate a process needing to make two calls
                if calls_len == 0 {
                    GossipResponse::Accepted(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(1),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else if calls_len == 1 {
                    GossipResponse::Accepted(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(2),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else {
                    GossipResponse::Accepted(vec![])
                }
            });

            let fake_gossip_address = fake_gossip_server.spawn();
            let fake_peer_address = PeerAddress {
                gossip: fake_gossip_address,
                api: fake_gossip_address,
                execution: Default::default(),
            };

            let mut node_config = NodeConfig::testing();
            node_config.sync_mode = SyncMode::Full;
            node_config.trusted_peers = vec![fake_peer_address];
            node_config.genesis_peer_discovery_timeout_millis = 10;
            let config = Config::new_with_random_peer_id(node_config);

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();

            let tokio_handle = tokio::runtime::Handle::current();
            let reth_peer_sender = noop_reth_peer_sender();

            let (_peer_network_handle, peer_list_guard) = spawn_peer_network_service_with_client(
                db.clone(),
                &config,
                reth_peer_sender,
                receiver,
                sender,
                tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
                tokio_handle,
            );

            let fake_mining_addr = IrysAddress::repeat_byte(2);
            peer_list_guard.add_or_update_peer(
                PeerListItem {
                    peer_id: IrysPeerId::from(fake_mining_addr),
                    mining_address: fake_mining_addr,
                    reputation_score: PeerScore::new(100),
                    response_time: 0,
                    address: fake_peer_address,
                    last_seen: 0,
                    is_online: true,
                    protocol_version: Default::default(),
                },
                true,
            );

            let data_handler =
                data_handler_stub(&config, &peer_list_guard, db.clone(), sync_state.clone());

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                &peer_list_guard,
                10,
                &config,
                data_handler,
            )
            .await
            .expect("to finish catching up");

            // There should be three calls total: two that got items and one that didn't
            {
                let data_requests = block_index_requests.lock().unwrap();
                assert_eq!(data_requests.len(), 3);
                debug!("Data requests: {:?}", data_requests);
                assert_eq!(data_requests[0].height, 11);
                assert_eq!(data_requests[1].height, 11);
                assert_eq!(data_requests[0].limit, 50);
                assert_eq!(data_requests[1].limit, 50);
                assert_eq!(data_requests[2].height, 12);
                assert_eq!(data_requests[2].limit, 50);
            }

            // Check that the sync status has changed to synced
            assert!(!sync_state.is_syncing());

            // Wait for all block requests to complete (with timeout)
            let start = std::time::Instant::now();
            let timeout = Duration::from_secs(5);
            while start.elapsed() < timeout {
                let len = block_requests_clone.lock().unwrap().len();
                if len >= 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            let block_requests = block_requests_clone.lock().unwrap();
            // The first block should have been requested at least twice (initial + retry)
            let first_block_request_count = block_requests
                .iter()
                .filter(|&block_hash| block_hash == &BlockHash::repeat_byte(1))
                .count();
            let requested_second_block = block_requests
                .iter()
                .find(|&block_hash| block_hash == &BlockHash::repeat_byte(2));
            assert!(
                first_block_request_count >= 2,
                "Expected first block to be requested at least twice (retry), but was requested {} times",
                first_block_request_count
            );
            assert!(requested_second_block.is_some());

            Ok(())
        }

        #[tokio::test]
        async fn should_sync_and_change_status_for_the_non_zero_genesis_with_offline_peers(
        ) -> eyre::Result<()> {
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let start_from = 10;
            let sync_state = ChainSyncState::new(true, false);

            let db = DatabaseProvider(Arc::new(
                open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                    .expect("can't open temp dir"),
            ));

            let mut node_config = NodeConfig::testing();
            node_config.node_mode = NodeMode::Genesis;
            node_config.trusted_peers = vec![];
            node_config.genesis_peer_discovery_timeout_millis = 10;
            let config = Config::new_with_random_peer_id(node_config);

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let runtime_handle = tokio::runtime::Handle::current();
            let reth_peer_sender = noop_reth_peer_sender();

            let (_peer_network_handle, peer_list) = spawn_peer_network_service_with_client(
                db.clone(),
                &config,
                reth_peer_sender,
                receiver,
                sender,
                tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
                runtime_handle,
            );

            let fake_peer_address = PeerAddress {
                gossip: SocketAddr::from(([127, 0, 0, 1], 1279)),
                api: SocketAddr::from(([127, 0, 0, 1], 1270)),
                execution: Default::default(),
            };

            let fake_mining_addr = IrysAddress::repeat_byte(2);
            peer_list.add_or_update_peer(
                PeerListItem {
                    peer_id: IrysPeerId::from(fake_mining_addr),
                    mining_address: fake_mining_addr,
                    reputation_score: PeerScore::new(100),
                    response_time: 0,
                    address: fake_peer_address,
                    last_seen: 0,
                    is_online: true,
                    protocol_version: Default::default(),
                },
                true,
            );

            let data_handler = data_handler_stub(&config, &peer_list, db, sync_state.clone());

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                &peer_list,
                start_from,
                &config,
                data_handler,
            )
            .await
            .expect("to finish catching up");

            // Check that the sync status has changed to synced
            assert!(!sync_state.is_syncing());

            Ok(())
        }
    }

    mod post_sync_unique_highest_blocks {
        use super::*;
        use crate::tests::util::data_handler_stub;
        use crate::tests::util::FakeGossipServer;
        use crate::types::GossipResponse;
        use eyre::Result as EyreResult;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
        use irys_types::{
            Config, DatabaseProvider, IrysAddress, NodeConfig, NodeInfo, PeerAddress, PeerListItem,
            PeerNetworkSender, PeerScore, SyncMode,
        };
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};

        #[tokio::test]
        async fn should_retry_next_peer_for_same_hash_and_succeed() -> EyreResult<()> {
            let sync_state = ChainSyncState::new(true, false);
            let temp_block_hash = BlockHash::repeat_byte(9);

            // Fake servers: first returns None for block, second returns a block
            let server1 = FakeGossipServer::new();
            let server2 = FakeGossipServer::new();

            let s1_calls = Arc::new(Mutex::new(0_usize));
            let s2_calls = Arc::new(Mutex::new(0_usize));
            let s1_calls_clone = s1_calls.clone();
            server1.set_on_pull_data_request(move |req| match req {
                GossipDataRequestV2::BlockHeader(_hash) => {
                    let mut c = s1_calls_clone.lock().unwrap();
                    *c += 1;
                    GossipResponse::Accepted(None)
                }
                _ => GossipResponse::Accepted(None),
            });

            let s2_calls_clone = s2_calls.clone();
            server2.set_on_pull_data_request(move |req| match req {
                GossipDataRequestV2::BlockHeader(_hash) => {
                    let mut c = s2_calls_clone.lock().unwrap();
                    *c += 1;
                    GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::new(
                        irys_testing_utils::new_mock_signed_header(),
                    ))))
                }
                _ => GossipResponse::Accepted(None),
            });

            // Set info handler to return the same hash
            server1.set_on_info_request(move || {
                let info = NodeInfo {
                    block_hash: temp_block_hash,
                    ..NodeInfo::default()
                };
                GossipResponse::Accepted(info)
            });
            server2.set_on_info_request(move || {
                let info = NodeInfo {
                    block_hash: temp_block_hash,
                    ..NodeInfo::default()
                };
                GossipResponse::Accepted(info)
            });

            let addr1 = server1.spawn();
            let addr2 = server2.spawn();

            // Config and services
            let mut node_config = NodeConfig::testing();
            node_config.sync_mode = SyncMode::Full;
            let config = Config::new_with_random_peer_id(node_config);

            let retry_timeout = config.node_config.sync.retry_block_request_timeout_secs;

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir");
            let db = DatabaseProvider(Arc::new(db_env));

            let runtime_handle = tokio::runtime::Handle::current();
            let reth_peer_sender = noop_reth_peer_sender();

            let (_peer_network_handle, peer_list_guard) = spawn_peer_network_service_with_client(
                db.clone(),
                &config,
                reth_peer_sender,
                receiver,
                sender,
                tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
                runtime_handle,
            );

            // Add two peers reporting the same highest hash
            let addr_a = IrysAddress::repeat_byte(0xA0);
            let addr_b = IrysAddress::repeat_byte(0xB0);
            let peer1 = PeerListItem {
                peer_id: IrysPeerId::from(addr_a),
                mining_address: addr_a,
                reputation_score: PeerScore::new(100),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr1,
                    api: addr1,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
                protocol_version: Default::default(),
            };
            let peer2 = PeerListItem {
                peer_id: IrysPeerId::from(addr_b),
                mining_address: addr_b,
                reputation_score: PeerScore::new(99),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr2,
                    api: addr2,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
                protocol_version: Default::default(),
            };

            peer_list_guard.add_or_update_peer(peer1, true);
            peer_list_guard.add_or_update_peer(peer2, true);

            // Build data handler
            let db = db.clone();
            let data_handler = data_handler_stub(&config, &peer_list_guard, db, sync_state.clone());

            // Execute helper
            pull_unique_highest_blocks::<_, _>(
                &peer_list_guard,
                &data_handler.gossip_client,
                data_handler.clone(),
                false,
                retry_timeout,
            )
            .await?;

            // Validate retries occurred: first peer attempted, second peer attempted and succeeded
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                *s1_calls.lock().unwrap() >= 1,
                "expected the first peer to be attempted"
            );
            assert!(
                *s2_calls.lock().unwrap() >= 1,
                "expected retry to second peer"
            );
            Ok(())
        }

        #[tokio::test]
        async fn should_try_all_peers_when_all_fail() -> EyreResult<()> {
            let sync_state = ChainSyncState::new(true, false);
            let temp_block_hash = BlockHash::repeat_byte(7);

            let server1 = FakeGossipServer::new();
            let server2 = FakeGossipServer::new();
            let s1_calls = Arc::new(Mutex::new(0_usize));
            let s2_calls = Arc::new(Mutex::new(0_usize));
            let s1_calls_clone = s1_calls.clone();
            server1.set_on_pull_data_request(move |req| match req {
                GossipDataRequestV2::BlockHeader(_hash) => {
                    *s1_calls_clone.lock().unwrap() += 1;
                    GossipResponse::Accepted(None)
                }
                _ => GossipResponse::Accepted(None),
            });
            let s2_calls_clone = s2_calls.clone();
            server2.set_on_pull_data_request(move |req| match req {
                GossipDataRequestV2::BlockHeader(_hash) => {
                    *s2_calls_clone.lock().unwrap() += 1;
                    GossipResponse::Accepted(None)
                }
                _ => GossipResponse::Accepted(None),
            });

            server1.set_on_info_request(move || {
                let info = NodeInfo {
                    block_hash: temp_block_hash,
                    ..NodeInfo::default()
                };
                GossipResponse::Accepted(info)
            });
            server2.set_on_info_request(move || {
                let info = NodeInfo {
                    block_hash: temp_block_hash,
                    ..NodeInfo::default()
                };
                GossipResponse::Accepted(info)
            });

            let addr1: SocketAddr = server1.spawn();
            let addr2: SocketAddr = server2.spawn();

            let mut node_config = NodeConfig::testing();
            node_config.sync_mode = SyncMode::Full;
            let config = Config::new_with_random_peer_id(node_config);
            let retry_timeout = config.node_config.sync.retry_block_request_timeout_secs;

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir");
            let db = DatabaseProvider(Arc::new(db_env));

            let runtime_handle = tokio::runtime::Handle::current();
            let reth_peer_sender = noop_reth_peer_sender();

            let (_peer_network_handle, peer_list_guard) = spawn_peer_network_service_with_client(
                db.clone(),
                &config,
                reth_peer_sender,
                receiver,
                sender,
                tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
                runtime_handle,
            );

            let addr_a = IrysAddress::repeat_byte(0xA1);
            let addr_b = IrysAddress::repeat_byte(0xB1);
            let peer1 = PeerListItem {
                peer_id: IrysPeerId::from(addr_a),
                mining_address: addr_a,
                reputation_score: PeerScore::new(100),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr1,
                    api: addr1,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
                protocol_version: Default::default(),
            };
            let peer2 = PeerListItem {
                peer_id: IrysPeerId::from(addr_b),
                mining_address: addr_b,
                reputation_score: PeerScore::new(99),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr2,
                    api: addr2,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
                protocol_version: Default::default(),
            };
            peer_list_guard.add_or_update_peer(peer1, true);
            peer_list_guard.add_or_update_peer(peer2, true);

            let db = db.clone();
            let data_handler = data_handler_stub(&config, &peer_list_guard, db, sync_state.clone());

            pull_unique_highest_blocks::<_, _>(
                &peer_list_guard,
                &data_handler.gossip_client,
                data_handler.clone(),
                false,
                retry_timeout,
            )
            .await?;

            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                *s1_calls.lock().unwrap() >= 1 && *s2_calls.lock().unwrap() >= 1,
                "expected both peers to be attempted"
            );
            Ok(())
        }
    }
}
