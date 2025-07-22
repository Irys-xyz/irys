use crate::block_status_provider::{BlockStatus, BlockStatusProvider};
use crate::SyncState;
use actix::Addr;
use irys_actors::block_tree_service::BlockTreeServiceMessage;
use irys_actors::block_validation::shadow_transactions_are_valid;
use irys_actors::reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor};
use irys_actors::services::ServiceSenders;
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_database::block_header_by_hash;
use irys_database::db::IrysDatabaseExt as _;
#[cfg(test)]
use irys_domain::execution_payload_cache::RethBlockProvider;
use irys_domain::{ExecutionPayloadCache, PeerList};
use irys_types::{
    BlockHash, Config, DatabaseProvider, GossipBroadcastMessage, GossipCacheKey, GossipData,
    IrysBlockHeader, PeerNetworkError,
};
use irys_vdf::state::VdfStateReadonly;
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use lru::LruCache;
use reth::revm::primitives::B256;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, error, info};

const BLOCK_POOL_CACHE_SIZE: usize = 250;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum BlockPoolError {
    #[error("Database error: {0}")]
    DatabaseError(String),
    #[error("Mempool error: {0}")]
    MempoolError(String),
    #[error("Internal BlockPool error: {0}")]
    OtherInternal(String),
    #[error("Block error: {0}")]
    BlockError(String),
    #[error("Block {0:?} has already been processed")]
    AlreadyProcessed(BlockHash),
    #[error("Block {0:?} is already being fast tracked")]
    AlreadyFastTracking(BlockHash),
    #[error("Block {0:?} is already being processed or has been processed")]
    TryingToReprocessFinalizedBlock(BlockHash),
    #[error("Block mismatch: {0}")]
    PreviousBlockDoesNotMatch(String),
    #[error("VDF Fast Forward error: {0}")]
    VdfFFError(String),
    #[error("Reth ForkChoiceUpdate failed: {0}")]
    ForkChoiceFailed(String),
    #[error("Previous block {0:?} not found")]
    PreviousBlockNotFound(BlockHash),
}

impl From<PeerNetworkError> for BlockPoolError {
    fn from(err: PeerNetworkError) -> Self {
        Self::OtherInternal(format!("Peer list error: {:?}", err))
    }
}

#[derive(Debug, Clone)]
pub struct BlockPool<B, M>
where
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    /// Database provider for accessing transaction headers and related data.
    db: DatabaseProvider,

    blocks_cache: BlockCache,

    block_discovery: B,
    mempool: M,
    peer_list: PeerList,

    sync_state: SyncState,

    block_status_provider: BlockStatusProvider,
    execution_payload_provider: ExecutionPayloadCache,

    vdf_state: VdfStateReadonly,

    config: Config,
    service_senders: ServiceSenders,
}

#[derive(Clone, Debug)]
struct BlockCacheInner {
    pub(crate) orphaned_blocks_by_parent: LruCache<BlockHash, Arc<IrysBlockHeader>>,
    pub(crate) block_hash_to_parent_hash: LruCache<BlockHash, BlockHash>,
    pub(crate) requested_blocks: HashSet<BlockHash>,
}

#[derive(Clone, Debug)]
struct BlockCache {
    inner: Arc<RwLock<BlockCacheInner>>,
}

impl BlockCache {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(BlockCacheInner::new())),
        }
    }

    async fn add_block(&self, block_header: Arc<IrysBlockHeader>) {
        self.inner.write().await.add_block(block_header);
    }

    async fn remove_block(&self, block_hash: &BlockHash) {
        self.inner.write().await.remove_block(block_hash);
    }

    async fn is_block_in_cache(&self, block_hash: &BlockHash) -> bool {
        self.inner.read().await.is_block_in_cache(block_hash)
    }

    async fn get_block_header_cloned(
        &self,
        block_hash: &BlockHash,
    ) -> Option<Arc<IrysBlockHeader>> {
        self.inner.write().await.get_block_header_cloned(block_hash)
    }

    async fn block_hash_to_parent_hash(&self, block_hash: &BlockHash) -> Option<BlockHash> {
        self.inner
            .write()
            .await
            .block_hash_to_parent_hash
            .get(block_hash)
            .copied()
    }

    async fn block_hash_to_parent_hash_contains(&self, block_hash: &BlockHash) -> bool {
        self.inner
            .write()
            .await
            .block_hash_to_parent_hash
            .contains(block_hash)
    }

    async fn orphaned_blocks_by_parent_contains(&self, block_hash: &BlockHash) -> bool {
        self.inner
            .write()
            .await
            .orphaned_blocks_by_parent
            .contains(block_hash)
    }

    async fn orphaned_blocks_by_parent_cloned(
        &self,
        block_hash: &BlockHash,
    ) -> Option<Arc<IrysBlockHeader>> {
        self.inner
            .write()
            .await
            .orphaned_blocks_by_parent
            .get(block_hash)
            .cloned()
    }

    async fn mark_block_as_requested(&self, block_hash: BlockHash) {
        self.inner.write().await.requested_blocks.insert(block_hash);
    }

    async fn remove_requested_block(&self, block_hash: &BlockHash) {
        self.inner.write().await.requested_blocks.remove(block_hash);
    }

    async fn is_block_requested(&self, block_hash: &BlockHash) -> bool {
        self.inner
            .write()
            .await
            .requested_blocks
            .contains(block_hash)
    }
}

impl BlockCacheInner {
    fn new() -> Self {
        Self {
            orphaned_blocks_by_parent: LruCache::new(
                NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap(),
            ),
            block_hash_to_parent_hash: LruCache::new(
                NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap(),
            ),
            requested_blocks: HashSet::new(),
        }
    }

    fn add_block(&mut self, block_header: Arc<IrysBlockHeader>) {
        self.block_hash_to_parent_hash
            .put(block_header.block_hash, block_header.previous_block_hash);
        self.orphaned_blocks_by_parent
            .put(block_header.previous_block_hash, block_header);
    }

    fn remove_block(&mut self, block_hash: &BlockHash) {
        if let Some(parent_hash) = self.block_hash_to_parent_hash.pop(block_hash) {
            self.orphaned_blocks_by_parent.pop(&parent_hash);
        }
    }

    fn get_block_header_cloned(&mut self, block_hash: &BlockHash) -> Option<Arc<IrysBlockHeader>> {
        if let Some(parent_hash) = self.block_hash_to_parent_hash.get(block_hash) {
            if let Some(header) = self.orphaned_blocks_by_parent.get(parent_hash) {
                return Some(Arc::clone(header));
            }
        }

        None
    }

    fn is_block_in_cache(&self, block_hash: &BlockHash) -> bool {
        self.block_hash_to_parent_hash.contains(block_hash)
    }
}

impl<B, M> BlockPool<B, M>
where
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    pub(crate) fn new(
        db: DatabaseProvider,
        peer_list: PeerList,
        block_discovery: B,
        mempool: M,
        sync_state: SyncState,
        block_status_provider: BlockStatusProvider,
        execution_payload_provider: ExecutionPayloadCache,
        vdf_state: VdfStateReadonly,
        config: Config,
        service_senders: ServiceSenders,
    ) -> Self {
        Self {
            db,
            blocks_cache: BlockCache::new(),
            peer_list,
            block_discovery,
            mempool,
            sync_state,
            block_status_provider,
            execution_payload_provider,
            vdf_state,
            config,
            service_senders,
        }
    }

    async fn insert_poa_to_mempool(&self, header: &IrysBlockHeader) -> Result<(), BlockPoolError> {
        if let Some(chunk) = &header.poa.chunk {
            debug!(
                "Block pool: Inserting POA chunk for block {:?}",
                header.block_hash
            );
            self.mempool
                .insert_poa_chunk(header.block_hash, chunk.clone())
                .await
                .map_err(|report| BlockPoolError::MempoolError(report.to_string()))?;
        }

        Ok(())
    }

    async fn wait_for_parent_to_appear_in_index(
        &self,
        header: &IrysBlockHeader,
    ) -> Result<(), BlockPoolError> {
        let block_height = header.height;

        if block_height > 0 {
            if !self
                .block_status_provider
                .is_height_in_the_index(block_height - 1)
            {
                debug!(
                    "Block pool: Parent block {:?} is not in the index, waiting for it to appear",
                    block_height - 1
                );
                self.block_status_provider
                    .wait_for_block_to_appear_in_index(block_height - 1)
                    .await;
            }

            Ok(())
        } else {
            Err(BlockPoolError::BlockError(
                "Cannot fast track genesis block".to_string(),
            ))
        }
    }

    async fn migrate_block(&self, header: &Arc<IrysBlockHeader>) -> Result<(), BlockPoolError> {
        debug!("Block pool: Migrating block {:?}", header.block_hash);
        self.mempool
            .migrate_block(Arc::clone(header))
            .await
            .map_err(|err| {
                BlockPoolError::MempoolError(format!("Mempool migration error: {:?}", err))
            })
            .map(|_| ())
    }

    async fn finalize_block_storage(
        &self,
        header: &IrysBlockHeader,
    ) -> Result<Option<Addr<RethServiceActor>>, BlockPoolError> {
        let hash = header.block_hash;
        let (sender, receiver) = oneshot::channel();
        self.service_senders
            .block_tree
            .send(BlockTreeServiceMessage::FastTrackStorageFinalized {
                block_header: header.clone(),
                response: sender,
            })
            .map_err(|send_err| {
                error!(
                    "Block pool: Failed to send a fast track request to block tree service: {:?}",
                    send_err
                );
                BlockPoolError::OtherInternal(format!(
                    "Failed to send a fast track request to block tree service: {:?}",
                    send_err
                ))
            })?;

        debug!(
            "Block pool: Fast track request sent to block tree service for block {:?}",
            hash
        );
        receiver
            .await
            .map_err(|recv_err| {
                BlockPoolError::OtherInternal(format!(
                    "Failed to receive a response from block tree service: {:?}",
                    recv_err
                ))
            })?
            .map_err(|err| {
                BlockPoolError::OtherInternal(format!(
                    "Failed to fast-track block in storage: {:?}",
                    err
                ))
            })
    }

    async fn validate_and_submit_reth_payload(
        &self,
        block_header: &IrysBlockHeader,
        reth_service: Option<Addr<RethServiceActor>>,
    ) -> Result<(), BlockPoolError> {
        debug!(
            "Block pool: Validating and submitting execution payload for block {:?}",
            block_header.block_hash
        );

        // For tests that specifically want to mock the payload provider
        // All tests that do not is going to use the real provider
        #[cfg(test)]
        {
            if let RethBlockProvider::Mock(_) =
                &self.execution_payload_provider.reth_payload_provider
            {
                return Ok(());
            }
        }

        let adapter = self
            .execution_payload_provider
            .reth_payload_provider
            .as_irys_reth_adapter()
            .ok_or(BlockPoolError::OtherInternal(
                "Reth payload provider is not set".into(),
            ))?;

        match shadow_transactions_are_valid(
            &self.config,
            &self.service_senders,
            block_header,
            adapter,
            &self.db,
            self.execution_payload_provider.clone(),
        )
        .await
        {
            Ok(()) => {}
            Err(err) => {
                return Err(BlockPoolError::OtherInternal(format!(
                    "Failed to validate and submit the execution payload for block {:?}: {:?}",
                    block_header.block_hash, err
                )));
            }
        }
        debug!(
            "Block pool: Execution payload for block {:?} validated and submitted",
            block_header.block_hash
        );

        if let Some(reth_service) = reth_service {
            debug!(
                "Sending ForkChoiceUpdateMessage to Reth service for block {:?}",
                block_header.block_hash
            );
            reth_service
                .send(ForkChoiceUpdateMessage {
                    head_hash: BlockHashType::Irys(block_header.block_hash),
                    confirmed_hash: None,
                    finalized_hash: None,
                })
                .await
                .map_err(|err| {
                    BlockPoolError::OtherInternal(format!(
                        "Failed to send ForkChoiceUpdateMessage to Reth service: {:?}",
                        err
                    ))
                })?
                .map_err(|err| {
                    BlockPoolError::ForkChoiceFailed(format!(
                        "Failed to update fork choice in Reth service: {:?}",
                        err
                    ))
                })?;
        }

        // Remove the payload from the cache after it has been processed to prevent excessive memory usage
        // during the fast track process (The cache is LRU, but its upper limit is more for unexpected situations)
        self.execution_payload_provider
            .remove_payload_from_cache(&block_header.evm_block_hash)
            .await;

        Ok(())
    }

    async fn reload_block_tree(&self) -> Result<(), BlockPoolError> {
        let (tx, rx) = oneshot::channel();
        self.service_senders
            .block_tree
            .send(BlockTreeServiceMessage::ReloadCacheFromDb { response: tx })
            .map_err(|err| {
                error!("Failed to send ReloadCacheFromDb message: {:?}", err);
                BlockPoolError::OtherInternal(format!(
                    "Failed to send ReloadCacheFromDb message: {:?}",
                    err
                ))
            })?;
        rx.await
            .map_err(|err| {
                error!(
                    "Failed to receive response for ReloadCacheFromDb: {:?}",
                    err
                );
                BlockPoolError::OtherInternal(format!(
                    "Failed to receive response for ReloadCacheFromDb: {:?}",
                    err
                ))
            })?
            .map_err(|err| {
                BlockPoolError::OtherInternal(format!(
                    "Failed to reload block tree cache: {:?}",
                    err
                ))
            })?;
        debug!("Block pool: Reloaded block tree cache");
        Ok(())
    }

    pub async fn repair_missing_payloads_if_any(
        &self,
        reth_service: Option<Addr<RethServiceActor>>,
    ) -> Result<(), BlockPoolError> {
        let Some(latest_block_in_index) = self.block_status_provider.latest_block_in_index() else {
            return Ok(());
        };

        let mut block_hash = latest_block_in_index.block_hash;
        let mut blocks_with_missing_payloads = vec![];

        loop {
            let block = self
                .get_block_data(&block_hash)
                .await?
                .ok_or(BlockPoolError::PreviousBlockNotFound(block_hash))?;

            let prev_payload_exists = self
                .execution_payload_provider
                .get_locally_stored_sealed_block(&block.evm_block_hash)
                .await
                .is_some();

            // Found a block with a payload or reached the genesis block
            if prev_payload_exists || block.height <= 1 {
                break;
            }

            block_hash = block.previous_block_hash;
            blocks_with_missing_payloads.push(block);
        }

        // The last block in the list is the oldest block with a missing payload
        while let Some(block) = blocks_with_missing_payloads.pop() {
            debug!(
                "Block pool: Repairing missing payload for block {:?}",
                block.block_hash
            );
            self.validate_and_submit_reth_payload(&block, reth_service.clone())
                .await?;
        }

        Ok(())
    }

    /// Fast tracks a block by migrating it to the mempool without performing any validation.
    /// This is useful when syncing a block from a node you trust
    async fn fast_track_block(
        &self,
        block_header: Arc<IrysBlockHeader>,
    ) -> Result<(), BlockPoolError> {
        let block_height = block_header.height;
        let block_hash = block_header.block_hash;
        let evm_block_hash = block_header.evm_block_hash;
        let execution_payload_provider = self.execution_payload_provider.clone();
        debug!(
            "Fast tracking block {:?} (height {})",
            block_header.block_hash, block_height,
        );

        // Preemptively request the execution payload from the network, so when we need
        // to validate and submit it, it will be already available
        tokio::spawn(async move {
            execution_payload_provider
                .request_payload_from_the_network(evm_block_hash, true)
                .await;
        });

        if self
            .block_status_provider
            .is_height_in_the_index(block_header.height)
        {
            debug!(
                "Block pool: Block {:?} (height {}) is already in the index, skipping the fast track",
                block_header.block_hash, block_header.height,
            );
            return Err(BlockPoolError::AlreadyProcessed(block_hash));
        }

        if self.blocks_cache.is_block_in_cache(&block_hash).await {
            debug!(
                "Block pool: Block {:?} (height {}) is already in the cache, skipping the fast track",
                block_header.block_hash, block_header.height,
            );
            return Err(BlockPoolError::AlreadyFastTracking(block_hash));
        }

        self.blocks_cache.add_block(block_header.clone()).await;

        // First, wait for the previous VDF step to be available
        let first_step_number = block_header.vdf_limiter_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        debug!(
            "Block pool: Waiting for VDF step number {}",
            prev_output_step_number
        );
        self.vdf_state.wait_for_step(prev_output_step_number).await;

        self.insert_poa_to_mempool(&block_header).await?;
        self.wait_for_parent_to_appear_in_index(&block_header)
            .await?;
        self.migrate_block(&block_header).await?;
        let reth_service = self.finalize_block_storage(&block_header).await?;

        // Validate the payload and submit it to the Reth service
        self.validate_and_submit_reth_payload(&block_header, reth_service)
            .await?;

        // After the block is inserted into the index, we can fast forward the VDF steps to
        // unblock next blocks processing
        fast_forward_vdf_steps_from_block(
            &block_header.vdf_limiter_info,
            &self.service_senders.vdf_fast_forward,
        )
        .map_err(|report| BlockPoolError::VdfFFError(report.to_string()))?;

        let mut process_ancestor = false;
        if let Some(switch_to_full_validation_at_height) =
            self.sync_state.full_validation_switch_height()
        {
            // mark_processed called on the current block height will set trust sync to false
            let switch_at_the_next_block =
                block_height as usize == switch_to_full_validation_at_height;
            if switch_at_the_next_block {
                self.reload_block_tree().await?;
                process_ancestor = true;
            }
        }

        self.sync_state.mark_processed(block_height as usize);
        self.blocks_cache.remove_block(&block_hash).await;
        if process_ancestor {
            debug!("BlockPool: Checking if fast tracked block has orphaned ancestors");
            let fut = Box::pin(self.process_orphaned_ancestor(block_hash));
            if let Err(err) = fut.await {
                // Ancestor processing doesn't affect the current block processing,
                //  but it still is important to log the error
                error!(
                    "Error processing orphaned ancestor for block {:?}: {:?}",
                    block_hash, err
                );
            }
        }
        Ok(())
    }

    pub(crate) async fn process_block(
        &self,
        block_header: Arc<IrysBlockHeader>,
        skip_validation_for_fast_track: bool,
    ) -> Result<(), BlockPoolError> {
        let block_hash = block_header.block_hash;
        if skip_validation_for_fast_track {
            debug!(
                "Block pool: The block {:?} (height {}) is marked for fast track, skipping validation",
                block_header.block_hash, block_header.height,
            );
            return match self.fast_track_block(block_header).await {
                Ok(()) => Ok(()),
                Err(err) => {
                    self.blocks_cache.remove_block(&block_hash).await;
                    Err(err)
                }
            };
        }

        check_block_status(
            &self.block_status_provider,
            block_header.block_hash,
            block_header.height,
        )?;

        // Adding the block to the pool, so if a block depending on that block arrives,
        // this block won't be requested from the network
        self.blocks_cache.add_block(block_header.clone()).await;
        debug!(
            "Block pool: Processing block {:?} (height {})",
            block_header.block_hash, block_header.height,
        );

        let current_block_height = block_header.height;
        let prev_block_hash = block_header.previous_block_hash;
        let current_block_hash = block_header.block_hash;

        let previous_block_status = self
            .block_status_provider
            .block_status(block_header.height.saturating_sub(1), &prev_block_hash);

        debug!(
            "Previous block status for block {:?}: {:?}",
            current_block_hash, previous_block_status
        );

        // If the parent block is in the db, process it
        if previous_block_status.is_processed() {
            info!(
                "Found parent block for block {:?}, checking if tree has enough capacity",
                current_block_hash
            );

            self.block_status_provider
                .wait_for_block_tree_to_catch_up(block_header.height)
                .await;

            if let Err(block_discovery_error) = self
                .block_discovery
                .handle_block(Arc::clone(&block_header))
                .await
            {
                error!("Block pool: Block validation error for block {:?}: {:?}. Removing block from the pool", block_header.block_hash, block_discovery_error);
                self.blocks_cache
                    .remove_block(&block_header.block_hash)
                    .await;
                return Err(BlockPoolError::BlockError(format!(
                    "{:?}",
                    block_discovery_error
                )));
            }

            info!(
                "Block pool: Block {:?} has been processed",
                current_block_hash
            );

            // Request the execution payload for the block if it is not already stored locally
            self.handle_execution_payload_for_prevalidated_block(
                block_header.evm_block_hash,
                false,
            );

            debug!(
                "Block pool: Marking block {:?} as processed",
                current_block_hash
            );
            self.sync_state
                .mark_processed(current_block_height as usize);
            self.blocks_cache
                .remove_block(&block_header.block_hash)
                .await;

            let fut = Box::pin(self.process_orphaned_ancestor(block_header.block_hash));
            if let Err(err) = fut.await {
                // Ancestor processing doesn't affect the current block processing,
                //  but it still is important to log the error
                error!(
                    "Error processing orphaned ancestor for block {:?}: {:?}",
                    block_header.block_hash, err
                );
            }

            return Ok(());
        }

        debug!(
            "Parent block for block {:?} not found in db",
            current_block_hash
        );

        self.request_parent_block_to_be_gossiped(block_header.previous_block_hash)
            .await
    }

    /// Requests the execution payload for the given EVM block hash if it is not already stored
    /// locally. After that, it waits for the payload to arrive and broadcasts it.
    /// This function spawns a new task to fire the request without waiting for the response.
    pub(crate) fn handle_execution_payload_for_prevalidated_block(
        &self,
        evm_block_hash: B256,
        use_trusted_peers_only: bool,
    ) {
        debug!(
            "Block pool: Handling execution payload for EVM block hash: {:?}",
            evm_block_hash
        );
        let execution_payload_provider = self.execution_payload_provider.clone();
        let gossip_broadcast_sender = self.service_senders.gossip_broadcast.clone();
        tokio::spawn(async move {
            if let Some(sealed_block) = execution_payload_provider
                .wait_for_sealed_block(&evm_block_hash, use_trusted_peers_only)
                .await
            {
                let evm_block = sealed_block.into_block();
                if let Err(err) = gossip_broadcast_sender.send(GossipBroadcastMessage::new(
                    GossipCacheKey::ExecutionPayload(evm_block_hash),
                    GossipData::ExecutionPayload(evm_block),
                )) {
                    error!(
                        "Failed to broadcast execution payload for block {:?}: {:?}",
                        evm_block_hash, err
                    );
                } else {
                    debug!(
                        "Execution payload for block {:?} has been broadcasted",
                        evm_block_hash
                    );
                }
            }
        });
    }

    pub(crate) async fn is_block_requested(&self, block_hash: &BlockHash) -> bool {
        self.blocks_cache.is_block_requested(block_hash).await
    }

    pub(crate) async fn is_block_processing_or_processed(
        &self,
        block_hash: &BlockHash,
        block_height: u64,
    ) -> bool {
        if let Some(parent_hash) = self
            .blocks_cache
            .block_hash_to_parent_hash(block_hash)
            .await
        {
            self.blocks_cache
                .orphaned_blocks_by_parent_contains(&parent_hash)
                .await
        } else {
            self.block_status_provider
                .block_status(block_height, block_hash)
                .is_processed()
        }
    }

    async fn process_orphaned_ancestor(&self, block_hash: BlockHash) -> Result<(), BlockPoolError> {
        let maybe_orphaned_block = self
            .blocks_cache
            .orphaned_blocks_by_parent_cloned(&block_hash)
            .await;

        if let Some(orphaned_block) = maybe_orphaned_block {
            info!(
                "Start processing orphaned ancestor block: {:?}",
                orphaned_block.block_hash
            );

            self.process_block(orphaned_block, false).await
        } else {
            info!(
                "No orphaned ancestor block found for block: {:?}",
                block_hash
            );
            Ok(())
        }
    }

    async fn request_parent_block_to_be_gossiped(
        &self,
        parent_block_hash: BlockHash,
    ) -> Result<(), BlockPoolError> {
        let previous_block_hash = parent_block_hash;

        let parent_is_already_in_the_pool = self
            .blocks_cache
            .block_hash_to_parent_hash_contains(&previous_block_hash)
            .await;

        // If the parent is also in the cache, it's likely that processing has already started
        if !parent_is_already_in_the_pool {
            debug!(
                "Block pool: Parent block {:?} not found in the cache, requesting it from the network",
                previous_block_hash
            );
            self.request_block_from_the_network(previous_block_hash)
                .await
        } else {
            debug!(
                "Parent block {:?} is already in the cache, skipping get data request",
                previous_block_hash
            );
            Ok(())
        }
    }

    async fn request_block_from_the_network(
        &self,
        block_hash: BlockHash,
    ) -> Result<(), BlockPoolError> {
        self.blocks_cache.mark_block_as_requested(block_hash).await;
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
                    "Block pool: Requested block {:?} from the network",
                    block_hash
                );
                Ok(())
            }
            Err(error) => {
                error!("Error while trying to fetch parent block {:?}: {:?}. Removing the block from the pool", block_hash, error);
                self.blocks_cache.remove_requested_block(&block_hash).await;
                self.blocks_cache.remove_block(&block_hash).await;
                Err(error.into())
            }
        }
    }

    /// Inserts an execution payload into the internal cache so that it can be
    /// retrieved by the [`ExecutionPayloadProvider`].
    pub async fn add_execution_payload_to_cache(
        &self,
        sealed_block: reth::primitives::SealedBlock<reth::primitives::Block>,
    ) {
        self.execution_payload_provider
            .add_payload_to_cache(sealed_block)
            .await;
    }

    pub(crate) async fn get_block_data(
        &self,
        block_hash: &BlockHash,
    ) -> Result<Option<Arc<IrysBlockHeader>>, BlockPoolError> {
        if let Some(header) = self.blocks_cache.get_block_header_cloned(block_hash).await {
            return Ok(Some(header));
        }

        match self.mempool.get_block_header(*block_hash, true).await {
            Ok(Some(header)) => return Ok(Some(Arc::new(header))),
            Ok(None) => {}
            Err(err) => {
                return Err(BlockPoolError::MempoolError(format!(
                    "Mempool error: {:?}",
                    err
                )))
            }
        }

        self.db
            .view_eyre(|tx| block_header_by_hash(tx, block_hash, true))
            .map_err(|db_error| BlockPoolError::DatabaseError(format!("{:?}", db_error)))
            .map(|block| block.map(Arc::new))
    }
}

fn check_block_status(
    block_status_provider: &BlockStatusProvider,
    block_hash: BlockHash,
    block_height: u64,
) -> Result<(), BlockPoolError> {
    let block_status = block_status_provider.block_status(block_height, &block_hash);

    match block_status {
        BlockStatus::NotProcessed => Ok(()),
        BlockStatus::ProcessedButCanBeReorganized => {
            debug!(
                "Block pool: Block {:?} (height {}) is already processed",
                block_hash, block_height,
            );
            Err(BlockPoolError::AlreadyProcessed(block_hash))
        }
        BlockStatus::Finalized => {
            debug!(
                    "Block pool: Block at height {} is finalized and cannot be reorganized (Tried to process block {:?})",
                    block_height,
                    block_hash,
                );
            Err(BlockPoolError::TryingToReprocessFinalizedBlock(block_hash))
        }
    }
}
