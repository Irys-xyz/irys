use crate::block_status_provider::{BlockStatus, BlockStatusProvider};
use crate::execution_payload_provider::ExecutionPayloadProvider;
use crate::peer_list::{PeerList, PeerListFacadeError};
use crate::SyncState;
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_database::block_header_by_hash;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{
    BlockHash, DatabaseProvider, GossipBroadcastMessage, GossipCacheKey, GossipData,
    IrysBlockHeader,
};
use irys_vdf::state::VdfStateReadonly;
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use irys_vdf::VdfStep;
use lru::LruCache;
use reth::revm::primitives::B256;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

const BLOCK_POOL_CACHE_SIZE: usize = 1000;

#[derive(Debug, Clone)]
pub enum BlockPoolError {
    DatabaseError(String),
    MempoolError(String),
    OtherInternal(String),
    BlockError(String),
    AlreadyProcessed(BlockHash),
    TryingToReprocessFinalizedBlock(BlockHash),
    PreviousBlockDoesNotMatch(String),
    VdfFFError(String),
}

impl From<PeerListFacadeError> for BlockPoolError {
    fn from(err: PeerListFacadeError) -> Self {
        Self::OtherInternal(format!("Peer list error: {:?}", err))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockPool<P, B, M>
where
    P: PeerList,
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    /// Database provider for accessing transaction headers and related data.
    db: DatabaseProvider,

    blocks_cache: BlockCache,

    block_discovery: B,
    mempool: M,
    peer_list: P,

    sync_state: SyncState,

    block_status_provider: BlockStatusProvider,
    execution_payload_provider: ExecutionPayloadProvider<P>,

    gossip_broadcast_sender: tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessage>,
    vdf_state: VdfStateReadonly,
    vdf_ff_sender: tokio::sync::mpsc::UnboundedSender<VdfStep>,
}

#[derive(Clone, Debug)]
struct BlockCacheInner {
    pub(crate) orphaned_blocks_by_parent: LruCache<BlockHash, IrysBlockHeader>,
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

    async fn add_block(&self, block_header: IrysBlockHeader) {
        self.inner.write().await.add_block(block_header);
    }

    async fn remove_block(&self, block_hash: &BlockHash) {
        self.inner.write().await.remove_block(block_hash);
    }

    async fn get_block_header_cloned(&self, block_hash: &BlockHash) -> Option<IrysBlockHeader> {
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
    ) -> Option<IrysBlockHeader> {
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

    fn add_block(&mut self, block_header: IrysBlockHeader) {
        self.orphaned_blocks_by_parent
            .put(block_header.previous_block_hash, block_header.clone());
        self.block_hash_to_parent_hash
            .put(block_header.block_hash, block_header.previous_block_hash);
    }

    fn remove_block(&mut self, block_hash: &BlockHash) {
        if let Some(parent_hash) = self.block_hash_to_parent_hash.pop(block_hash) {
            self.orphaned_blocks_by_parent.pop(&parent_hash);
        }
    }

    fn get_block_header_cloned(&mut self, block_hash: &BlockHash) -> Option<IrysBlockHeader> {
        if let Some(parent_hash) = self.block_hash_to_parent_hash.get(block_hash) {
            if let Some(header) = self.orphaned_blocks_by_parent.get(parent_hash) {
                return Some(header.clone());
            }
        }

        None
    }
}

impl<P, B, M> BlockPool<P, B, M>
where
    P: PeerList,
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    pub(crate) fn new(
        db: DatabaseProvider,
        peer_list: P,
        block_discovery: B,
        mempool: M,
        sync_state: SyncState,
        block_status_provider: BlockStatusProvider,
        execution_payload_provider: ExecutionPayloadProvider<P>,
        gossip_broadcast_sender: tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessage>,
        vdf_state: VdfStateReadonly,
        vdf_ff_sender: tokio::sync::mpsc::UnboundedSender<VdfStep>,
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
            gossip_broadcast_sender,
            vdf_state,
            vdf_ff_sender,
        }
    }

    /// Fast tracks a block by migrating it to the mempool without performing any validation.
    /// This is useful when syncing a block from a node you trust
    async fn fast_track_block(&self, block_header: IrysBlockHeader) -> Result<(), BlockPoolError> {
        let block_height = block_header.height;
        let block_hash = block_header.block_hash;
        debug!(
            "Fast tracking block {:?} (height {})",
            block_header.block_hash, block_height,
        );

        // Check if the block is already processed or being processed
        if self
            .is_block_processing_or_processed(&block_header.block_hash, block_header.height)
            .await
        {
            debug!(
                "Block {:?} (height {}) is already processed or being processed",
                block_header.block_hash, block_header.height,
            );
            return Ok(());
        }

        // First, wait for the previous VDF step to be available
        let first_step_number = block_header.vdf_limiter_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        debug!(
            "Block pool: Waiting for VDF step number {}",
            prev_output_step_number
        );
        self.vdf_state.wait_for_step(prev_output_step_number).await;

        debug!(
            "Block pool: Fast forwarding VDF steps for block {:?}",
            block_header.block_hash
        );
        fast_forward_vdf_steps_from_block(&block_header.vdf_limiter_info, &self.vdf_ff_sender)
            .map_err(|report| BlockPoolError::VdfFFError(report.to_string()))?;

        if let Some(chunk) = &block_header.poa.chunk {
            debug!(
                "Block pool: Inserting POA chunk for block {:?}",
                block_header.block_hash
            );
            self.mempool
                .insert_poa_chunk(block_header.block_hash, chunk.clone())
                .await
                .map_err(|report| BlockPoolError::MempoolError(report.to_string()))?;
        };

        if block_height > 0 {
            debug!(
                "Block pool: Fast tracking block {:?}: Checking if the parent block is in the index",
                block_header.block_hash,
            );
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
        } else {
            return Err(BlockPoolError::BlockError(
                "Cannot fast track genesis block".to_string(),
            ));
        }

        debug!("Block pool: Migrating block {:?}", block_header.block_hash);
        self.mempool
            .migrate_block(block_header)
            .await
            .map_err(|err| {
                BlockPoolError::MempoolError(format!("Mempool migration error: {:?}", err))
            })?;

        debug!(
            "Block pool: Block {:?} successfully fast tracked",
            block_hash
        );
        self.sync_state.mark_processed(block_height as usize);
        Ok(())
    }

    pub(crate) async fn process_block(
        &self,
        block_header: IrysBlockHeader,
        skip_validation_for_fast_track: bool,
    ) -> Result<(), BlockPoolError> {
        if skip_validation_for_fast_track {
            debug!(
                "Block pool: The block block {:?} (height {}) is marked for fast track, skipping validation",
                block_header.block_hash, block_header.height,
            );
            return self.fast_track_block(block_header).await;
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
            info!("Found parent block for block {:?}", current_block_hash);

            if let Err(block_discovery_error) = self
                .block_discovery
                .handle_block(block_header.clone())
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
            self.handle_execution_payload_for_prevalidated_block(block_header.evm_block_hash);

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
    pub(crate) fn handle_execution_payload_for_prevalidated_block(&self, evm_block_hash: B256) {
        let execution_payload_provider = self.execution_payload_provider.clone();
        let gossip_broadcast_sender = self.gossip_broadcast_sender.clone();
        tokio::spawn(async move {
            if let Some(sealed_block) = execution_payload_provider
                .wait_for_sealed_block(&evm_block_hash)
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

    pub(crate) async fn get_block_data(
        &self,
        block_hash: &BlockHash,
    ) -> Result<Option<IrysBlockHeader>, BlockPoolError> {
        if let Some(header) = self.blocks_cache.get_block_header_cloned(block_hash).await {
            return Ok(Some(header));
        }

        match self.mempool.get_block_header(*block_hash, true).await {
            Ok(Some(header)) => return Ok(Some(header)),
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
