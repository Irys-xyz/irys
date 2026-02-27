use crate::block_status_provider::{BlockStatus, BlockStatusProvider};
use crate::chain_sync::SyncChainServiceMessage;
use crate::types::InternalGossipError;
use crate::{GossipDataHandler, GossipError, GossipResult};
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::mempool_guard::MempoolReadGuard;
use irys_actors::reth_service::{ForkChoiceUpdateMessage, RethServiceMessage};
use irys_actors::services::ServiceSenders;
use irys_actors::{MempoolFacade, TxIngressError};
use irys_database::block_header_by_hash;
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::chain_sync_state::ChainSyncState;

#[cfg(test)]
use irys_domain::execution_payload_cache::RethBlockProvider;

use irys_domain::forkchoice_markers::ForkChoiceMarkers;
use irys_domain::ExecutionPayloadCache;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::SystemLedger;
use irys_types::{
    BlockBody, BlockHash, BlockTransactions, Config, DataLedger, DatabaseProvider, EvmBlockHash,
    IrysBlockHeader, IrysTransactionResponse, PeerNetworkError, H256,
};
use lru::LruCache;
use reth::revm::primitives::B256;
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::num::NonZeroUsize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, error, info, instrument, warn};

const BLOCK_POOL_CACHE_SIZE: usize = 250;
const BACKFILL_DEPTH: u64 = 100;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum CriticalBlockPoolError {
    #[error("Database error: {0}")]
    DatabaseError(String),
    #[error("Mempool error: {0}")]
    MempoolError(String),
    #[error("Internal BlockPool error: {0}")]
    OtherInternal(String),
    #[error("Block error: {0}")]
    BlockError(String),
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
    #[error("Block {0:?} is a part of a pruned fork")]
    ForkedBlock(BlockHash),
    #[error("Transaction validation for the block {0:?} failed: {1:?}")]
    TransactionValidationFailed(BlockHash, TxIngressError),
    #[error("Trying to reprocess block {0:?} that is not in the pool")]
    TryingToReprocessBlockThatIsNotInPool(BlockHash),
    #[error("Header/body mismatch in block {block_hash:?}: {ledger} ledger expects {expected} txs but found {found}. Missing tx IDs: {missing_ids:?}")]
    HeaderBodyMismatch {
        block_hash: BlockHash,
        ledger: String,
        expected: usize,
        found: usize,
        missing_ids: Vec<H256>,
    },
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum AdvisoryBlockPoolError {
    #[error("Block {0:?} has already been processed")]
    AlreadyProcessed(BlockHash),
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum BlockPoolError {
    #[error(transparent)]
    Critical(#[from] CriticalBlockPoolError),
    #[error(transparent)]
    Advisory(#[from] AdvisoryBlockPoolError),
}

impl From<PeerNetworkError> for BlockPoolError {
    fn from(err: PeerNetworkError) -> Self {
        Self::Critical(CriticalBlockPoolError::OtherInternal(format!(
            "Peer list error: {:?}",
            err
        )))
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
    pub(crate) db: DatabaseProvider,

    blocks_cache: BlockCacheGuard,

    block_discovery: B,
    mempool: M,
    sync_service_sender: mpsc::UnboundedSender<SyncChainServiceMessage>,

    sync_state: ChainSyncState,

    block_status_provider: BlockStatusProvider,
    pub execution_payload_provider: ExecutionPayloadCache,

    config: Config,
    service_senders: ServiceSenders,
    pub mempool_guard: MempoolReadGuard,
}

#[derive(Clone, Debug)]
pub(crate) struct CachedBlock {
    pub(crate) header: Arc<IrysBlockHeader>,
    pub(crate) is_processing: bool,
    pub(crate) is_fast_tracking: bool,
    pub(crate) block_body: Arc<BlockBody>,
}

#[derive(Clone, Debug)]
struct BlockCacheInner {
    pub(crate) orphaned_blocks_by_parent: LruCache<BlockHash, HashSet<BlockHash>>,
    pub(crate) blocks: LruCache<BlockHash, CachedBlock>,
    pub(crate) requested_blocks: HashSet<BlockHash>,
    /// Per-block fetched transactions cache. Groups transactions by the block they belong to.
    pub(crate) txs_by_block: LruCache<BlockHash, Vec<IrysTransactionResponse>>,
}

#[derive(Clone, Debug)]
pub(crate) struct BlockCacheGuard {
    inner: Arc<RwLock<BlockCacheInner>>,
}

#[derive(Clone, Debug)]
pub(crate) enum BlockRemovalReason {
    SuccessfullyProcessed,
    FailedToProcess(FailureReason),
}

impl Display for BlockRemovalReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SuccessfullyProcessed => {
                write!(f, "Block was successfully processed")
            }
            Self::FailedToProcess(reason) => {
                write!(f, "Block processing failed due to: {}", reason)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum FailureReason {
    ParentIsAPartOfAPrunedFork,
    WasNotAbleToFetchRethPayload,
    BlockPrevalidationFailed,
    FailedToPull(GossipError),
    HeaderBodyMismatch,
}

impl Display for FailureReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParentIsAPartOfAPrunedFork => {
                write!(f, "Parent block is a part of a pruned fork")
            }
            Self::WasNotAbleToFetchRethPayload => {
                write!(f, "Was not able to fetch Reth execution payload")
            }
            Self::BlockPrevalidationFailed => {
                write!(f, "Block prevalidation failed")
            }
            Self::FailedToPull(err) => {
                write!(f, "Failed to pull block because: {}", err)
            }
            Self::HeaderBodyMismatch => {
                write!(f, "Header/body transaction count mismatch")
            }
        }
    }
}

impl BlockCacheGuard {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(BlockCacheInner::new())),
        }
    }

    async fn add_block(
        &self,
        block_header: Arc<IrysBlockHeader>,
        block_body: Arc<BlockBody>,
        is_fast_tracking: bool,
    ) -> Option<(BlockHash, CachedBlock)> {
        self.inner
            .write()
            .await
            .add_block(block_header, block_body, is_fast_tracking)
    }

    async fn remove_block(&self, block_hash: &BlockHash, reason: BlockRemovalReason) {
        debug!("Block {block_hash:?} has been removed from BlockPool because {reason}",);
        self.inner.write().await.remove_block(block_hash);
    }

    async fn get_block_cloned(&self, block_hash: &BlockHash) -> Option<CachedBlock> {
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

    async fn is_block_processing(&self, block_hash: &BlockHash) -> bool {
        self.inner.write().await.is_block_processing(block_hash)
    }

    async fn change_block_processing_status(&self, block_hash: BlockHash, is_processing: bool) {
        self.inner
            .write()
            .await
            .change_block_processing_status(block_hash, is_processing);
    }

    async fn take_txs_for_block(&self, block_hash: &BlockHash) -> Vec<IrysTransactionResponse> {
        self.inner.write().await.take_txs_for_block(block_hash)
    }

    async fn add_txs_for_block(&self, block_hash: BlockHash, txs: Vec<IrysTransactionResponse>) {
        self.inner.write().await.add_txs_for_block(block_hash, txs);
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
            txs_by_block: LruCache::new(NonZeroUsize::new(BLOCK_POOL_CACHE_SIZE).unwrap()),
        }
    }

    /// Return a block if the previous record has been updated or evicted
    fn add_block(
        &mut self,
        block_header: Arc<IrysBlockHeader>,
        block_body: Arc<BlockBody>,
        fast_track: bool,
    ) -> Option<(BlockHash, CachedBlock)> {
        let block_hash = block_header.block_hash;
        let previous_block_hash = block_header.previous_block_hash;
        let evicted = self.blocks.push(
            block_header.block_hash,
            CachedBlock {
                header: block_header,
                is_processing: true,
                is_fast_tracking: fast_track,
                block_body,
            },
        );

        if let Some(set) = self.orphaned_blocks_by_parent.get_mut(&previous_block_hash) {
            set.insert(block_hash);
        } else {
            let mut set = HashSet::new();
            set.insert(block_hash);
            self.orphaned_blocks_by_parent.put(previous_block_hash, set);
        }

        evicted
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
            // Remove any transactions cached for this block (if present)
            self.txs_by_block.pop(block_hash);
        }
    }

    fn get_block_header_cloned(&mut self, block_hash: &BlockHash) -> Option<CachedBlock> {
        self.blocks.get(block_hash).cloned()
    }

    fn take_txs_for_block(&mut self, block_hash: &BlockHash) -> Vec<IrysTransactionResponse> {
        self.txs_by_block.pop(block_hash).unwrap_or_default()
    }

    fn add_txs_for_block(&mut self, block_hash: BlockHash, txs: Vec<IrysTransactionResponse>) {
        self.txs_by_block.put(block_hash, txs);
    }
}

impl<B, M> BlockPool<B, M>
where
    B: BlockDiscoveryFacade,
    M: MempoolFacade,
{
    #[tracing::instrument(level = "trace", skip_all, err)]
    fn fcu_markers(&self) -> eyre::Result<ForkChoiceMarkers> {
        let migration_depth = self.config.consensus.block_migration_depth as usize;
        let prune_depth = self.config.consensus.block_tree_depth as usize;
        let tree = self.block_status_provider.block_tree_read_guard().read();
        let index = self.block_status_provider.block_index_read_guard().read();
        ForkChoiceMarkers::from_block_tree(&tree, &index, &self.db, migration_depth, prune_depth)
    }

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
        mempool_guard: MempoolReadGuard,
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
            mempool_guard,
        }
    }

    async fn validate_and_submit_reth_payload(
        &self,
        block_header: &IrysBlockHeader,
        reth_service: Option<mpsc::UnboundedSender<RethServiceMessage>>,
        gossip_data_handler: Arc<GossipDataHandler<M, B>>,
    ) -> Result<(), BlockPoolError> {
        // This function repairs missing execution payloads for already-validated blocks.
        // Since blocks have been validated when accepted into the block index, we
        // presume that the block is valid and submit the payload to reth
        debug!(
            "Block pool: Repairing missing execution payload for block {:?}",
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
            .ok_or(CriticalBlockPoolError::OtherInternal(
                "Reth payload provider is not set".into(),
            ))?;

        Self::pull_and_seal_execution_payload(
            &self.execution_payload_provider,
            &self.sync_service_sender,
            block_header.evm_block_hash,
            false,
            Some(Arc::clone(&gossip_data_handler)),
        )
        .await
        .map_err(|error| {
            CriticalBlockPoolError::OtherInternal(format!(
                "Encountered a problem while trying to fix payload {:?}: {error:?}",
                block_header.evm_block_hash
            ))
        })?;

        // Fetch the execution data that was already pulled and sealed
        let execution_data = self
            .execution_payload_provider
            .wait_for_payload(&block_header.evm_block_hash)
            .await
            .ok_or_else(|| {
                CriticalBlockPoolError::OtherInternal(format!(
                    "Failed to fetch execution payload for block {:?}",
                    block_header.evm_block_hash
                ))
            })?;

        // Directly submit the payload to reth
        irys_actors::block_validation::submit_payload_to_reth(
            block_header,
            adapter,
            execution_data,
        )
        .await
        .map_err(|err| {
            CriticalBlockPoolError::OtherInternal(format!(
                "Failed to submit payload to reth for block {:?}: {:?}",
                block_header.block_hash, err
            ))
        })?;
        debug!(
            "Block pool: Execution payload for block {:?} repaired and submitted",
            block_header.block_hash
        );

        if let Some(reth_service) = reth_service {
            let fcu_markers = self.fcu_markers().map_err(|_err| {
                CriticalBlockPoolError::OtherInternal("FCU marker computation failed".to_string())
            })?;
            let head_hash = fcu_markers.head.block_hash;
            let confirmed_hash = fcu_markers.migration_block.block_hash;
            let finalized_hash = fcu_markers.prune_block.block_hash;
            debug!(
                fcu.head = %head_hash,
                fcu.confirmed = %confirmed_hash,
                fcu.finalized = %finalized_hash,
                "Sending ForkChoiceUpdateMessage to Reth service"
            );
            let (tx, rx) = oneshot::channel();

            reth_service
                .send(RethServiceMessage::ForkChoice {
                    update: ForkChoiceUpdateMessage {
                        head_hash,
                        confirmed_hash,
                        finalized_hash,
                    },
                    response: tx,
                })
                .map_err(|err| {
                    CriticalBlockPoolError::OtherInternal(format!(
                        "Failed to send ForkChoiceUpdateMessage to Reth service: {:?}",
                        err
                    ))
                })?;

            rx.await.map_err(|err| {
                CriticalBlockPoolError::ForkChoiceFailed(format!(
                    "Reth service dropped FCU acknowledgment: {:?}",
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
        reth_service: Option<mpsc::UnboundedSender<RethServiceMessage>>,
        gossip_data_handler: Arc<GossipDataHandler<M, B>>,
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
                .get_block_header(&block_hash)
                .await?
                .ok_or(CriticalBlockPoolError::PreviousBlockNotFound(block_hash))?;

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
            self.validate_and_submit_reth_payload(
                &block,
                reth_service.clone(),
                Arc::clone(&gossip_data_handler),
            )
            .await?;
        }

        Ok(())
    }

    /// Process a block with provided BlockTransactions.
    ///
    /// This method handles:
    /// - Block status validation
    /// - Parent block resolution (caches txs if block becomes orphan)
    /// - Mempool synchronization (inserting txs for validation service)
    /// - Block discovery processing
    /// - Cleanup and notifications
    #[instrument(
        skip_all,
        target = "BlockPool",
        fields(block.hash = ?block_header.block_hash, block.height = block_header.height),
    )]
    pub(crate) async fn process_block(
        &self,
        block_header: Arc<IrysBlockHeader>,
        block_body: Arc<BlockBody>,
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
            return Err(BlockPoolError::Advisory(
                AdvisoryBlockPoolError::AlreadyProcessed(block_header.block_hash),
            ));
        }

        let maybe_evicted_or_updated = self
            .blocks_cache
            .add_block(
                Arc::clone(&block_header),
                Arc::clone(&block_body),
                skip_validation_for_fast_track,
            )
            .await;

        if let Some((evicted_hash, _)) = maybe_evicted_or_updated {
            let is_evicted = evicted_hash != block_header.block_hash;
            if is_evicted {
                warn!("Block {evicted_hash:?} has been evicted from BlockPool cache");
            }
        }

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
            "Previous block status for the parent block of the block {:?}: {:?}: {:?}",
            current_block_hash, prev_block_hash, previous_block_status
        );

        if previous_block_status.is_a_part_of_pruned_fork() {
            error!(
                "Block pool: Parent block ({:?}) for block {:?} is a part of a pruned fork, removing block from the pool",
                prev_block_hash, current_block_hash
            );
            self.blocks_cache
                .remove_block(
                    &block_header.block_hash,
                    BlockRemovalReason::FailedToProcess(FailureReason::ParentIsAPartOfAPrunedFork),
                )
                .await;
            return Err(CriticalBlockPoolError::ForkedBlock(block_header.block_hash).into());
        }

        if !previous_block_status.is_processed() {
            let block_transactions = match order_transactions_for_block(
                &block_header,
                block_body.data_transactions.clone(),
                block_body.commitment_transactions.clone(),
            ) {
                Ok(txs) => txs,
                Err(
                    e @ BlockPoolError::Critical(CriticalBlockPoolError::HeaderBodyMismatch {
                        block_hash,
                        ..
                    }),
                ) => {
                    error!(
                        "Block pool: Header/body mismatch for orphan block {:?}. Removing from cache.",
                        block_hash
                    );
                    self.blocks_cache
                        .remove_block(
                            &block_header.block_hash,
                            BlockRemovalReason::FailedToProcess(FailureReason::HeaderBodyMismatch),
                        )
                        .await;
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            self.blocks_cache
                .change_block_processing_status(block_header.block_hash, false)
                .await;

            // Cache transactions for this orphan block so they're available when reprocessed
            let txs_to_cache: Vec<IrysTransactionResponse> = block_transactions
                .all_data_txs()
                .map(|tx| IrysTransactionResponse::Storage(tx.clone()))
                .chain(
                    block_transactions
                        .commitment_txs
                        .iter()
                        .map(|tx| IrysTransactionResponse::Commitment(tx.clone())),
                )
                .collect();
            if !txs_to_cache.is_empty() {
                debug!(
                    "Caching {} transactions for orphan block {:?}",
                    txs_to_cache.len(),
                    current_block_hash
                );
                self.blocks_cache
                    .add_txs_for_block(current_block_hash, txs_to_cache)
                    .await;
            }

            debug!(
                "Parent block for block {:?} is not found in the db",
                current_block_hash
            );

            let is_already_in_cache = self.blocks_cache.contains_block(&prev_block_hash).await;

            if is_already_in_cache {
                let is_parent_processing = self
                    .blocks_cache
                    .is_block_processing(&prev_block_hash)
                    .await;
                if is_parent_processing {
                    debug!(
                        "Parent block {:?} is already processing, skipping the request",
                        prev_block_hash
                    );
                } else {
                    debug!(
                        "Parent block {:?} is already in the cache and not processing, triggering its processing",
                        prev_block_hash
                    );
                    if let Err(err) = self.sync_service_sender.send(
                        SyncChainServiceMessage::AttemptReprocessingBlock(prev_block_hash),
                    ) {
                        error!(
                            "BlockPool: Failed to send AttemptReprocessingBlock message: {:?}",
                            err
                        );
                    }
                }
                return Ok(ProcessBlockResult::ParentAlreadyInCache);
            } else {
                debug!(
                    "Parent block for block {:?} is not in the cache either",
                    current_block_hash
                );
            }

            let canonical_height = self.block_status_provider.canonical_height();

            if current_block_height > canonical_height + BACKFILL_DEPTH {
                // IMPORTANT! If the node is just processing blocks slower than the network, the sync service should catch it up eventually.
                warn!(
                    "Block pool: The block {:?} (height {}) is too far ahead of the latest canonical block (height {}). This might indicate a potential issue.",
                    current_block_hash, current_block_height, canonical_height
                );

                return Ok(ProcessBlockResult::ParentTooFarAhead);
            }

            debug!(
                "Requesting parent block {:?} for block {:?} from the network",
                prev_block_hash, current_block_hash
            );
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
                debug!(
                    "Block pool: Requested parent block {:?} for block {:?} from the network",
                    prev_block_hash, current_block_hash
                );
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
                None,
            )
            .await
            {
                error!(
                    "Block pool: Reth payload fetching error for block {:?}: {:?}. Removing block from the pool",
                    block_header.block_hash, err
                );
                self.blocks_cache
                    .remove_block(
                        &block_header.block_hash,
                        BlockRemovalReason::FailedToProcess(
                            FailureReason::WasNotAbleToFetchRethPayload,
                        ),
                    )
                    .await;
                return Err(CriticalBlockPoolError::BlockError(format!("{:?}", err)).into());
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

        let block_transactions = match order_transactions_for_block(
            &block_header,
            block_body.data_transactions.clone(),
            block_body.commitment_transactions.clone(),
        ) {
            Ok(txs) => txs,
            Err(
                e @ BlockPoolError::Critical(CriticalBlockPoolError::HeaderBodyMismatch {
                    block_hash,
                    ..
                }),
            ) => {
                error!(
                    "Block pool: Header/body mismatch for block {:?}. Removing from cache.",
                    block_hash
                );
                self.blocks_cache
                    .remove_block(
                        &block_header.block_hash,
                        BlockRemovalReason::FailedToProcess(FailureReason::HeaderBodyMismatch),
                    )
                    .await;
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        debug!(
            "Block pool: Processing block {:?} with {} submit, {} publish, {} commitment txs",
            current_block_hash,
            block_transactions.get_ledger_txs(DataLedger::Submit).len(),
            block_transactions.get_ledger_txs(DataLedger::Publish).len(),
            block_transactions.commitment_txs.len()
        );

        // Insert transactions into mempool so validation service can find them later.
        for commitment_tx in &block_transactions.commitment_txs {
            if let Err(err) = self
                .mempool
                .handle_commitment_transaction_ingress_gossip(commitment_tx.clone())
                .await
            {
                if !matches!(err, TxIngressError::Skipped) {
                    warn!(
                        "Block pool: Failed to insert commitment tx {} into mempool for block {:?}: {:?}",
                        commitment_tx.id(), current_block_hash, err
                    );
                }
            }
        }
        for data_tx in block_transactions.all_data_txs() {
            if let Err(err) = self
                .mempool
                .handle_data_transaction_ingress_gossip(data_tx.clone())
                .await
            {
                if !matches!(err, TxIngressError::Skipped) {
                    warn!(
                        "Block pool: Failed to insert data tx {} into mempool for block {:?}: {:?}",
                        data_tx.id, current_block_hash, err
                    );
                }
            }
        }

        if let Err(block_discovery_error) = self
            .block_discovery
            .handle_block(
                Arc::clone(&block_header),
                block_transactions,
                skip_validation_for_fast_track,
            )
            .await
        {
            error!(
                "Block pool: Block validation error for block {:?}: {:?}. Removing block from the pool",
                block_header.block_hash, block_discovery_error
            );
            self.blocks_cache
                .remove_block(
                    &block_header.block_hash,
                    BlockRemovalReason::FailedToProcess(FailureReason::BlockPrevalidationFailed),
                )
                .await;
            return Err(
                CriticalBlockPoolError::BlockError(format!("{:?}", block_discovery_error)).into(),
            );
        }

        info!(
            "Block pool: Block {:?} has been processed",
            current_block_hash
        );

        if !skip_validation_for_fast_track {
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
            .remove_block(
                &block_header.block_hash,
                BlockRemovalReason::SuccessfullyProcessed,
            )
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
        gossip_data_handler: Option<Arc<GossipDataHandler<M, B>>>,
    ) -> GossipResult<()> {
        debug!(
            "Block pool: Forcing handling of execution payload for EVM block hash: {:?}",
            evm_block_hash
        );
        let (response_sender, response_receiver) = oneshot::channel();

        if !execution_payload_provider
            .is_payload_in_cache(&evm_block_hash)
            .await
        {
            debug!(
                "BlockPool: Execution payload for EVM block hash {:?} is not in cache, requesting from the network",
                evm_block_hash
            );

            if let Some(gossip_data_handler) = gossip_data_handler {
                let result = gossip_data_handler
                    .pull_and_add_execution_payload_to_cache(evm_block_hash, use_trusted_peers_only)
                    .await;
                if let Err(e) = response_sender.send(result) {
                    let err_text = format!(
                        "BlockPool: Failed to send response from pull_and_add_execution_payload_to_cache: {:?}",
                        e
                    );
                    error!(err_text);
                    return Err(GossipError::Internal(InternalGossipError::Unknown(
                        err_text,
                    )));
                }
            } else if let Err(send_err) =
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
            debug!(
                "BlockPool: Payload for EVM block hash {:?} is already in cache, no need to request",
                evm_block_hash
            );
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
                None,
            )
            .await
            {
                Ok(()) => {
                    let gossip_payload = execution_payload_provider
                        .get_sealed_block_from_cache(&evm_block_hash)
                        .await
                        .map(GossipBroadcastMessageV2::from);

                    if let Some(payload) = gossip_payload {
                        if let Err(err) = gossip_broadcast_sender.send(payload) {
                            error!(
                                "Block pool: Failed to broadcast execution payload for EVM block hash {:?}: {:?}",
                                evm_block_hash, err
                            );
                        } else {
                            debug!(
                                "Block pool: Broadcasted execution payload for EVM block hash {:?}",
                                evm_block_hash
                            );
                        }
                    }
                }
                Err(err) => {
                    error!(
                        "Block pool: Failed to handle execution payload for EVM block hash {:?}: {:?}",
                        evm_block_hash, err
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
        self.blocks_cache.is_block_processing(block_hash).await
            || self
                .block_status_provider
                .block_status(block_height, block_hash)
                .is_processed()
    }

    /// Inserts an execution payload into the internal cache so that it can be
    /// retrieved by the [`irys_domain::execution_payload_cache::RethBlockProvider`].
    pub async fn add_execution_payload_to_cache(
        &self,
        sealed_block: reth::primitives::SealedBlock<reth::primitives::Block>,
    ) {
        self.execution_payload_provider
            .add_payload_to_cache(sealed_block)
            .await;
    }

    pub async fn get_block_header(
        &self,
        block_hash: &BlockHash,
    ) -> Result<Option<Arc<IrysBlockHeader>>, BlockPoolError> {
        if let Some(header) = self.blocks_cache.get_block_cloned(block_hash).await {
            return Ok(Some(Arc::clone(&header.header)));
        }

        match self.mempool.get_block_header(*block_hash, true).await {
            Ok(Some(header)) => return Ok(Some(Arc::new(header))),
            Ok(None) => {}
            Err(err) => {
                return Err(CriticalBlockPoolError::MempoolError(format!(
                    "Mempool error: {:?}",
                    err
                ))
                .into());
            }
        }

        self.db
            .view_eyre(|tx| block_header_by_hash(tx, block_hash, true))
            .map_err(|db_error| {
                CriticalBlockPoolError::DatabaseError(format!("{:?}", db_error)).into()
            })
            .map(|block| block.map(Arc::new))
    }

    pub async fn get_cached_block_body(&self, block_hash: &BlockHash) -> Option<Arc<BlockBody>> {
        self.blocks_cache
            .get_block_cloned(block_hash)
            .await
            .map(|block| block.block_body)
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
                if let Some(block) = self.blocks_cache.get_block_cloned(&hash).await {
                    orphaned_blocks.push(block);
                }
            }
            Some(orphaned_blocks)
        } else {
            None
        }
    }

    /// Check if parent hash exists in block cache - for orphan block processing
    pub async fn contains_block(&self, block_hash: &BlockHash) -> bool {
        self.blocks_cache.contains_block(block_hash).await
    }

    pub async fn is_block_processing(&self, block_hash: &BlockHash) -> bool {
        self.blocks_cache.is_block_processing(block_hash).await
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
    pub(crate) async fn remove_block_from_cache(
        &self,
        block_hash: &BlockHash,
        reason: BlockRemovalReason,
    ) {
        self.blocks_cache.remove_block(block_hash, reason).await;
        // Remove associated transactions as well
        self.blocks_cache.take_txs_for_block(block_hash).await;
    }

    /// Take cached transactions for a block (removes them from cache).
    /// Used by GossipDataHandler for network-enabled tx fetching.
    pub async fn take_cached_txs_for_block(
        &self,
        block_hash: &BlockHash,
    ) -> Vec<IrysTransactionResponse> {
        self.blocks_cache.take_txs_for_block(block_hash).await
    }

    /// Get a cached block header by hash.
    /// Used by GossipDataHandler for network-enabled tx fetching.
    pub(crate) async fn get_cached_block(&self, block_hash: &BlockHash) -> Option<CachedBlock> {
        self.blocks_cache.get_block_cloned(block_hash).await
    }
}

/// Order pre-fetched transactions into BlockTransactions structure.
///
/// Caller is responsible for providing ALL required transactions.
/// This function only handles ordering them correctly per ledger.
/// Transactions are returned in the exact order specified in the block header,
/// which is critical for commitment transaction validation (e.g., stake must come before pledge).
pub(crate) fn order_transactions_for_block(
    block_header: &IrysBlockHeader,
    data_txs: Vec<irys_types::DataTransactionHeader>,
    commitment_txs: Vec<irys_types::CommitmentTransaction>,
) -> Result<BlockTransactions, BlockPoolError> {
    use std::collections::HashMap;

    // Extract required IDs from block header (preserving order)
    // Use ledger_id-based lookup to avoid relying on vector ordering
    let submit_ids: Vec<H256> = block_header
        .data_ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .map(|l| l.tx_ids.0.clone())
        .unwrap_or_default();

    let publish_ids: Vec<H256> = block_header
        .data_ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::Publish as u32)
        .map(|l| l.tx_ids.0.clone())
        .unwrap_or_default();

    let commitment_ids: Vec<H256> = block_header
        .system_ledgers
        .iter()
        .find(|l| l.ledger_id == SystemLedger::Commitment as u32)
        .map(|l| l.tx_ids.0.clone())
        .unwrap_or_default();

    // Create sets for quick lookup
    let submit_ids_set: HashSet<H256> = submit_ids.iter().copied().collect();
    let publish_ids_set: HashSet<H256> = publish_ids.iter().copied().collect();

    // Collect transactions into maps by ID
    let mut submit_txs_map: HashMap<H256, _> = HashMap::new();
    let mut publish_txs_map: HashMap<H256, _> = HashMap::new();
    let mut commitment_txs_map: HashMap<H256, _> = HashMap::new();

    for data_tx in data_txs {
        // Note: A tx can be in both submit and publish ledgers (published after submission)
        // so we check both independently and clone if needed for both
        let in_submit = submit_ids_set.contains(&data_tx.id);
        let in_publish = publish_ids_set.contains(&data_tx.id);

        if in_submit && in_publish {
            submit_txs_map.insert(data_tx.id, data_tx.clone());
            publish_txs_map.insert(data_tx.id, data_tx);
        } else if in_submit {
            submit_txs_map.insert(data_tx.id, data_tx);
        } else if in_publish {
            publish_txs_map.insert(data_tx.id, data_tx);
        }
    }

    for commitment_tx in commitment_txs {
        commitment_txs_map.insert(commitment_tx.id(), commitment_tx);
    }

    // Build final vectors in the exact order specified by block header
    let submit_txs: Vec<_> = submit_ids
        .iter()
        .filter_map(|id| submit_txs_map.remove(id))
        .collect();

    let publish_txs: Vec<_> = publish_ids
        .iter()
        .filter_map(|id| publish_txs_map.remove(id))
        .collect();

    let commitment_txs: Vec<_> = commitment_ids
        .iter()
        .filter_map(|id| commitment_txs_map.remove(id))
        .collect();

    // Validate header/body consistency: check that resolved counts match expected counts
    if submit_txs.len() != submit_ids.len() {
        let missing_ids: Vec<H256> = submit_ids
            .iter()
            .filter(|id| !submit_txs.iter().any(|tx| &tx.id == *id))
            .copied()
            .collect();
        return Err(BlockPoolError::Critical(
            CriticalBlockPoolError::HeaderBodyMismatch {
                block_hash: block_header.block_hash,
                ledger: "submit".to_string(),
                expected: submit_ids.len(),
                found: submit_txs.len(),
                missing_ids,
            },
        ));
    }

    if publish_txs.len() != publish_ids.len() {
        let missing_ids: Vec<H256> = publish_ids
            .iter()
            .filter(|id| !publish_txs.iter().any(|tx| &tx.id == *id))
            .copied()
            .collect();
        return Err(BlockPoolError::Critical(
            CriticalBlockPoolError::HeaderBodyMismatch {
                block_hash: block_header.block_hash,
                ledger: "publish".to_string(),
                expected: publish_ids.len(),
                found: publish_txs.len(),
                missing_ids,
            },
        ));
    }

    if commitment_txs.len() != commitment_ids.len() {
        let missing_ids: Vec<H256> = commitment_ids
            .iter()
            .filter(|id| !commitment_txs.iter().any(|tx| &tx.id() == *id))
            .copied()
            .collect();
        return Err(BlockPoolError::Critical(
            CriticalBlockPoolError::HeaderBodyMismatch {
                block_hash: block_header.block_hash,
                ledger: "commitment".to_string(),
                expected: commitment_ids.len(),
                found: commitment_txs.len(),
                missing_ids,
            },
        ));
    }

    Ok(BlockTransactions {
        commitment_txs,
        data_txs: HashMap::from([
            (DataLedger::Submit, submit_txs),
            (DataLedger::Publish, publish_txs),
        ]),
        custody_proofs: Vec::new(),
    })
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
            Err(BlockPoolError::Advisory(
                AdvisoryBlockPoolError::AlreadyProcessed(block_hash),
            ))
        }
        BlockStatus::Finalized => {
            debug!(
                "Block pool: Block at height {} is finalized and cannot be reorganized (Tried to process block {:?})",
                block_height, block_hash,
            );
            Err(CriticalBlockPoolError::TryingToReprocessFinalizedBlock(block_hash).into())
        }
        BlockStatus::PartOfAPrunedFork => {
            debug!(
                "Block pool: Block {:?} (height {}) is part of a pruned fork",
                block_hash, block_height,
            );
            Err(CriticalBlockPoolError::ForkedBlock(block_hash).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{CommitmentTransactionV2, DataTransactionHeader, IrysBlockHeaderV1};
    use std::sync::Arc;

    fn make_header(block_byte: u8, parent_byte: u8, height: u64) -> Arc<IrysBlockHeader> {
        let header = IrysBlockHeader::V1(IrysBlockHeaderV1 {
            height,
            block_hash: BlockHash::repeat_byte(block_byte),
            previous_block_hash: BlockHash::repeat_byte(parent_byte),
            ..IrysBlockHeaderV1::default()
        });
        Arc::new(header)
    }

    #[test]
    fn add_single_block() {
        let mut cache = BlockCacheInner::new();
        let parent = BlockHash::repeat_byte(0xAA);
        let child1 = make_header(0x01, 0xAA, 10);

        cache.add_block(child1.clone(), Default::default(), false);

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

        cache.add_block(child1.clone(), Default::default(), true); // fast track first
        cache.add_block(child2.clone(), Default::default(), false); // second sibling

        let set = cache
            .orphaned_blocks_by_parent
            .get(&parent)
            .expect("parent entry should exist");
        assert!(set.contains(&child1.block_hash));
        assert!(set.contains(&child2.block_hash));

        // Both sibling blocks should be stored in blocks cache
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
        cache.add_block(child1.clone(), Default::default(), false);
        cache.add_block(child2.clone(), Default::default(), false);

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
        cache.add_block(block.clone(), Default::default(), false);

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
        cache.add_block(child.clone(), Default::default(), false);
        // Sanity: parent entry exists
        assert!(cache.orphaned_blocks_by_parent.get(&parent).is_some());
        // Remove only child
        cache.remove_block(&child.block_hash);
        // Parent entry should be removed entirely
        assert!(cache.orphaned_blocks_by_parent.get(&parent).is_none());
    }

    #[test]
    fn block_body_storage_and_retrieval() {
        let mut cache = BlockCacheInner::new();
        let _parent = BlockHash::repeat_byte(0xFA);
        let child1 = make_header(0x50, 0xFA, 100);

        // Create a non-empty BlockBody
        let block_body = Arc::new(BlockBody {
            block_hash: child1.block_hash,
            data_transactions: vec![DataTransactionHeader::default()],
            commitment_transactions: vec![],
            custody_proofs: vec![],
        });

        // Add block with the BlockBody
        cache.add_block(child1.clone(), block_body.clone(), false);

        // Retrieve the cached block
        let cached = cache
            .blocks
            .get(&child1.block_hash)
            .expect("child1 should be stored in blocks cache");

        // Verify BlockBody can be retrieved and matches what was inserted
        assert_eq!(cached.block_body.block_hash, child1.block_hash);
        assert_eq!(cached.block_body.data_transactions.len(), 1);
        assert_eq!(cached.block_body.commitment_transactions.len(), 0);

        // Verify the BlockBody is the same Arc reference
        assert!(Arc::ptr_eq(&cached.block_body, &block_body));
    }

    #[test]
    fn block_body_storage_with_default() {
        let mut cache = BlockCacheInner::new();
        let _parent = BlockHash::repeat_byte(0xFB);
        let child1 = make_header(0x51, 0xFB, 101);

        // Add block with Default::default() BlockBody (as used in existing call sites)
        cache.add_block(child1.clone(), Default::default(), false);

        // Retrieve the cached block
        let cached = cache
            .blocks
            .get(&child1.block_hash)
            .expect("child1 should be stored in blocks cache");

        // Verify default BlockBody is stored
        assert_eq!(cached.block_body.block_hash, BlockHash::default());
        assert_eq!(cached.block_body.data_transactions.len(), 0);
        assert_eq!(cached.block_body.commitment_transactions.len(), 0);
    }

    #[test]
    fn order_transactions_matching_header_body() {
        use irys_types::{
            CommitmentTransaction, CommitmentV2WithMetadata, DataTransactionHeaderV1,
            DataTransactionHeaderV1WithMetadata, DataTransactionMetadata, SystemTransactionLedger,
        };

        // Create test transaction IDs
        let submit_tx_id1 = H256::repeat_byte(0x11);
        let submit_tx_id2 = H256::repeat_byte(0x12);
        let publish_tx_id1 = H256::repeat_byte(0x21);
        let commitment_tx_id1 = H256::repeat_byte(0x31);

        // Create block header with specific transaction ordering
        let header = IrysBlockHeaderV1 {
            block_hash: BlockHash::repeat_byte(0xAA),
            height: 50,
            data_ledgers: vec![
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Publish as u32, // Index 0
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![publish_tx_id1]),
                    total_chunks: 1,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Submit as u32, // Index 1
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![submit_tx_id2, submit_tx_id1]), // Note: reversed order
                    total_chunks: 2,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            system_ledgers: vec![SystemTransactionLedger {
                ledger_id: 0, // SystemLedger::Commitment
                tx_ids: irys_types::H256List(vec![commitment_tx_id1]),
            }],
            ..Default::default()
        };

        let header = IrysBlockHeader::V1(header);

        // Create matching transactions (deliberately in different order than header)
        let data_txs = vec![
            DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: submit_tx_id1,
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            }),
            DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: submit_tx_id2,
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            }),
            DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: publish_tx_id1,
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            }),
        ];

        let commitment_tx = CommitmentTransactionV2 {
            id: commitment_tx_id1,
            ..Default::default()
        };
        let commitment_txs = vec![CommitmentTransaction::V2(CommitmentV2WithMetadata {
            tx: commitment_tx,
            metadata: Default::default(),
        })];

        // Execute ordering function
        let result = order_transactions_for_block(&header, data_txs, commitment_txs)
            .expect("Should succeed with matching header/body");

        // Verify transaction ordering matches header's ledger ID order
        let submit_txs = result.data_txs.get(&DataLedger::Submit).unwrap();
        assert_eq!(submit_txs.len(), 2);
        assert_eq!(submit_txs[0].id, submit_tx_id2); // First in header order
        assert_eq!(submit_txs[1].id, submit_tx_id1); // Second in header order

        let publish_txs = result.data_txs.get(&DataLedger::Publish).unwrap();
        assert_eq!(publish_txs.len(), 1);
        assert_eq!(publish_txs[0].id, publish_tx_id1);

        assert_eq!(result.commitment_txs.len(), 1);
        assert_eq!(result.commitment_txs[0].id(), commitment_tx_id1);
    }

    #[test]
    fn order_transactions_header_body_mismatch_missing_tx() {
        use irys_types::{
            DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
        };

        // Create test transaction IDs
        let submit_tx_id1 = H256::repeat_byte(0x11);
        let submit_tx_id2 = H256::repeat_byte(0x12); // This will be missing from body

        // Create block header expecting two submit transactions
        let header = IrysBlockHeaderV1 {
            block_hash: BlockHash::repeat_byte(0xBB),
            height: 51,
            // Only include Submit ledger at correct index (1)
            // Need to have Publish at index 0 (even if empty) since Submit is at index 1
            data_ledgers: vec![
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Publish as u32,
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![]),
                    total_chunks: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Submit as u32,
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![submit_tx_id1, submit_tx_id2]),
                    total_chunks: 2,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            system_ledgers: vec![],
            ..Default::default()
        };

        let header = IrysBlockHeader::V1(header);

        // Create body with only ONE transaction (mismatch)
        let data_txs = vec![DataTransactionHeader::V1(
            DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: submit_tx_id1,
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            },
        )];

        let commitment_txs = vec![];

        // Execute ordering function - it should return an error
        let result = order_transactions_for_block(&header, data_txs, commitment_txs);

        // Verify the function returns an error for the mismatch
        assert!(
            result.is_err(),
            "Should return error for header/body mismatch"
        );

        if let Err(BlockPoolError::Critical(CriticalBlockPoolError::HeaderBodyMismatch {
            ledger,
            expected,
            found,
            missing_ids,
            ..
        })) = result
        {
            assert_eq!(ledger, "submit");
            assert_eq!(expected, 2);
            assert_eq!(found, 1);
            assert_eq!(missing_ids.len(), 1);
            assert!(missing_ids.contains(&submit_tx_id2));
        } else {
            panic!("Expected HeaderBodyMismatch error");
        }
    }

    #[test]
    fn order_transactions_header_body_mismatch_wrong_ledger() {
        use irys_types::{
            DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
        };

        // Create test transaction IDs
        let tx_id1 = H256::repeat_byte(0x13);

        // Create block header expecting transaction in Submit ledger
        let header = IrysBlockHeaderV1 {
            block_hash: BlockHash::repeat_byte(0xCC),
            height: 52,
            // Need Publish at index 0 (empty) and Submit at index 1
            data_ledgers: vec![
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Publish as u32,
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![]),
                    total_chunks: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Submit as u32,
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![tx_id1]),
                    total_chunks: 1,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            system_ledgers: vec![],
            ..Default::default()
        };

        let header = IrysBlockHeader::V1(header);

        // Create body with transaction that has a different ID than expected
        // (simulating the transaction being missing from expected ledger)
        let data_txs = vec![DataTransactionHeader::V1(
            DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: H256::repeat_byte(0x99), // Different ID, not in any expected ledger
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            },
        )];

        let commitment_txs = vec![];

        // Execute ordering function - since we're providing tx with wrong ID,
        // it gets ignored. We get error for missing tx in Submit.
        let result = order_transactions_for_block(&header, data_txs, commitment_txs);

        // This should return an error because tx_id1 is expected in Submit but not provided
        assert!(
            result.is_err(),
            "Should return error when expected transaction is missing"
        );

        if let Err(BlockPoolError::Critical(CriticalBlockPoolError::HeaderBodyMismatch {
            ledger,
            expected,
            found,
            ..
        })) = result
        {
            assert_eq!(ledger, "submit");
            assert_eq!(expected, 1);
            assert_eq!(found, 0); // Transaction not found in expected position
        } else {
            panic!("Expected HeaderBodyMismatch error");
        }
    }

    #[test]
    fn order_transactions_commitment_mismatch() {
        use irys_types::{CommitmentTransaction, CommitmentTransactionV2, SystemTransactionLedger};

        // Create test commitment transaction IDs
        let commitment_tx_id1 = H256::repeat_byte(0x41);
        let commitment_tx_id2 = H256::repeat_byte(0x42); // This will be missing

        // Create block header expecting two commitment transactions
        let header = IrysBlockHeaderV1 {
            block_hash: BlockHash::repeat_byte(0xDD),
            height: 53,
            data_ledgers: vec![],
            system_ledgers: vec![SystemTransactionLedger {
                ledger_id: 0, // SystemLedger::Commitment
                tx_ids: irys_types::H256List(vec![commitment_tx_id1, commitment_tx_id2]),
            }],
            ..Default::default()
        };

        let header = IrysBlockHeader::V1(header);

        // Create body with only ONE commitment transaction
        let data_txs = vec![];

        let commitment_tx = CommitmentTransactionV2 {
            id: commitment_tx_id1,
            ..Default::default()
        };
        let commitment_txs = vec![CommitmentTransaction::V2(
            irys_types::CommitmentV2WithMetadata {
                tx: commitment_tx,
                metadata: Default::default(),
            },
        )];
        let result = order_transactions_for_block(&header, data_txs, commitment_txs);

        // Verify the function returns an error for the mismatch
        assert!(
            result.is_err(),
            "Should return error for commitment mismatch"
        );

        if let Err(BlockPoolError::Critical(CriticalBlockPoolError::HeaderBodyMismatch {
            ledger,
            expected,
            found,
            missing_ids,
            ..
        })) = result
        {
            assert_eq!(ledger, "commitment");
            assert_eq!(expected, 2);
            assert_eq!(found, 1);
            assert_eq!(missing_ids.len(), 1);
            assert!(missing_ids.contains(&commitment_tx_id2));
        } else {
            panic!("Expected HeaderBodyMismatch error");
        }
    }

    #[test]
    fn order_transactions_tx_in_both_ledgers() {
        use irys_types::{
            DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
        };

        // Create a transaction ID that appears in both Submit and Publish
        let dual_tx_id = H256::repeat_byte(0x77);

        // Create block header with transaction in BOTH ledgers (Publish at index 0, Submit at index 1)
        let header = IrysBlockHeaderV1 {
            block_hash: BlockHash::repeat_byte(0xEE),
            height: 54,
            data_ledgers: vec![
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Publish as u32,
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![dual_tx_id]),
                    total_chunks: 1,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
                irys_types::DataTransactionLedger {
                    ledger_id: DataLedger::Submit as u32,
                    tx_root: H256::zero(),
                    tx_ids: irys_types::H256List(vec![dual_tx_id]),
                    total_chunks: 1,
                    expires: None,
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            system_ledgers: vec![],
            ..Default::default()
        };

        let header = IrysBlockHeader::V1(header);

        // Provide the transaction once in the body
        let data_txs = vec![DataTransactionHeader::V1(
            DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: dual_tx_id,
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            },
        )];

        let commitment_txs = vec![];

        // Execute ordering function
        let result = order_transactions_for_block(&header, data_txs, commitment_txs)
            .expect("Should succeed with transaction in both ledgers");

        // Verify the transaction appears in BOTH ledgers (cloned)
        let submit_txs = result.data_txs.get(&DataLedger::Submit).unwrap();
        assert_eq!(submit_txs.len(), 1);
        assert_eq!(submit_txs[0].id, dual_tx_id);

        let publish_txs = result.data_txs.get(&DataLedger::Publish).unwrap();
        assert_eq!(publish_txs.len(), 1);
        assert_eq!(publish_txs[0].id, dual_tx_id);

        // This tests the code path that handles transactions in both ledgers
    }
}
