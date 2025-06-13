use crate::block_status_provider::{BlockStatus, BlockStatusProvider};
use crate::peer_list::{PeerList, PeerListFacadeError};
use crate::SyncState;
use base58::ToBase58;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_database::block_header_by_hash;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader};
use lru::LruCache;
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
pub(crate) struct BlockPool<R, B>
where
    R: PeerList,
    B: BlockDiscoveryFacade,
{
    /// Database provider for accessing transaction headers and related data.
    pub(crate) db: DatabaseProvider,

    blocks_cache: BlockCache,

    pub(crate) block_discovery: B,
    pub(crate) peer_list: R,

    sync_state: SyncState,

    block_status_provider: BlockStatusProvider,
}

#[derive(Clone, Debug)]
struct BlockCache {
    pub orphaned_blocks_by_parent: Arc<RwLock<LruCache<BlockHash, IrysBlockHeader>>>,
    pub block_hash_to_parent_hash: Arc<RwLock<LruCache<BlockHash, BlockHash>>>,
}

impl BlockCache {
    pub(crate) fn new() -> Self {
        Self {
            orphaned_blocks_by_parent: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap(),
            ))),
            block_hash_to_parent_hash: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap(),
            ))),
        }
    }

    pub(crate) async fn add_block(&self, block_header: IrysBlockHeader) {
        let mut orphaned_blocks_by_parent = self.orphaned_blocks_by_parent.write().await;
        let mut block_hash_to_parent_hash = self.block_hash_to_parent_hash.write().await;

        orphaned_blocks_by_parent.put(block_header.previous_block_hash, block_header.clone());
        block_hash_to_parent_hash.put(block_header.block_hash, block_header.previous_block_hash);
    }

    pub(crate) async fn remove_block(&self, block_hash: &BlockHash) {
        let mut orphaned_blocks_by_parent = self.orphaned_blocks_by_parent.write().await;
        let mut block_hash_to_parent_hash = self.block_hash_to_parent_hash.write().await;

        if let Some(parent_hash) = block_hash_to_parent_hash.pop(block_hash) {
            orphaned_blocks_by_parent.pop(&parent_hash);
        }
    }

    pub(crate) async fn get_block_header_cloned(
        &self,
        block_hash: &BlockHash,
    ) -> Option<IrysBlockHeader> {
        if let Some(parent_hash) = self.block_hash_to_parent_hash.write().await.get(block_hash) {
            if let Some(header) = self
                .orphaned_blocks_by_parent
                .write()
                .await
                .get(parent_hash)
            {
                return Some(header.clone());
            }
        }

        None
    }
}

impl<R, B> BlockPool<R, B>
where
    R: PeerList,
    B: BlockDiscoveryFacade,
{
    pub(crate) fn new(
        db: DatabaseProvider,
        peer_list: R,
        block_discovery: B,
        sync_state: SyncState,
        block_status_provider: BlockStatusProvider,
    ) -> Self {
        Self {
            db,
            blocks_cache: BlockCache::new(),
            peer_list,
            block_discovery,
            sync_state,
            block_status_provider,
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
            "Block pool: Processing block {} (height {})",
            block_header.block_hash.0.to_base58(),
            block_header.height,
        );

        let current_block_height = block_header.height;
        let prev_block_hash = block_header.previous_block_hash;
        let current_block_hash = block_header.block_hash;

        let previous_block_status = self
            .block_status_provider
            .block_status(block_header.height.saturating_sub(1), &prev_block_hash);

        debug!(
            "Previous block status for block {}: {:?}",
            current_block_hash.0.to_base58(),
            previous_block_status
        );

        // If the parent block is in the db, process it
        if previous_block_status.is_processed() {
            info!(
                "Found parent block for block {}",
                current_block_hash.0.to_base58()
            );

            if let Err(block_discovery_error) = self
                .block_discovery
                .handle_block(block_header.clone())
                .await
            {
                error!("Block pool: Block validation error for block {}: {:?}. Removing block from the pool", block_header.block_hash.0.to_base58(), block_discovery_error);
                self.blocks_cache
                    .remove_block(&block_header.block_hash)
                    .await;
                return Err(BlockPoolError::BlockError(format!(
                    "{:?}",
                    block_discovery_error
                )));
            }

            info!(
                "Block pool: Block {} has been processed",
                current_block_hash.0.to_base58()
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
                    "Error processing orphaned ancestor for block {}: {:?}",
                    block_header.block_hash.0.to_base58(),
                    err
                );
            }

            return Ok(());
        }

        debug!(
            "Parent block for block {} not found in db",
            current_block_hash.0.to_base58()
        );

        self.request_parent_block_to_be_gossiped(block_header.previous_block_hash)
            .await
    }

    pub(crate) async fn is_block_processing_or_processed(
        &self,
        block_hash: &BlockHash,
        block_height: u64,
    ) -> bool {
        if let Some(parent_hash) = self
            .blocks_cache
            .block_hash_to_parent_hash
            .write()
            .await
            .get(block_hash)
        {
            self.blocks_cache
                .orphaned_blocks_by_parent
                .write()
                .await
                .contains(parent_hash)
        } else {
            self.block_status_provider
                .block_status(block_height, block_hash)
                .is_processed()
        }
    }

    async fn process_orphaned_ancestor(&self, block_hash: BlockHash) -> Result<(), BlockPoolError> {
        let maybe_orphaned_block = self
            .blocks_cache
            .orphaned_blocks_by_parent
            .write()
            .await
            .get(&block_hash)
            .cloned();

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
            .block_hash_to_parent_hash
            .read()
            .await
            .contains(&previous_block_hash);

        // If the parent is also in the cache it's likely that processing has already started
        if !parent_is_already_in_the_pool {
            debug!(
                "Block pool: Parent block {} not found in the cache, requesting it from the network",
                previous_block_hash.0.to_base58()
            );
            self.request_block_from_the_network(previous_block_hash)
                .await
        } else {
            debug!(
                "Parent block {} is already in the cache, skipping get data request",
                previous_block_hash.0.to_base58()
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
                    "Block pool: Requested block {} from the network",
                    block_hash.0.to_base58()
                );
                Ok(())
            }
            Err(error) => {
                error!("Error while trying to fetch parent block {}: {:?}. Removing the block from the pool", block_hash.0.to_base58(), error);
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
