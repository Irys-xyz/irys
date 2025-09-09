use crate::gossip_data_handler::GossipDataHandler;
use crate::{BlockPool, GossipError, GossipResult};
use actix::Addr;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::reth_service::RethServiceActor;
use irys_actors::MempoolFacade;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{BlockIndexReadGuard, PeerList};
use irys_types::{
    Address, BlockHash, BlockIndexItem, BlockIndexQuery, Config, EvmBlockHash, NodeMode,
    PeerListItem, SyncMode, TokioServiceHandle,
};
use rand::prelude::SliceRandom as _;
use reth::tasks::shutdown::Shutdown;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

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
            GossipError::Network(msg) => Self::Network(msg),
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

const BLOCK_BATCH_SIZE: usize = 10;
const PERIODIC_SYNC_CHECK_INTERVAL_SECS: u64 = 30; // Check every 30 seconds if we're behind
const RETRY_BLOCK_REQUEST_TIMEOUT_SECS: u64 = 30; // Timeout for retry block pull/process

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
}

/// Inner service containing the sync logic
#[derive(Debug, Clone)]
pub struct ChainSyncServiceInner<A: ApiClient, B: BlockDiscoveryFacade, M: MempoolFacade> {
    sync_state: ChainSyncState,
    api_client: A,
    peer_list: PeerList,
    config: Config,
    block_index: BlockIndexReadGuard,
    block_pool: Arc<BlockPool<B, M>>,
    /// This field signifies when the sync task is already spawned, but the sync has not started yet.
    ///  The time gap between spawning the task and starting the sync can be significant. The node
    ///  needs to fetch the tip of the block index from the network to figure out how
    ///  much behind the network we are, if at all.
    is_sync_task_spawned: Arc<AtomicBool>,
    gossip_data_handler: Arc<GossipDataHandler<M, B, A>>,
    reth_service_actor: Option<Addr<RethServiceActor>>,
    /// An atomic bool to enable or disable VDF mining when sync is in progress
    is_vdf_mining_enabled: Arc<AtomicBool>,
}

/// Main sync service that runs in its own tokio task
#[derive(Debug)]
pub struct ChainSyncService<T: ApiClient, B: BlockDiscoveryFacade, M: MempoolFacade> {
    shutdown: Shutdown,
    msg_rx: mpsc::UnboundedReceiver<SyncChainServiceMessage>,
    inner: ChainSyncServiceInner<T, B, M>,
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

impl<B: BlockDiscoveryFacade, M: MempoolFacade> ChainSyncServiceInner<IrysApiClient, B, M> {
    pub fn new(
        sync_state: ChainSyncState,
        peer_list: PeerList,
        config: irys_types::Config,
        block_index: BlockIndexReadGuard,
        block_pool: Arc<BlockPool<B, M>>,
        gossip_data_handler: Arc<GossipDataHandler<M, B, IrysApiClient>>,
        reth_service_actor: Option<Addr<RethServiceActor>>,
        is_vdf_mining_enabled: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_client(
            sync_state,
            IrysApiClient::new(),
            peer_list,
            config,
            block_index,
            block_pool,
            gossip_data_handler,
            reth_service_actor,
            is_vdf_mining_enabled,
        )
    }
}

impl<A: ApiClient, B: BlockDiscoveryFacade, M: MempoolFacade> ChainSyncServiceInner<A, B, M> {
    pub fn new_with_client(
        sync_state: ChainSyncState,
        api_client: A,
        peer_list: PeerList,
        config: irys_types::Config,
        block_index: BlockIndexReadGuard,
        block_pool: Arc<BlockPool<B, M>>,
        gossip_data_handler: Arc<GossipDataHandler<M, B, A>>,
        reth_service_actor: Option<Addr<RethServiceActor>>,
        is_vdf_mining_enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sync_state,
            api_client,
            peer_list,
            config,
            block_index,
            block_pool,
            is_sync_task_spawned: Arc::new(AtomicBool::new(false)),
            gossip_data_handler,
            reth_service_actor,
            is_vdf_mining_enabled,
        }
    }

    /// Perform a sync operation
    async fn spawn_chain_sync_task(
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

        self.is_sync_task_spawned.store(true, Ordering::Relaxed);

        let start_sync_from_height = self.block_index.read().latest_height();
        // Clear any pending blocks from the cache
        self.block_pool.block_cache_guard().clear().await;

        let config = self.config.clone();
        let peer_list = self.peer_list.clone();
        let sync_state = self.sync_state.clone();
        let api_client = self.api_client.clone();
        let gossip_data_handler = self.gossip_data_handler.clone();
        let is_sync_task_spawned = self.is_sync_task_spawned.clone();
        let block_pool = self.block_pool.clone();
        let reth_service_addr = self.reth_service_actor.clone();
        let is_vdf_mining_enabled = Arc::clone(&self.is_vdf_mining_enabled);

        tokio::spawn(
            async move {
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
                    .repair_missing_payloads_if_any(reth_service_addr)
                    .await
                {
                    error!(
                        "Sync task: Failed to repair missing payloads before starting sync: {:?}",
                        err
                    );
                }

                let res = sync_chain(
                    sync_state.clone(),
                    api_client,
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
                    orphaned_block.header.block_hash
                );
                let block_pool = self.block_pool.clone();
                let header = orphaned_block.header;
                let is_fast_tracking = orphaned_block.is_fast_tracking;
                futures.push(async move {
                    block_pool
                        .process_block(header, is_fast_tracking)
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
            self.block_pool.remove_requested_block(&block_hash).await;
            self.block_pool.remove_block_from_cache(&block_hash).await;
            Err(ChainSyncError::Internal(format!(
                "Network error: {:?}",
                err
            )))
        } else {
            Ok(())
        }
    }
}

impl<T: ApiClient, B: BlockDiscoveryFacade, M: MempoolFacade> ChainSyncService<T, B, M> {
    #[tracing::instrument(skip_all)]
    pub fn spawn_service(
        inner: ChainSyncServiceInner<T, B, M>,
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

    #[tracing::instrument(skip_all)]
    async fn start(mut self) -> ChainSyncResult<()> {
        info!("Starting sync service");

        // Set up periodic sync check timer
        let mut periodic_timer = interval(Duration::from_secs(PERIODIC_SYNC_CHECK_INTERVAL_SECS));
        periodic_timer.tick().await; // Consume the first immediate tick

        loop {
            tokio::select! {
                biased; // enable bias so polling happens in definition order

                // Check for shutdown signal
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

    #[tracing::instrument(skip_all)]
    async fn handle_message(&self, msg: SyncChainServiceMessage) -> ChainSyncResult<()> {
        match msg {
            SyncChainServiceMessage::InitialSync(response_sender) => {
                self.inner
                    .spawn_chain_sync_task(Some(response_sender), true)
                    .await;
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
            &self.inner.api_client,
            &self.inner.block_index,
        )
        .await
        {
            Ok(true) => {
                info!("Periodic sync check: We're behind the network, starting sync");
                self.inner.spawn_chain_sync_task(None, false).await;
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

#[instrument(skip_all, err)]
async fn sync_chain<B: BlockDiscoveryFacade, M: MempoolFacade, A: ApiClient>(
    sync_state: ChainSyncState,
    api_client: impl ApiClient,
    peer_list: &PeerList,
    mut start_sync_from_height: usize,
    config: &irys_types::Config,
    gossip_data_handler: Arc<GossipDataHandler<M, B, A>>,
) -> ChainSyncResult<()> {
    let sync_mode = config.node_config.sync_mode;
    let genesis_peer_discovery_timeout_millis =
        config.node_config.genesis_peer_discovery_timeout_millis;
    // Check if gossip reception is enabled before starting sync
    if !sync_state.is_gossip_reception_enabled() {
        debug!("Sync task: Gossip reception is disabled, skipping sync");
        sync_state.finish_sync();
        return Ok(());
    }

    // If the peer doesn't have any blocks, it should start syncing from 1, as the genesis block
    // should always be present
    if start_sync_from_height == 0 {
        start_sync_from_height = 1;
    }
    let is_trusted_mode = matches!(sync_mode, SyncMode::Trusted);

    sync_state.set_syncing_from(start_sync_from_height);
    sync_state.set_trusted_sync(is_trusted_mode);

    let is_a_genesis_node = matches!(config.node_config.node_mode, NodeMode::Genesis);
    if is_a_genesis_node && sync_state.sync_target_height() <= 1 {
        debug!("Sync task: The node is a genesis node with no blocks, skipping the sync task");
        sync_state.finish_sync();
        return Ok(());
    }

    if matches!(sync_mode, SyncMode::Trusted) {
        sync_state.set_trusted_sync(true);
    } else {
        sync_state.set_trusted_sync(false);
    }

    debug!("Sync task: Starting a chain sync task, waiting for active peers. Mode: {:?}, starting from height: {}, trusted mode: {}", sync_mode, start_sync_from_height, sync_state.is_trusted_sync());

    if is_a_genesis_node {
        warn!("Sync task: Because the node is a genesis node, waiting for active peers for {}, and if no peers are added, then skipping the sync task", genesis_peer_discovery_timeout_millis);
        match timeout(
            Duration::from_millis(genesis_peer_discovery_timeout_millis),
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
                return Ok(());
            }
        };
    } else {
        sync_state.set_is_syncing(true);
        peer_list.wait_for_active_peers().await;
    }

    debug!("Sync task: Syncing started");

    if is_trusted_mode {
        let trusted_peers = peer_list.online_trusted_peers();
        if trusted_peers.is_empty() {
            return if is_a_genesis_node {
                sync_state.mark_processed(sync_state.sync_target_height());
                sync_state.finish_sync();
                Ok(())
            } else {
                Err(ChainSyncError::Network(
                    "No trusted peers available".to_string(),
                ))
            };
        }

        if let Err(err) = check_and_update_full_validation_switch_height(
            config,
            peer_list,
            &api_client,
            &sync_state,
        )
        .await
        {
            if is_a_genesis_node {
                warn!("Since the node is a genesis node, skipping the sync task due to being unable to verify the full validation switch height from trusted peers: {}", err);
            } else {
                return Err(err);
            }
        }
    }

    let block_index = match get_block_index(
        peer_list,
        &api_client,
        sync_state.sync_target_height(),
        BLOCK_BATCH_SIZE,
        5,
        is_trusted_mode,
    )
    .await
    {
        Ok(index) => {
            debug!("Sync task: Fetched block index: {:?}", index);
            index
        }
        Err(err) => {
            error!("Sync task: Failed to fetch block index: {}", err);
            if is_a_genesis_node {
                warn!("Sync task: Because the node is a genesis node, skipping the sync task due to being unable to fetch the index from peers");
                vec![]
            } else {
                return Err(err);
            }
        }
    };

    let mut blocks_to_request = block_index.len();

    let mut block_queue = VecDeque::new();
    block_queue.extend(block_index);

    // If no new blocks were added to the index, nothing is going to mark
    //  the tip as processed
    if block_queue.is_empty() {
        debug!("Sync task: No new blocks to process, marking the current sync target height as processed");
        sync_state.mark_processed(sync_state.sync_target_height());
        sync_state.finish_sync();
    } else {
        sync_state.set_is_syncing(true);
    }

    let mut sync_target = sync_state.sync_target_height() + block_queue.len();
    sync_state.set_sync_target_height(sync_target);

    while let Some(block) = block_queue.pop_front() {
        if sync_state.is_queue_full() {
            debug!("Sync task: Block queue is full, waiting for an empty slot");

            // Retry logic for wait_for_an_empty_queue_slot
            let retry_attempts = 3;
            let mut wait_success = false;

            for attempt in 1..=retry_attempts {
                debug!("Sync task: Wait attempt {} for empty queue slot", attempt);

                match sync_state.wait_for_an_empty_queue_slot().await {
                    Ok(()) => {
                        debug!(
                            "Sync task: Queue slot became available on attempt {}",
                            attempt
                        );
                        wait_success = true;
                        break;
                    }
                    Err(_) => {
                        warn!(
                            "Sync task: Timeout on attempt {} waiting for queue slot",
                            attempt
                        );

                        if attempt < retry_attempts {
                            // Try to request the block at height = last synced + 1 to trigger processing
                            let retry_height = sync_state.highest_processed_block() + 1;
                            debug!("Sync task: Attempting to request block at height {} to trigger processing", retry_height);

                            match get_block_index(
                                peer_list,
                                &api_client,
                                retry_height,
                                1, // Just get one block
                                3, // 3 retries for the network call
                                is_trusted_mode,
                            )
                            .await
                            {
                                Ok(retry_index) if !retry_index.is_empty() => {
                                    let retry_block = &retry_index[0];
                                    debug!("Sync task: Got to retry block {:?} for height {}, requesting from network", retry_block.block_hash, retry_height);

                                    // Try to reprocess the last prevalidated block
                                    match timeout(
                                        Duration::from_secs(RETRY_BLOCK_REQUEST_TIMEOUT_SECS),
                                        gossip_data_handler.pull_and_process_block(
                                            retry_block.block_hash,
                                            sync_state.is_syncing_from_a_trusted_peer(),
                                        ),
                                    )
                                    .await
                                    {
                                        Ok(Ok(())) => {
                                            debug!("Sync task: Successfully requested retry block {:?} from network", retry_block.block_hash);
                                        }
                                        Ok(Err(e)) => {
                                            warn!("Sync task: Failed to request retry block {:?} from network: {}", retry_block.block_hash, e);
                                        }
                                        Err(_) => {
                                            warn!("Sync task: Timeout ({:?}s) while requesting retry block {:?} from network", RETRY_BLOCK_REQUEST_TIMEOUT_SECS, retry_block.block_hash);
                                        }
                                    }
                                }
                                Ok(_) => {
                                    warn!("Sync task: Retry attempt returned empty index for height {}", retry_height);
                                }
                                Err(e) => {
                                    warn!("Sync task: Failed to get retry block index for height {}: {}", retry_height, e);
                                }
                            }

                            // Wait a bit before next retry
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                        }
                    }
                }
            }

            if !wait_success {
                error!("Sync task: All wait attempts failed, exiting sync");
                return Err(ChainSyncError::Internal(
                    "Failed to get queue slot after timeout and retries".to_string(),
                ));
            }
        }

        let sync_state_clone = sync_state.clone();
        let data_handler = gossip_data_handler.clone();
        let block_hash = block.block_hash;

        tokio::spawn(
            async move {
                debug!(
                    "Sync task: Requesting block {:?} (sync target height is {}) from the network",
                    block_hash,
                    sync_state_clone.sync_target_height()
                );

                match data_handler
                    .pull_and_process_block(
                        block_hash,
                        sync_state_clone.is_syncing_from_a_trusted_peer(),
                    )
                    .await
                {
                    Ok(()) => {
                        debug!("Sync task: Successfully processed block {:?}", block_hash);
                    }
                    Err(e) => {
                        error!("Sync task: Failed to process block {:?}: {}", block_hash, e);
                        // Don't need to retry here, the code at the beginning of the loop will handle it
                        // by requesting the block again if needed
                    }
                }
            }
            .in_current_span(),
        );

        blocks_to_request -= 1;
        if blocks_to_request == 0 {
            if is_trusted_mode {
                check_and_update_full_validation_switch_height(
                    config,
                    peer_list,
                    &api_client,
                    &sync_state,
                )
                .await?;
            }

            let additional_index = get_block_index(
                peer_list,
                &api_client,
                sync_target,
                BLOCK_BATCH_SIZE,
                5,
                is_trusted_mode,
            )
            .await?;

            sync_target += additional_index.len();
            sync_state.set_sync_target_height(sync_target);

            block_queue.extend(additional_index);
            blocks_to_request = block_queue.len();
            if blocks_to_request == 0 {
                debug!(
                    "block index request for entries >{} returned no extra results",
                    &sync_target
                );
                break;
            }
        }
    }

    debug!("Sync task: Block queue is empty, waiting for the highest processed block to reach the target sync height");
    sync_state
        .wait_for_processed_block_to_reach_target()
        .in_current_span()
        .await;
    sync_state.finish_sync();

    // After finishing primary sync, best-effort pull of all unique highest blocks announced by peers.
    // This uses the same timeout logic as the retry path in the main sync loop.
    match pull_unique_highest_blocks(
        peer_list,
        &api_client,
        gossip_data_handler.clone(),
        is_trusted_mode,
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

    info!("Sync task: Gossip service sync task completed");
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
    api_client: &impl ApiClient,
    use_trusted_peers_only: bool,
    top_n: Option<usize>,
) -> ChainSyncResult<HashMap<BlockHash, (u64, Vec<(Address, PeerListItem)>)>> {
    // Pick peers: trusted or top N active
    let peers: Vec<(Address, irys_types::PeerListItem)> = if use_trusted_peers_only {
        debug!("Collecting highest blocks from trusted peers");
        peer_list.online_trusted_peers()
    } else {
        let limit = top_n.unwrap_or(10);
        debug!("Collecting highest blocks from top {} active peers", limit);
        peer_list.top_active_peers(Some(limit), None)
    };

    if peers.is_empty() {
        return Err(ChainSyncError::Network(
            "No online peers available".to_string(),
        ));
    }

    let mut peers_by_top_block_hash: HashMap<BlockHash, (u64, Vec<(Address, PeerListItem)>)> =
        HashMap::new();

    for (miner_address, peer) in peers {
        match api_client.node_info(peer.address.api).await {
            Ok(info) => {
                let hash = info.block_hash;
                let height = info.block_index_height;
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
                    "Peer {:?} reported highest block hash {:?} (api: {})",
                    miner_address, hash, peer.address.api
                );
            }
            Err(err) => {
                warn!(
                    "Failed to fetch node info from peer {:?} (api: {}): {}",
                    miner_address, peer.address.api, err
                );
            }
        }
    }

    Ok(peers_by_top_block_hash)
}

/// Collects unique highest block hashes from peers and pulls each of them in parallel.
/// Each pull uses the same timeout logic as used elsewhere in the sync task.
async fn pull_unique_highest_blocks<B: BlockDiscoveryFacade, M: MempoolFacade, A: ApiClient>(
    peer_list: &PeerList,
    api_client: &impl ApiClient,
    gossip_data_handler: Arc<GossipDataHandler<M, B, A>>,
    use_trusted_peers_only: bool,
) -> ChainSyncResult<()> {
    // Collect unique hashes with their reporting peers (trusted or top-N active peers)
    let peers_by_top_block_hash =
        pull_highest_blocks(peer_list, api_client, use_trusted_peers_only, None).await?;

    if peers_by_top_block_hash.is_empty() {
        debug!("Post-sync: No unique highest blocks reported by peers");
        return Ok(());
    }

    let mut futures = Vec::new();
    for (block_hash, (reported_height, peers)) in peers_by_top_block_hash {
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
                    Duration::from_secs(RETRY_BLOCK_REQUEST_TIMEOUT_SECS),
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
                            attempt_num, RETRY_BLOCK_REQUEST_TIMEOUT_SECS, block_hash, peer.0
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
    api_client: &impl ApiClient,
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
            let node_info = match api_client.node_info(peer.address.api).await {
                Ok(info) => info,
                Err(err) => {
                    warn!(
                            "Sync task: Failed to fetch node info from trusted peer {}: {}, trying another peer",
                            peer.address.api, err
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

async fn get_block_index(
    peer_list: &PeerList,
    api_client: &impl ApiClient,
    start: usize,
    limit: usize,
    retries: usize,
    fetch_from_the_trusted_peer: bool,
) -> ChainSyncResult<Vec<BlockIndexItem>> {
    debug!(
        "Fetching block index starting from height {} with limit {}",
        start, limit
    );
    let mut peers_to_fetch_index_from = if fetch_from_the_trusted_peer {
        debug!("Fetching block index from trusted peers");
        peer_list.online_trusted_peers()
    } else {
        debug!("Fetching block index from top active peers");
        peer_list.top_active_peers(Some(5), None)
    };

    if peers_to_fetch_index_from.is_empty() {
        return Err(ChainSyncError::Network("No peers available".to_string()));
    }

    // the peer to remove from the candidate list of peers
    // this is so we don't have conflicting mutable and immutable borrows
    let mut to_remove = None;

    for _ in 0..retries {
        if let Some(to_remove) = to_remove {
            peers_to_fetch_index_from.retain(|peer| peer.0 != to_remove);
        };

        let (miner_address, top_peer) =
            peers_to_fetch_index_from
                .choose(&mut rand::thread_rng())
                .ok_or(GossipError::Network("No peers available".to_string()))?;

        let should_remove = match api_client
            .node_info(top_peer.address.api)
            .await
            .map_err(|network_error| GossipError::Network(network_error.to_string()))
        {
            Ok(info) => {
                if info.is_syncing {
                    info!(
                        "Peer {} is syncing, skipping for block index fetch",
                        &miner_address
                    );

                    true
                } else {
                    false
                }
            }
            Err(error) => {
                error!(
                    "Failed to fetch node info from peer {:?}: {:?}",
                    miner_address, error
                );
                true
            }
        };

        if should_remove {
            // this is so we don't have conflicting mutable and immutable borrows
            to_remove = Some(*miner_address);
            continue;
        }

        match api_client
            .get_block_index(
                top_peer.address.api,
                BlockIndexQuery {
                    height: start,
                    limit,
                },
            )
            .await
            .map_err(|network_error| ChainSyncError::Network(network_error.to_string()))
        {
            Ok(index) => {
                debug!(
                    "Fetched block index from peer {:?}: {:?}",
                    miner_address, index
                );
                return Ok(index);
            }
            Err(error) => {
                error!(
                    "Failed to fetch block index from peer {:?}: {:?}",
                    miner_address, error
                );
                continue;
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
    api_client: &impl ApiClient,
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
        let node_info = match api_client.node_info(peer.address.api).await {
            Ok(info) => info,
            Err(err) => {
                warn!("Sync task: Failed to fetch node info from trusted peer {}: {}, trying another peer", peer.address.api, err);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::util::{ApiClientStub, FakeGossipServer, MockRethServiceActor};
    use irys_types::BlockHash;

    mod catch_up_task {
        use super::*;
        use crate::peer_network_service::PeerNetworkService;
        use crate::tests::util::data_handler_stub;
        use crate::types::GossipResponse;
        use crate::GetPeerListGuard;
        use actix::Actor as _;
        use eyre::eyre;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::{
            Address, Config, DatabaseProvider, GossipData, GossipDataRequest, IrysBlockHeader,
            NodeConfig, PeerAddress, PeerListItem, PeerNetworkSender, PeerScore,
        };
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex, RwLock};

        #[actix_web::test]
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
                    GossipDataRequest::ExecutionPayload(_) => GossipResponse::Accepted(None),
                    GossipDataRequest::Block(block_hash) => {
                        info!("Fake server pull data request: {block_hash:?}");
                        let mut block_requests = block_requests.lock().unwrap();
                        let requests_len = block_requests.len();
                        block_requests.push(block_hash);

                        // Simulating one false response so the block gets requested again
                        if requests_len == 0 {
                            GossipResponse::Accepted(None)
                        } else {
                            sync_state_clone.mark_processed(start_from + requests_len);
                            GossipResponse::Accepted(Some(GossipData::Block(Arc::new(
                                IrysBlockHeader::new_mock_header(),
                            ))))
                        }
                    }
                    GossipDataRequest::Chunk(_) => GossipResponse::Accepted(None),
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
            let config = Config::new(node_config);

            let api_client_stub = ApiClientStub::new();
            let calls = Arc::new(Mutex::new(vec![]));
            let block_index_requests = calls.clone();
            api_client_stub.set_block_index_handler(move |query| {
                let mut calls_ref = calls.lock().unwrap();
                let calls_len = calls_ref.len();
                calls_ref.push(query);

                // Simulate a process needing to make two calls
                if calls_len == 0 {
                    Ok(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(1),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else if calls_len == 1 {
                    Ok(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(2),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else {
                    Ok(vec![])
                }
            });

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();

            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();
            let peer_list_service = PeerNetworkService::new_with_custom_api_client(
                db.clone(),
                &config,
                api_client_stub.clone(),
                reth_mock_addr.clone(),
                receiver,
                sender,
            );
            let peer_service_addr = peer_list_service.start();
            let peer_list_guard = peer_service_addr
                .send(GetPeerListGuard)
                .await
                .expect("to get a peer list")
                .expect("to get a peer list");

            peer_list_guard.add_or_update_peer(
                Address::repeat_byte(2),
                PeerListItem {
                    reputation_score: PeerScore::new(100),
                    response_time: 0,
                    address: fake_peer_address,
                    last_seen: 0,
                    is_online: true,
                },
                true,
            );

            let data_handler = data_handler_stub(
                &config,
                &peer_list_guard,
                db.clone(),
                api_client_stub.clone(),
                sync_state.clone(),
            )
            .await;

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                api_client_stub.clone(),
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
                assert_eq!(data_requests[0].height, 10);
                assert_eq!(data_requests[1].height, 11);
                assert_eq!(data_requests[0].limit, 10);
                assert_eq!(data_requests[1].limit, 10);
                assert_eq!(data_requests[2].height, 12);
                assert_eq!(data_requests[2].limit, 10);
            }

            // Check that the sync status has changed to synced
            assert!(!sync_state.is_syncing());

            tokio::time::sleep(Duration::from_millis(100)).await;

            let block_requests = block_requests_clone.lock().unwrap();
            assert_eq!(block_requests.len(), 3);
            let requested_first_block = block_requests
                .iter()
                .find(|&block_hash| block_hash == &BlockHash::repeat_byte(1));
            let requested_first_block_again = block_requests
                .iter()
                .find(|&block_hash| block_hash == &BlockHash::repeat_byte(1));
            let requested_second_block = block_requests
                .iter()
                .find(|&block_hash| block_hash == &BlockHash::repeat_byte(2));
            assert!(requested_first_block.is_some());
            // As the first call didn't return anything, the peer tries to fetch it once again
            assert!(requested_first_block_again.is_some());
            assert!(requested_second_block.is_some());

            Ok(())
        }

        #[actix_web::test]
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
            let config = Config::new(node_config);

            let api_client_stub = ApiClientStub {
                txs: Default::default(),
                block_index_handler: Arc::new(RwLock::new(Box::new(
                    move |_query: BlockIndexQuery| Err(eyre!("Simulating index request error")),
                ))),
                block_index_calls: Arc::new(Default::default()),
                node_info_handler: Arc::new(RwLock::new(Box::new(|_| {
                    Ok(irys_types::NodeInfo::default())
                }))),
            };

            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let peer_list_service = PeerNetworkService::new_with_custom_api_client(
                db.clone(),
                &config,
                api_client_stub.clone(),
                reth_mock_addr.clone(),
                receiver,
                sender,
            );

            let fake_peer_address = PeerAddress {
                gossip: SocketAddr::from(([127, 0, 0, 1], 1279)),
                api: SocketAddr::from(([127, 0, 0, 1], 1270)),
                execution: Default::default(),
            };

            let peer_service_addr = peer_list_service.start();
            let peer_list = peer_service_addr
                .send(GetPeerListGuard)
                .await
                .expect("to get peer list")
                .expect("to get peer list");

            peer_list.add_or_update_peer(
                Address::repeat_byte(2),
                PeerListItem {
                    reputation_score: PeerScore::new(100),
                    response_time: 0,
                    address: fake_peer_address,
                    last_seen: 0,
                    is_online: true,
                },
                true,
            );

            let data_handler = data_handler_stub(
                &config,
                &peer_list,
                db,
                api_client_stub.clone(),
                sync_state.clone(),
            )
            .await;

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                api_client_stub.clone(),
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
        use crate::peer_network_service::PeerNetworkService;
        use crate::tests::util::data_handler_stub;
        use crate::tests::util::{ApiClientStub, FakeGossipServer, MockRethServiceActor};
        use crate::types::GossipResponse;
        use crate::GetPeerListGuard;
        use actix::Actor as _;
        use eyre::Result as EyreResult;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::{
            Address, Config, DatabaseProvider, GossipData, GossipDataRequest, IrysBlockHeader,
            NodeConfig, NodeInfo, PeerAddress, PeerListItem, PeerNetworkSender, PeerScore,
            SyncMode,
        };
        use std::net::SocketAddr;
        use std::sync::{Arc, Mutex};

        #[actix_web::test]
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
                GossipDataRequest::Block(_hash) => {
                    let mut c = s1_calls_clone.lock().unwrap();
                    *c += 1;
                    GossipResponse::Accepted(None)
                }
                _ => GossipResponse::Accepted(None),
            });

            let s2_calls_clone = s2_calls.clone();
            server2.set_on_pull_data_request(move |req| match req {
                GossipDataRequest::Block(_hash) => {
                    let mut c = s2_calls_clone.lock().unwrap();
                    *c += 1;
                    GossipResponse::Accepted(Some(GossipData::Block(Arc::new(
                        IrysBlockHeader::new_mock_header(),
                    ))))
                }
                _ => GossipResponse::Accepted(None),
            });

            let addr1 = server1.spawn();
            let addr2 = server2.spawn();

            // Config and services
            let mut node_config = NodeConfig::testing();
            node_config.sync_mode = SyncMode::Full;
            let config = Config::new(node_config);

            let api_client_stub = ApiClientStub::new();
            api_client_stub.set_node_info_handler(move |_api| {
                let info = NodeInfo {
                    block_hash: temp_block_hash,
                    ..NodeInfo::default()
                };
                Ok(info)
            });

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir");
            let db = DatabaseProvider(Arc::new(db_env));
            let peer_service = PeerNetworkService::new_with_custom_api_client(
                db.clone(),
                &config,
                api_client_stub.clone(),
                reth_mock_addr.clone(),
                receiver,
                sender,
            );
            let peer_service_addr = peer_service.start();
            let peer_list_guard = peer_service_addr
                .send(GetPeerListGuard)
                .await
                .expect("peer list")
                .expect("peer list");

            // Add two peers reporting the same highest hash
            let peer1 = PeerListItem {
                reputation_score: PeerScore::new(100),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr1,
                    api: addr1,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
            };
            let peer2 = PeerListItem {
                reputation_score: PeerScore::new(99),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr2,
                    api: addr2,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
            };

            let addr_a = Address::repeat_byte(0xA0);
            let addr_b = Address::repeat_byte(0xB0);
            peer_list_guard.add_or_update_peer(addr_a, peer1, true);
            peer_list_guard.add_or_update_peer(addr_b, peer2, true);

            // Build data handler
            let db = db.clone();
            let data_handler = data_handler_stub(
                &config,
                &peer_list_guard,
                db,
                api_client_stub.clone(),
                sync_state.clone(),
            )
            .await;

            // Execute helper
            pull_unique_highest_blocks::<_, _, _>(
                &peer_list_guard,
                &api_client_stub,
                data_handler,
                false,
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

        #[actix_web::test]
        async fn should_try_all_peers_when_all_fail() -> EyreResult<()> {
            let sync_state = ChainSyncState::new(true, false);
            let temp_block_hash = BlockHash::repeat_byte(7);

            let server1 = FakeGossipServer::new();
            let server2 = FakeGossipServer::new();
            let s1_calls = Arc::new(Mutex::new(0_usize));
            let s2_calls = Arc::new(Mutex::new(0_usize));
            let s1_calls_clone = s1_calls.clone();
            server1.set_on_pull_data_request(move |req| match req {
                GossipDataRequest::Block(_hash) => {
                    *s1_calls_clone.lock().unwrap() += 1;
                    GossipResponse::Accepted(None)
                }
                _ => GossipResponse::Accepted(None),
            });
            let s2_calls_clone = s2_calls.clone();
            server2.set_on_pull_data_request(move |req| match req {
                GossipDataRequest::Block(_hash) => {
                    *s2_calls_clone.lock().unwrap() += 1;
                    GossipResponse::Accepted(None)
                }
                _ => GossipResponse::Accepted(None),
            });

            let addr1: SocketAddr = server1.spawn();
            let addr2: SocketAddr = server2.spawn();

            let mut node_config = NodeConfig::testing();
            node_config.sync_mode = SyncMode::Full;
            let config = Config::new(node_config);

            let api_client_stub = ApiClientStub::new();
            api_client_stub.set_node_info_handler(move |_api| {
                let info = NodeInfo {
                    block_hash: temp_block_hash,
                    ..NodeInfo::default()
                };
                Ok(info)
            });

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir");
            let db = DatabaseProvider(Arc::new(db_env));
            let peer_service = PeerNetworkService::new_with_custom_api_client(
                db.clone(),
                &config,
                api_client_stub.clone(),
                reth_mock_addr.clone(),
                receiver,
                sender,
            );
            let peer_service_addr = peer_service.start();
            let peer_list_guard = peer_service_addr
                .send(GetPeerListGuard)
                .await
                .expect("peer list")
                .expect("peer list");

            let peer1 = PeerListItem {
                reputation_score: PeerScore::new(100),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr1,
                    api: addr1,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
            };
            let peer2 = PeerListItem {
                reputation_score: PeerScore::new(99),
                response_time: 0,
                address: PeerAddress {
                    gossip: addr2,
                    api: addr2,
                    execution: Default::default(),
                },
                last_seen: 0,
                is_online: true,
            };
            let addr_a = Address::repeat_byte(0xA1);
            let addr_b = Address::repeat_byte(0xB1);
            peer_list_guard.add_or_update_peer(addr_a, peer1, true);
            peer_list_guard.add_or_update_peer(addr_b, peer2, true);

            let db = db.clone();
            let data_handler = data_handler_stub(
                &config,
                &peer_list_guard,
                db,
                api_client_stub.clone(),
                sync_state.clone(),
            )
            .await;

            pull_unique_highest_blocks::<_, _, _>(
                &peer_list_guard,
                &api_client_stub,
                data_handler,
                false,
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
