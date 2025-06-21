use crate::block_status_provider::{BlockStatus, BlockStatusProvider};
use crate::execution_payload_provider::ExecutionPayloadProvider;
use crate::peer_list::{PeerList, PeerListFacadeError};
use crate::SyncState;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_database::block_header_by_hash;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{
    BlockHash, DatabaseProvider, GossipBroadcastMessage, GossipCacheKey, GossipData,
    IrysBlockHeader,
};
use lru::LruCache;
use reth::revm::primitives::B256;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

const BLOCK_POOL_CACHE_SIZE: usize = 1000;

#[derive(Debug, Clone)]
pub enum BlockPoolError {
    DatabaseError(String),
    OtherInternal(String),
    BlockError(String),
    AlreadyProcessed(BlockHash),
    TryingToReprocessFinalizedBlock(BlockHash),
    PreviousBlockDoesNotMatch(String),
}

impl From<PeerListFacadeError> for BlockPoolError {
    fn from(err: PeerListFacadeError) -> Self {
        Self::OtherInternal(format!("Peer list error: {:?}", err))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockPool<P, B>
where
    P: PeerList,
    B: BlockDiscoveryFacade,
{
    /// Database provider for accessing transaction headers and related data.
    db: DatabaseProvider,

    blocks_cache: BlockCache,

    block_discovery: B,
    peer_list: P,

    sync_state: SyncState,

    block_status_provider: BlockStatusProvider,
    execution_payload_provider: ExecutionPayloadProvider<P>,

    gossip_broadcast_sender: tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessage>,
}

#[derive(Clone, Debug)]
struct BlockCacheInner {
    pub(crate) orphaned_blocks_by_parent: LruCache<BlockHash, IrysBlockHeader>,
    pub(crate) block_hash_to_parent_hash: LruCache<BlockHash, BlockHash>,
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

impl<P, B> BlockPool<P, B>
where
    P: PeerList,
    B: BlockDiscoveryFacade,
{
    pub(crate) fn new(
        db: DatabaseProvider,
        peer_list: P,
        block_discovery: B,
        sync_state: SyncState,
        block_status_provider: BlockStatusProvider,
        execution_payload_provider: ExecutionPayloadProvider<P>,
        gossip_broadcast_sender: tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessage>,
    ) -> Self {
        Self {
            db,
            blocks_cache: BlockCache::new(),
            peer_list,
            block_discovery,
            sync_state,
            block_status_provider,
            execution_payload_provider,
            gossip_broadcast_sender,
        }
    }

    pub(crate) async fn process_block(
        &self,
        block_header: IrysBlockHeader,
    ) -> Result<(), BlockPoolError> {
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

            self.process_block(orphaned_block).await
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
        match self
            .peer_list
            .request_block_from_the_network(block_hash)
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
