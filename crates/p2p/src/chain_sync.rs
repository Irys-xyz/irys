use crate::{BlockPool, GossipError};
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::mempool_service::MempoolFacade;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{BlockIndexReadGuard, PeerList};
use irys_types::{
    BlockHash, BlockIndexItem, BlockIndexQuery, Config, NodeMode, TokioServiceHandle,
};
use rand::prelude::SliceRandom as _;
use reth::tasks::shutdown::Shutdown;
use std::collections::VecDeque;
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
        }
    }
}

const BLOCK_BATCH_SIZE: usize = 10;
const PERIODIC_SYNC_CHECK_INTERVAL_SECS: u64 = 30; // Check every 30 seconds if we're behind

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
}

/// Inner service containing the sync logic
#[derive(Debug, Clone)]
pub struct ChainSyncServiceInner<T: ApiClient, B: BlockDiscoveryFacade, M: MempoolFacade> {
    sync_state: ChainSyncState,
    api_client: T,
    peer_list: PeerList,
    config: irys_types::Config,
    block_index: BlockIndexReadGuard,
    block_pool: Arc<BlockPool<B, M>>,
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
    ) -> Self {
        Self::new_with_client(
            sync_state,
            IrysApiClient::new(),
            peer_list,
            config,
            block_index,
            block_pool,
        )
    }
}

impl<T: ApiClient, B: BlockDiscoveryFacade, M: MempoolFacade> ChainSyncServiceInner<T, B, M> {
    pub fn new_with_client(
        sync_state: ChainSyncState,
        api_client: T,
        peer_list: PeerList,
        config: irys_types::Config,
        block_index: BlockIndexReadGuard,
        block_pool: Arc<BlockPool<B, M>>,
    ) -> Self {
        Self {
            sync_state,
            api_client,
            peer_list,
            config,
            block_index,
            block_pool,
        }
    }

    /// Perform a sync operation
    async fn spawn_chain_sync_task(&self, response: Option<oneshot::Sender<ChainSyncResult<()>>>) {
        let start_sync_from_height = self.block_index.read().latest_height();
        // Clear any pending blocks from the cache
        self.block_pool.block_cache_guard().clear().await;

        let config = self.config.clone();
        let peer_list = self.peer_list.clone();
        let sync_state = self.sync_state.clone();
        let api_client = self.api_client.clone();

        tokio::spawn(async move {
            let res = sync_chain(
                sync_state.clone(),
                api_client,
                &peer_list,
                start_sync_from_height
                    .try_into()
                    .expect("Expected to be able to convert u64 to usize"),
                &config,
            )
            .await;

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
        });
    }

    /// Process orphaned ancestor block - moved from OrphanBlockProcessingService
    async fn process_orphaned_ancestor(&self, block_hash: BlockHash) -> Result<(), ChainSyncError> {
        let maybe_orphaned_block = self
            .block_pool
            .get_orphaned_block_by_parent(&block_hash)
            .await;

        if let Some(orphaned_block) = maybe_orphaned_block {
            info!(
                "Start processing orphaned ancestor block: {:?}",
                orphaned_block.block_hash
            );

            self.block_pool
                .process_block(orphaned_block, false)
                .await
                .map_err(|e| ChainSyncError::Internal(format!("Block processing error: {:?}", e)))
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
        let parent_is_already_in_the_pool = self
            .block_pool
            .is_parent_hash_in_cache(&parent_block_hash)
            .await;

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
        match self
            .peer_list
            .request_block_from_the_network(
                block_hash,
                self.sync_state.is_syncing_from_a_trusted_peer(),
            )
            .await
        {
            Ok(()) => {
                debug!(
                    "Orphan service: Requested block {:?} from the network",
                    block_hash
                );
                Ok(())
            }
            Err(error) => {
                error!("Error while trying to fetch parent block {:?}: {:?}. Removing the block from the pool", block_hash, error);
                self.block_pool.remove_requested_block(&block_hash).await;
                self.block_pool.remove_block_from_cache(&block_hash).await;
                Err(ChainSyncError::Internal(format!(
                    "Network error: {:?}",
                    error
                )))
            }
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
            .instrument(tracing::Span::current()),
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
                    .spawn_chain_sync_task(Some(response_sender))
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
                    let result = inner.process_orphaned_ancestor(block_hash).await;
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
        }
        Ok(())
    }

    async fn handle_periodic_sync_check(&self) {
        // Skip if already syncing
        if self.inner.sync_state.is_syncing() {
            debug!("Periodic sync check: Already syncing, skipping");
            return;
        }

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
                self.inner.spawn_chain_sync_task(None).await;
            }
            Ok(false) => {
                debug!("Periodic sync check: We're up to date with the network");
            }
            Err(e) => {
                warn!(
                    "Periodic sync check: Failed to check if behind network: {}, trusted peers are likely offline, trying to run a sync without them",
                    e
                );
                self.inner.spawn_chain_sync_task(None).await;
            }
        }
    }
}

#[instrument(skip_all, err)]
async fn sync_chain(
    sync_state: ChainSyncState,
    api_client: impl ApiClient,
    peer_list: &PeerList,
    mut start_sync_from_height: usize,
    config: &irys_types::Config,
) -> ChainSyncResult<()> {
    let node_mode = config.node_config.mode;
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
    let is_trusted_mode = matches!(node_mode, NodeMode::TrustedPeerSync);

    sync_state.set_syncing_from(start_sync_from_height);
    sync_state.set_trusted_sync(is_trusted_mode);

    let is_in_genesis_mode = matches!(node_mode, NodeMode::Genesis);

    if matches!(node_mode, NodeMode::TrustedPeerSync) {
        sync_state.set_trusted_sync(true);
    } else {
        sync_state.set_trusted_sync(false);
    }

    debug!("Sync task: Starting a chain sync task, waiting for active peers. Mode: {:?}, starting from height: {}, trusted mode: {}", node_mode, start_sync_from_height, sync_state.is_trusted_sync());

    if is_in_genesis_mode && sync_state.sync_target_height() <= 1 {
        debug!("Sync task: The node is a genesis node with no blocks, skipping the sync task");
        sync_state.finish_sync();
        return Ok(());
    }

    let fetch_index_from_the_trusted_peer = !is_in_genesis_mode;
    if is_in_genesis_mode {
        warn!("Sync task: Because the node is a genesis node, waiting for active peers for {}, and if no peers are added, then skipping the sync task", genesis_peer_discovery_timeout_millis);
        match timeout(
            Duration::from_millis(genesis_peer_discovery_timeout_millis),
            peer_list.wait_for_active_peers(),
        )
        .await
        {
            Ok(()) => {}
            Err(elapsed) => {
                warn!("Sync task: Due to the node being in genesis mode, after waiting for active peers for {} and no peers showing up, skipping the sync task", elapsed);
                sync_state.finish_sync();
                return Ok(());
            }
        };
    } else {
        peer_list.wait_for_active_peers().await;
    }

    debug!("Sync task: Syncing started");

    if is_trusted_mode {
        let trusted_peers = peer_list.trusted_peers();
        if trusted_peers.is_empty() {
            return Err(ChainSyncError::Network(
                "No trusted peers available".to_string(),
            ));
        }

        check_and_update_full_validation_switch_height(config, peer_list, &api_client, &sync_state)
            .await?;
    }

    let block_index = match get_block_index(
        peer_list,
        &api_client,
        sync_state.sync_target_height(),
        BLOCK_BATCH_SIZE,
        5,
        fetch_index_from_the_trusted_peer,
    )
    .await
    {
        Ok(index) => {
            debug!("Sync task: Fetched block index: {:?}", index);
            index
        }
        Err(err) => {
            error!("Sync task: Failed to fetch block index: {}", err);
            if is_in_genesis_mode {
                warn!("Sync task: No peers available, skipping the sync task");
                sync_state.finish_sync();
                return Ok(());
            }
            return Err(err);
        }
    };

    let mut target = sync_state.sync_target_height() + block_index.len();

    let mut blocks_to_request = block_index.len();

    let mut block_queue = VecDeque::new();
    block_queue.extend(block_index);

    // If no new blocks were added to the index, nothing is going to mark
    //  the tip as processed
    if block_queue.is_empty() {
        debug!("Sync task: No new blocks to process, marking the current sync target height as processed");
        sync_state.mark_processed(sync_state.sync_target_height());
    }

    while let Some(block) = block_queue.pop_front() {
        if sync_state.is_queue_full() {
            debug!("Sync task: Block queue is full, waiting for an empty slot");
            sync_state.wait_for_an_empty_queue_slot().await;
        }

        let peer_list_clone = peer_list.clone();
        let sync_state_clone = sync_state.clone();
        let block_hash = block.block_hash;

        tokio::spawn(async move {
            debug!(
                "Sync task: Requesting block {:?} (sync height is {}) from the network",
                block_hash,
                sync_state_clone.sync_target_height()
            );
            sync_state_clone.increment_sync_target_height();
            match peer_list_clone
                .request_block_from_the_network(block_hash, sync_state_clone.is_trusted_sync())
                .await
            {
                Ok(()) => {
                    info!(
                        "Sync task: Successfully requested block {:?} (sync height is {}) from the network",
                        block_hash,
                        sync_state_clone.sync_target_height()
                    );
                }
                Err(err) => {
                    error!(
                        "Sync task: Failed to request block {:?} (height {}) from the network: {}",
                        block_hash,
                        sync_state_clone.sync_target_height(),
                        err
                    );
                }
            }
        }.in_current_span());

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
                target,
                BLOCK_BATCH_SIZE,
                5,
                fetch_index_from_the_trusted_peer,
            )
            .await?;

            target += additional_index.len();
            block_queue.extend(additional_index);
            blocks_to_request = block_queue.len();
            if blocks_to_request == 0 {
                debug!(
                    "block index request for entries >{} returned no extra results",
                    &target
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
    info!("Sync task: Gossip service sync task completed");
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
    let trusted_peers = peer_list.trusted_peers();
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
        peer_list.trusted_peers()
    } else {
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

    let trusted_peers = peer_list.trusted_peers();
    if trusted_peers.is_empty() {
        return Err(ChainSyncError::Network(
            "No trusted peers available".to_string(),
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
        }
    }

    if let Some(highest_trusted_peer_height) = highest_trusted_peer_height {
        Ok(block_index.read().latest_height() + migration_depth < highest_trusted_peer_height)
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
        use crate::GetPeerListGuard;
        use actix::Actor as _;
        use eyre::eyre;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::{
            Address, Config, DatabaseProvider, NodeConfig, PeerAddress, PeerListItem,
            PeerNetworkSender, PeerScore,
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
            fake_gossip_server.set_on_block_data_request(move |block_hash| {
                let mut block_requests = block_requests.lock().unwrap();
                let requests_len = block_requests.len();
                block_requests.push(block_hash);

                // Simulating one false response so the block gets requested again
                if requests_len == 0 {
                    false
                } else {
                    sync_state_clone.mark_processed(start_from + requests_len);
                    true
                }
            });
            let fake_gossip_address = fake_gossip_server.spawn();
            let fake_peer_address = PeerAddress {
                gossip: fake_gossip_address,
                api: fake_gossip_address,
                execution: Default::default(),
            };

            let mut node_config = NodeConfig::testing();
            node_config.mode = NodeMode::PeerSync;
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

                // Simulate process needing to make two calls
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
                db,
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

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                api_client_stub.clone(),
                &peer_list_guard,
                10,
                &config,
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
            node_config.mode = NodeMode::Genesis;
            node_config.trusted_peers = vec![];
            node_config.genesis_peer_discovery_timeout_millis = 10;
            let config = Config::new(node_config);

            let api_client_stub = ApiClientStub {
                txs: Default::default(),
                block_index_handler: Arc::new(RwLock::new(Box::new(
                    move |_query: BlockIndexQuery| Err(eyre!("Simulating index request error")),
                ))),
                block_index_calls: Arc::new(Default::default()),
            };

            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();

            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let peer_list_service = PeerNetworkService::new_with_custom_api_client(
                db,
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

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            sync_chain(
                sync_state.clone(),
                api_client_stub.clone(),
                &peer_list,
                start_from,
                &config,
            )
            .await
            .expect("to finish catching up");

            // Check that the sync status has changed to synced
            assert!(!sync_state.is_syncing());

            Ok(())
        }
    }
}
