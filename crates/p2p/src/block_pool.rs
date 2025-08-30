use crate::block_status_provider::{BlockStatus, BlockStatusProvider};
use crate::chain_sync::SyncChainServiceMessage;
use crate::types::InternalGossipError;
use crate::{GossipError, GossipResult};
use actix::Addr;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::block_validation::shadow_transactions_are_valid;
use irys_actors::reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor};
use irys_actors::services::ServiceSenders;
use irys_actors::MempoolFacade;
use irys_database::block_header_by_hash;
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::chain_sync_state::ChainSyncState;
#[cfg(test)]
use irys_domain::execution_payload_cache::RethBlockProvider;
use irys_domain::ExecutionPayloadCache;
use irys_types::{
    BlockHash, Config, DatabaseProvider, EvmBlockHash, GossipBroadcastMessage, IrysBlockHeader,
    PeerNetworkError,
};
use lru::LruCache;
use reth::revm::primitives::B256;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, instrument, warn};

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

#[derive(Debug, Copy, Clone)]
pub(crate) enum ProcessBlockResult {
    /// Block has been processed successfully
    Processed,
    /// Block has been added to the pool, waiting for the parent block
    ParentRequested,
    /// Block has been added to the pool, but the parent block is already in the cache, so no request was made
    ParentAlreadyInCache,
    /// Block has been added to the pool, and the request for the parent block failed
    ParentRequestFailed,
    /// Block has been added to the pool, but no request for the parent block was made (too far ahead of canonical)
    ParentTooFarAhead,
}

#[derive(Debug, Clone)]
pub struct BlockPool<B, M>
where
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    /// Database provider for accessing transaction headers and related data.
    db: DatabaseProvider,

    blocks_cache: BlockCacheGuard,

    block_discovery: B,
    mempool: M,
    sync_service_sender: mpsc::UnboundedSender<SyncChainServiceMessage>,

    sync_state: ChainSyncState,

    block_status_provider: BlockStatusProvider,
    execution_payload_provider: ExecutionPayloadCache,

    config: Config,
    service_senders: ServiceSenders,
}

#[derive(Clone, Debug)]
pub(crate) struct CachedBlock {
    pub(crate) header: Arc<IrysBlockHeader>,
    pub(crate) is_processing: bool,
    pub(crate) is_fast_tracking: bool,
}

#[derive(Clone, Debug)]
struct BlockCacheInner {
    pub(crate) orphaned_blocks_by_parent: LruCache<BlockHash, HashSet<BlockHash>>,
    pub(crate) blocks: LruCache<BlockHash, CachedBlock>,
    pub(crate) requested_blocks: HashSet<BlockHash>,
}

#[derive(Clone, Debug)]
pub(crate) struct BlockCacheGuard {
    inner: Arc<RwLock<BlockCacheInner>>,
}

impl BlockCacheGuard {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(BlockCacheInner::new())),
        }
    }

    async fn add_block(&self, block_header: Arc<IrysBlockHeader>, is_fast_tracking: bool) {
        self.inner
            .write()
            .await
            .add_block(block_header, is_fast_tracking);
    }

    async fn remove_block(&self, block_hash: &BlockHash) {
        self.inner.write().await.remove_block(block_hash);
    }

    async fn get_block_header_cloned(&self, block_hash: &BlockHash) -> Option<CachedBlock> {
        self.inner.write().await.get_block_header_cloned(block_hash)
    }

    async fn contains_block(&self, block_hash: &BlockHash) -> bool {
        self.inner.write().await.blocks.contains(block_hash)
    }

    async fn orphaned_blocks_for_parent(
        &self,
        block_hash: &BlockHash,
    ) -> Option<HashSet<BlockHash>> {
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

    /// Internal crate method to clear cache
    pub(crate) async fn clear(&self) {
        let mut guard = self.inner.write().await;
        guard.orphaned_blocks_by_parent.clear();
        guard.requested_blocks.clear();
        guard.blocks.clear();
    }

    async fn is_block_processing(&self, block_hash: &BlockHash) -> bool {
        self.inner.write().await.is_block_processing(block_hash)
    }

    async fn change_block_processing_status(&self, block_hash: BlockHash, is_processing: bool) {
        self.inner
            .write()
            .await
            .change_block_processing_status(block_hash, is_processing);
    }
}

impl BlockCacheInner {
    fn new() -> Self {
        Self {
            orphaned_blocks_by_parent: LruCache::new(
                NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap(),
            ),
            blocks: LruCache::new(NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap()),
            requested_blocks: HashSet::new(),
        }
    }

    fn add_block(&mut self, block_header: Arc<IrysBlockHeader>, fast_track: bool) {
        let block_hash = block_header.block_hash;
        let previous_block_hash = block_header.previous_block_hash;
        self.blocks.put(
            block_header.block_hash,
            CachedBlock {
                header: block_header,
                is_processing: true,
                is_fast_tracking: fast_track,
            },
        );

        if let Some(set) = self.orphaned_blocks_by_parent.get_mut(&previous_block_hash) {
            set.insert(block_hash);
        } else {
            let mut set = HashSet::new();
            set.insert(block_hash);
            self.orphaned_blocks_by_parent.put(previous_block_hash, set);
        }
    }

    fn is_block_processing(&mut self, block_hash: &BlockHash) -> bool {
        self.blocks
            .get(block_hash)
            .map(|block| block.is_processing)
            .unwrap_or(false)
    }

    fn change_block_processing_status(&mut self, block_hash: BlockHash, is_processing: bool) {
        if let Some(block) = self.blocks.get_mut(&block_hash) {
            block.is_processing = is_processing
        }
    }

    fn remove_block(&mut self, block_hash: &BlockHash) {
        if let Some(removed_block) = self.blocks.pop(block_hash) {
            let parent_hash = removed_block.header.previous_block_hash;
            let mut set_is_empty = false;
            if let Some(set) = self.orphaned_blocks_by_parent.get_mut(&parent_hash) {
                set.remove(block_hash);
                if set.is_empty() {
                    set_is_empty = true;
                }
            }
            if set_is_empty {
                self.orphaned_blocks_by_parent.pop(&parent_hash);
            }
        }
    }

    fn get_block_header_cloned(&mut self, block_hash: &BlockHash) -> Option<CachedBlock> {
        self.blocks.get(block_hash).cloned()
    }
}

impl<B, M> BlockPool<B, M>
where
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    pub(crate) fn new(
        db: DatabaseProvider,
        block_discovery: B,
        mempool: M,
        sync_service_sender: mpsc::UnboundedSender<SyncChainServiceMessage>,
        sync_state: ChainSyncState,
        block_status_provider: BlockStatusProvider,
        execution_payload_provider: ExecutionPayloadCache,
        config: Config,
        service_senders: ServiceSenders,
    ) -> Self {
        Self {
            db,
            blocks_cache: BlockCacheGuard::new(),
            block_discovery,
            mempool,
            sync_service_sender,
            sync_state,
            block_status_provider,
            execution_payload_provider,
            config,
            service_senders,
        }
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

        // Get parent epoch snapshot from block tree
        let parent_epoch_snapshot = self
            .block_status_provider
            .block_tree_read_guard()
            .read()
            .get_epoch_snapshot(&block_header.previous_block_hash)
            .ok_or_else(|| {
                BlockPoolError::OtherInternal(format!(
                    "Parent epoch snapshot isn't found for block {:?}",
                    block_header.previous_block_hash
                ))
            })?;

        // Get block index from the block status provider
        let block_index = self.block_status_provider.block_index_read_guard().inner();

        Self::pull_and_seal_execution_payload(
            &self.execution_payload_provider,
            &self.sync_service_sender,
            block_header.evm_block_hash,
            false,
        )
        .await
        .map_err(|error| {
            BlockPoolError::OtherInternal(format!(
                "Encountered a problem while trying to fix payload {:?}: {error:?}",
                block_header.evm_block_hash
            ))
        })?;

        match shadow_transactions_are_valid(
            &self.config,
            &self.service_senders,
            block_header,
            adapter,
            &self.db,
            self.execution_payload_provider.clone(),
            parent_epoch_snapshot,
            block_index,
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

    #[instrument(err, skip_all)]
    pub async fn repair_missing_payloads_if_any(
        &self,
        reth_service: Option<Addr<RethServiceActor>>,
    ) -> Result<(), BlockPoolError> {
        if reth_service.is_none() {
            error!("Reth service is not available, skipping payload repair");
            return Ok(());
        }

        let Some(latest_block_in_index) = self.block_status_provider.latest_block_in_index() else {
            debug!("No payloads to repair");
            return Ok(());
        };

        let mut block_hash = latest_block_in_index.block_hash;
        debug!("Latest block in index: {}", &block_hash);
        let mut blocks_with_missing_payloads = vec![];

        loop {
            let block = self
                .get_block_data(&block_hash)
                .await?
                .ok_or(BlockPoolError::PreviousBlockNotFound(block_hash))?;

            let prev_payload_exists = self
                .execution_payload_provider
                .is_stored_in_reth(&block.evm_block_hash);

            // Found a block with a payload or reached the genesis block
            if prev_payload_exists || block.height <= 1 {
                break;
            }

            block_hash = block.previous_block_hash;
            debug!(
                "Found block with missing payload: {} {} {}",
                &block.block_hash, &block.height, &block.evm_block_hash
            );
            blocks_with_missing_payloads.push(block);
        }

        if blocks_with_missing_payloads.is_empty() {
            debug!("No missing payloads found");
            return Ok(());
        }

        // The last block in the list is the oldest block with a missing payload
        while let Some(block) = blocks_with_missing_payloads.pop() {
            debug!(
                "Repairing a missing payload for the block {:?}",
                block.block_hash
            );
            self.validate_and_submit_reth_payload(&block, reth_service.clone())
                .await?;
        }

        Ok(())
    }

    #[instrument(skip_all, target = "BlockPool")]
    pub(crate) async fn process_block(
        &self,
        block_header: Arc<IrysBlockHeader>,
        skip_validation_for_fast_track: bool,
    ) -> Result<ProcessBlockResult, BlockPoolError> {
        check_block_status(
            &self.block_status_provider,
            block_header.block_hash,
            block_header.height,
        )?;

        let is_processing = self
            .blocks_cache
            .is_block_processing(&block_header.block_hash)
            .await;
        if is_processing {
            warn!(
                "Block pool: Block {:?} is already being processed or fast-tracked, skipping",
                block_header.block_hash
            );
            return Err(BlockPoolError::AlreadyProcessed(block_header.block_hash));
        }

        self.blocks_cache
            .add_block(Arc::clone(&block_header), skip_validation_for_fast_track)
            .await;

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
            "Previous block status for the parent block of the block {:?}: {:?}",
            current_block_hash, previous_block_status
        );

        if !previous_block_status.is_processed() {
            self.blocks_cache
                .change_block_processing_status(block_header.block_hash, false)
                .await;
            debug!(
                "Parent block for block {:?} is not found in the db",
                current_block_hash
            );

            let is_already_in_cache = self.blocks_cache.contains_block(&prev_block_hash).await;

            if is_already_in_cache {
                debug!(
                    "Parent block {:?} is already in the cache, skipping the request",
                    prev_block_hash
                );
                return Ok(ProcessBlockResult::ParentAlreadyInCache);
            }

            let canonical_height = self.block_status_provider.canonical_height();

            if current_block_height
                > canonical_height + u64::from(self.config.consensus.block_migration_depth * 2)
            {
                // IMPORTANT! If the node is just processing blocks slower than the network, the sync service should catch it up eventually.
                warn!(
                    "Block pool: The block {:?} (height {}) is too far ahead of the latest canonical block (height {}). This might indicate a potential issue.",
                    current_block_hash, current_block_height, canonical_height
                );

                return Ok(ProcessBlockResult::ParentTooFarAhead);
            }

            // Use the sync service to request parent block (fire and forget)
            if let Err(send_err) =
                self.sync_service_sender
                    .send(SyncChainServiceMessage::RequestBlockFromTheNetwork {
                        block_hash: prev_block_hash,
                        response: None,
                    })
            {
                error!(
                    "BlockPool: Failed to send RequestBlockFromTheNetwork message: {:?}",
                    send_err
                );
                return Ok(ProcessBlockResult::ParentRequestFailed);
            } else {
                return Ok(ProcessBlockResult::ParentRequested);
            }
        }

        if skip_validation_for_fast_track {
            // Preemptively handle reth payload for the trusted sync path
            if let Err(err) = Self::pull_and_seal_execution_payload(
                &self.execution_payload_provider,
                &self.sync_service_sender,
                block_header.evm_block_hash,
                skip_validation_for_fast_track,
            )
            .await
            {
                error!("Block pool: Reth payload fetching error for block {:?}: {:?}. Removing block from the pool", block_header.block_hash, err);
                self.blocks_cache
                    .remove_block(&block_header.block_hash)
                    .await;
                return Err(BlockPoolError::BlockError(format!("{:?}", err)));
            }
        }

        info!(
            "Found parent block for block {:?}, checking if tree has enough capacity",
            current_block_hash
        );

        // TODO: validate this UNTRUSTED height against the parent block's height (as we have processed it)

        self.block_status_provider
            .wait_for_block_tree_can_process_height(block_header.height)
            .await;

        if let Err(block_discovery_error) = self
            .block_discovery
            .handle_block(Arc::clone(&block_header), skip_validation_for_fast_track)
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

        if !skip_validation_for_fast_track {
            // If skip validation is true, we handle it preemptively above, if it isn't, it's a
            //  good idea to request it here
            self.pull_and_seal_execution_payload_in_background(
                block_header.evm_block_hash,
                skip_validation_for_fast_track,
            );
        }

        debug!(
            "Block pool: Marking block {:?} as processed",
            current_block_hash
        );
        self.sync_state
            .mark_processed(current_block_height as usize);
        self.blocks_cache
            .remove_block(&block_header.block_hash)
            .await;

        debug!(
            "Block pool: Notifying sync service to process orphaned ancestors of block {:?}",
            current_block_hash
        );
        if let Err(send_err) =
            self.sync_service_sender
                .send(SyncChainServiceMessage::BlockProcessedByThePool {
                    block_hash: current_block_hash,
                    response: None,
                })
        {
            error!(
                "Block pool: Failed to send BlockProcessedByThePool message: {:?}",
                send_err
            );
        }

        Ok(ProcessBlockResult::Processed)
    }

    pub(crate) async fn pull_and_seal_execution_payload(
        execution_payload_provider: &ExecutionPayloadCache,
        sync_service_sender: &mpsc::UnboundedSender<SyncChainServiceMessage>,
        evm_block_hash: EvmBlockHash,
        use_trusted_peers_only: bool,
    ) -> GossipResult<()> {
        debug!(
            "Block pool: Forcing handling of execution payload for EVM block hash: {:?}",
            evm_block_hash
        );
        let (response_sender, response_receiver) = tokio::sync::oneshot::channel();

        if !execution_payload_provider
            .is_payload_in_cache(&evm_block_hash)
            .await
        {
            debug!("BlockPool: Execution payload for EVM block hash {:?} is not in cache, requesting from the network", evm_block_hash);
            if let Err(send_err) =
                sync_service_sender.send(SyncChainServiceMessage::PullPayloadFromTheNetwork {
                    evm_block_hash,
                    use_trusted_peers_only,
                    response: response_sender,
                })
            {
                let err_text = format!(
                    "BlockPool: Failed to send PullPayloadFromTheNetwork message: {:?}",
                    send_err
                );
                error!(err_text);
                return Err(GossipError::Internal(InternalGossipError::Unknown(
                    err_text,
                )));
            }

            response_receiver.await.map_err(|recv_err| {
                let err_text = format!(
                    "BlockPool: Failed to receive response from PullPayloadFromTheNetwork: {:?}",
                    recv_err
                );
                error!(err_text);
                GossipError::Internal(InternalGossipError::Unknown(err_text))
            })?
        } else {
            debug!("BlockPool: Payload for EVM block hash {:?} is already in cache, no need to request", evm_block_hash);
            Ok(())
        }
    }

    /// Requests the execution payload for the given EVM block hash if it is not already stored
    /// locally. After that, it waits for the payload to arrive and broadcasts it.
    /// This function spawns a new task to fire the request without waiting for the response.
    pub(crate) fn pull_and_seal_execution_payload_in_background(
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
        let chain_sync_sender = self.sync_service_sender.clone();
        tokio::spawn(async move {
            match Self::pull_and_seal_execution_payload(
                &execution_payload_provider,
                &chain_sync_sender,
                evm_block_hash,
                use_trusted_peers_only,
            )
            .await
            {
                Ok(()) => {
                    let gossip_payload = execution_payload_provider
                        .get_sealed_block_from_cache(&evm_block_hash)
                        .await
                        .map(GossipBroadcastMessage::from);

                    if let Some(payload) = gossip_payload {
                        if let Err(err) = gossip_broadcast_sender.send(payload) {
                            error!("Block pool: Failed to broadcast execution payload for EVM block hash {:?}: {:?}", evm_block_hash, err);
                        } else {
                            debug!(
                                "Block pool: Broadcasted execution payload for EVM block hash {:?}",
                                evm_block_hash
                            );
                        }
                    }
                }
                Err(err) => {
                    error!("Block pool: Failed to handle execution payload for EVM block hash {:?}: {:?}", evm_block_hash, err);
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
        self.blocks_cache.is_block_processing(block_hash).await
            || self
                .block_status_provider
                .block_status(block_height, block_hash)
                .is_processed()
    }

    /// Internal method for the p2p services to get direct access to the cache
    pub(crate) fn block_cache_guard(&self) -> BlockCacheGuard {
        self.blocks_cache.clone()
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
            return Ok(Some(Arc::clone(&header.header)));
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

    /// Get orphaned block by parent hash - for orphan block processing
    pub(crate) async fn get_orphaned_blocks_by_parent(
        &self,
        parent_hash: &BlockHash,
    ) -> Option<Vec<CachedBlock>> {
        let orphaned_hashes = self
            .blocks_cache
            .orphaned_blocks_for_parent(parent_hash)
            .await;

        if let Some(hashes) = orphaned_hashes {
            let mut orphaned_blocks = Vec::new();
            for hash in hashes {
                if let Some(block) = self.blocks_cache.get_block_header_cloned(&hash).await {
                    orphaned_blocks.push(block);
                }
            }
            Some(orphaned_blocks)
        } else {
            None
        }
    }

    /// Check if parent hash exists in block cache - for orphan block processing
    pub(crate) async fn contains_block(&self, block_hash: &BlockHash) -> bool {
        self.blocks_cache.contains_block(block_hash).await
    }

    /// Mark the block as requested - for orphan block processing
    pub(crate) async fn mark_block_as_requested(&self, block_hash: BlockHash) {
        self.blocks_cache.mark_block_as_requested(block_hash).await;
    }

    /// Remove requested block - for orphan block processing
    pub(crate) async fn remove_requested_block(&self, block_hash: &BlockHash) {
        self.blocks_cache.remove_requested_block(block_hash).await;
    }

    /// Remove block from cache - for orphan block processing
    pub(crate) async fn remove_block_from_cache(&self, block_hash: &BlockHash) {
        self.blocks_cache.remove_block(block_hash).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_header(block_byte: u8, parent_byte: u8, height: u64) -> Arc<IrysBlockHeader> {
        let header = IrysBlockHeader {
            height,
            block_hash: BlockHash::repeat_byte(block_byte),
            previous_block_hash: BlockHash::repeat_byte(parent_byte),
            ..IrysBlockHeader::default()
        };
        Arc::new(header)
    }

    #[test]
    fn add_single_block() {
        let mut cache = BlockCacheInner::new();
        let parent = BlockHash::repeat_byte(0xAA);
        let child1 = make_header(0x01, 0xAA, 10);

        cache.add_block(child1.clone(), false);

        // parent -> set contains child1
        let set = cache
            .orphaned_blocks_by_parent
            .get(&parent)
            .expect("parent entry should exist");
        assert!(set.contains(&child1.block_hash));

        // child1 present in blocks cache with correct flags
        let cached = cache
            .blocks
            .get(&child1.block_hash)
            .expect("child1 should be stored in blocks cache");
        assert!(cached.is_processing);
        assert!(!cached.is_fast_tracking);
    }

    #[test]
    fn add_multiple_sibling_blocks_only_first_cached() {
        let mut cache = BlockCacheInner::new();
        let parent = BlockHash::repeat_byte(0xBB);
        let child1 = make_header(0x02, 0xBB, 11);
        let child2 = make_header(0x03, 0xBB, 12);

        cache.add_block(child1.clone(), true); // fast track first
        cache.add_block(child2.clone(), false); // second sibling

        let set = cache
            .orphaned_blocks_by_parent
            .get(&parent)
            .expect("parent entry should exist");
        assert!(set.contains(&child1.block_hash));
        assert!(set.contains(&child2.block_hash));

        // Only the first added sibling is stored in blocks cache (current implementation behavior)
        assert!(cache.blocks.get(&child1.block_hash).is_some());
        assert!(cache.blocks.get(&child2.block_hash).is_some());

        // Verify the fast tracking flag for first
        assert!(
            cache
                .blocks
                .get(&child1.block_hash)
                .expect("exists")
                .is_fast_tracking
        );
    }

    #[test]
    fn remove_blocks_updates_mappings() {
        let mut cache = BlockCacheInner::new();
        let parent = BlockHash::repeat_byte(0xCC);
        let child1 = make_header(0x10, 0xCC, 20);
        let child2 = make_header(0x11, 0xCC, 21);
        cache.add_block(child1.clone(), false);
        cache.add_block(child2.clone(), false);

        // Remove first child
        cache.remove_block(&child1.block_hash);
        // parent entry still exists because child2 remains
        let set = cache
            .orphaned_blocks_by_parent
            .get(&parent)
            .expect("parent entry should remain");
        assert!(!set.contains(&child1.block_hash));
        assert!(set.contains(&child2.block_hash));

        // Remove the second child
        cache.remove_block(&child2.block_hash);
        // parent entry should now be gone
        assert!(cache.orphaned_blocks_by_parent.get(&parent).is_none());
    }

    #[test]
    fn change_processing_status() {
        let mut cache = BlockCacheInner::new();
        let block = make_header(0x20, 0xDD, 30);
        cache.add_block(block.clone(), false);

        assert!(
            cache
                .blocks
                .get(&block.block_hash)
                .expect("cached")
                .is_processing
        );

        cache.change_block_processing_status(block.block_hash, false);
        assert!(
            !cache
                .blocks
                .get(&block.block_hash)
                .expect("cached")
                .is_processing
        );
    }

    #[test]
    fn remove_nonexistent_block_is_noop() {
        let mut cache = BlockCacheInner::new();
        // Attempt to remove block that was never added
        let bogus = BlockHash::repeat_byte(0xEE);
        cache.remove_block(&bogus);
        // Ensure internal maps remain empty
        assert!(cache.orphaned_blocks_by_parent.iter().next().is_none());
        assert!(cache.blocks.iter().next().is_none());
    }

    #[test]
    fn remove_single_orphan_removes_parent_entry() {
        let mut cache = BlockCacheInner::new();
        let parent = BlockHash::repeat_byte(0xAB);
        let child = make_header(0xCD, 0xAB, 42);
        cache.add_block(child.clone(), false);
        // Sanity: parent entry exists
        assert!(cache.orphaned_blocks_by_parent.get(&parent).is_some());
        // Remove only child
        cache.remove_block(&child.block_hash);
        // Parent entry should be removed entirely
        assert!(cache.orphaned_blocks_by_parent.get(&parent).is_none());
    }
}
