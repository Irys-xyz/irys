use crate::{
    block_index_service::{BlockIndexReadGuard, BlockIndexService},
    chunk_migration_service::ChunkMigrationService,
    ema_service::EmaServiceMessage,
    mempool_service::MempoolServiceMessage,
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    validation_service::{RequestValidationMessage, ValidationService},
    BlockFinalizedMessage,
};
use actix::prelude::*;
use base58::ToBase58 as _;
use eyre::ensure;
use futures::future::Either;
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _, tx_header_by_txid};
use irys_types::{
    Address, BlockHash, Config, ConsensusConfig, DataLedger, DatabaseProvider, IrysBlockHeader,
    IrysTransactionHeader, IrysTransactionId, H256, U256,
};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use reth_db::{transaction::DbTx, Database as _};
use std::pin::pin;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, RwLock, RwLockReadGuard},
    time::SystemTime,
};
use tokio::{
    sync::{mpsc::UnboundedReceiver, oneshot},
    task::JoinHandle,
};
use tracing::{debug, error, info};

/// Wraps the internal `Arc<RwLock<_>>` to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct BlockTreeReadGuard {
    block_tree_cache: Arc<RwLock<BlockTreeCache>>,
}

impl BlockTreeReadGuard {
    /// Creates a new `ReadGuard` for the `block_tree` cache
    pub const fn new(block_tree_cache: Arc<RwLock<BlockTreeCache>>) -> Self {
        Self { block_tree_cache }
    }

    /// Accessor method to get a read guard for the `block_tree` cache
    pub fn read(&self) -> RwLockReadGuard<'_, BlockTreeCache> {
        self.block_tree_cache.read().unwrap()
    }

    #[cfg(any(test, feature = "test-utils"))]
    /// Accessor method to get a write guard for the `block_tree` cache
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, BlockTreeCache> {
        self.block_tree_cache.write().unwrap()
    }
}

// Messages that the CommitmentCache service supports
#[derive(Debug)]
pub enum BlockTreeServiceMessage {
    GetBlockTreeReadGuard {
        response: oneshot::Sender<BlockTreeReadGuard>,
    },
    BlockPreValidated {
        block: Arc<IrysBlockHeader>,
        all_txs: Arc<Vec<IrysTransactionHeader>>,
        response: oneshot::Sender<eyre::Result<()>>,
    },
    BlockValidationFinished {
        block_hash: H256,
        validation_result: ValidationResult,
    },
}

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockTreeService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<BlockTreeServiceMessage>,
    inner: BlockTreeServiceInner,
}

#[derive(Debug)]
pub struct BlockTreeServiceInner {
    db: DatabaseProvider,
    /// Block tree internal state
    pub cache: Arc<RwLock<BlockTreeCache>>,
    /// The wallet address of the local miner
    pub miner_address: Address,
    /// Read view of the `block_index`
    pub block_index_guard: BlockIndexReadGuard,
    /// Global storage config
    pub consensus_config: ConsensusConfig,
    /// Channels for communicating with the services
    pub service_senders: ServiceSenders,
    /// Reth service actor sender
    pub reth_service_actor: Addr<RethServiceActor>,
    /// Current actix system
    pub system: System,
}

#[derive(Debug, Clone)]
pub struct ReorgEvent {
    pub old_fork: Arc<Vec<Arc<IrysBlockHeader>>>,
    pub new_fork: Arc<Vec<Arc<IrysBlockHeader>>>,
    pub fork_parent: Arc<IrysBlockHeader>,
    pub new_tip: BlockHash,
    pub timestamp: SystemTime,
}

#[derive(Debug, Clone)]
pub struct BlockMigratedEvent {
    pub block: Arc<IrysBlockHeader>,
}

impl BlockTreeService {
    /// Spawn a new BlockTree service
    pub fn spawn_service(
        exec: &TaskExecutor,
        rx: UnboundedReceiver<BlockTreeServiceMessage>,
        db: DatabaseProvider,
        block_index_guard: BlockIndexReadGuard,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_service_actor: Addr<RethServiceActor>,
    ) -> JoinHandle<()> {
        // Dereference miner_address here, before the closure
        let miner_address = config.node_config.miner_address();
        let consensus_config = config.node_config.consensus_config().clone();
        let service_senders = service_senders.clone();
        let system = System::current();
        exec.spawn_critical_with_graceful_shutdown_signal(
            "BlockTree Service",
            |shutdown| async move {
                let cache = BlockTreeCache::initialize_from_list(
                    block_index_guard.clone(),
                    reth_service_actor.clone(),
                    db.clone(),
                );

                let block_tree_service = Self {
                    shutdown,
                    msg_rx: rx,
                    inner: BlockTreeServiceInner {
                        db,
                        cache: Arc::new(RwLock::new(cache)),
                        miner_address,
                        block_index_guard,
                        consensus_config,
                        service_senders,
                        reth_service_actor,
                        system,
                    },
                };
                block_tree_service
                    .start()
                    .await
                    .expect("BlockTree encountered an irrecoverable error")
            },
        )
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting BlockTree service");

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg).await?;
                }
                Either::Left((None, _)) => {
                    tracing::warn!("receiver channel closed");
                    break None;
                }
                Either::Right((shutdown, _)) => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down BlockTree service");
        Ok(())
    }
}

impl BlockTreeServiceInner {
    /// Dispatches received messages to appropriate handler methods and sends responses
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: BlockTreeServiceMessage) -> eyre::Result<()> {
        match msg {
            BlockTreeServiceMessage::GetBlockTreeReadGuard { response } => {
                let guard = BlockTreeReadGuard::new(self.cache.clone());
                let _ = response.send(guard);
            }
            BlockTreeServiceMessage::BlockPreValidated {
                block,
                all_txs,
                response,
            } => {
                let result = self.on_block_prevalidated(block, all_txs).await;
                let _ = response.send(result);
            }
            BlockTreeServiceMessage::BlockValidationFinished {
                block_hash,
                validation_result,
            } => {
                self.on_block_validation_finished(block_hash, validation_result);
            }
        }
        Ok(())
    }

    fn send_storage_finalized_message(&self, block_hash: BlockHash) -> eyre::Result<()> {
        let tx = self
            .db
            .clone()
            .tx()
            .map_err(|e| eyre::eyre!("Failed to create transaction: {}", e))?;

        let block_header = match block_header_by_hash(&tx, &block_hash, false) {
            Ok(Some(header)) => header,
            Ok(None) => {
                return Err(eyre::eyre!("No block header found for hash {}", block_hash));
            }
            Err(e) => {
                return Err(eyre::eyre!("Failed to get previous block header: {}", e));
            }
        };

        // Get all the transactions for the finalized block, error if not found
        // TODO: Eventually abstract this for support of `n` ledgers
        let submit_txs = get_ledger_tx_headers(&tx, &block_header, DataLedger::Submit);
        let publish_txs = get_ledger_tx_headers(&tx, &block_header, DataLedger::Publish);

        let all_txs = {
            let mut combined = submit_txs.unwrap_or_default();
            combined.extend(publish_txs.unwrap_or_default());
            combined
        };

        info!(
            "Migrating to block_index - hash: {} height: {}",
            &block_header.block_hash.0.to_base58(),
            &block_header.height
        );

        // HACK
        System::set_current(self.system.clone());

        let chunk_migration = ChunkMigrationService::from_registry();
        let block_index = BlockIndexService::from_registry();
        let block_finalized_message = BlockFinalizedMessage {
            block_header: Arc::new(block_header),
            all_txs: Arc::new(all_txs),
        };

        block_index.do_send(block_finalized_message.clone());
        chunk_migration.do_send(block_finalized_message);
        Ok(())
    }

    fn notify_services_of_block_confirmation(
        &self,
        tip_hash: BlockHash,
        confirmed_block: &Arc<IrysBlockHeader>,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
    ) {
        debug!(
            "JESSEDEBUG confirming irys block evm_block_hash: {} ({})",
            &confirmed_block.evm_block_hash, &confirmed_block.height
        );
        if let Err(e) = self.reth_service_actor.try_send(ForkChoiceUpdateMessage {
            head_hash: BlockHashType::Irys(tip_hash),
            confirmed_hash: Some(BlockHashType::Evm(confirmed_block.evm_block_hash)),
            finalized_hash: None,
        }) {
            panic!(
                "Unable to send confirmation FCU message to reth for {}: {}",
                &tip_hash, &e
            )
        }
        self.service_senders
            .mempool
            .send(MempoolServiceMessage::BlockConfirmedMessage(
                confirmed_block.clone(),
                all_tx,
            ))
            .expect("mempool service has unexpectedly become unreachable");
        self.service_senders
            .ema
            .send(EmaServiceMessage::BlockConfirmed)
            .expect("EMA service has unexpectedly become unreachable");
    }

    /// Checks if a block that is `chunk_migration_depth` blocks behind `arc_block`
    /// should be finalized. If eligible, sends finalization message unless block
    /// is already in `block_index`. Panics if the `block_tree` and `block_index` are
    /// inconsistent.
    fn try_notify_services_of_block_finalization(
        &self,
        arc_block: &Arc<IrysBlockHeader>,
        cache: &BlockTreeCache,
    ) {
        let migration_depth = self.consensus_config.chunk_migration_depth as usize;

        // Skip if block isn't deep enough for finalization
        if arc_block.height <= migration_depth as u64 {
            return;
        }

        let (longest_chain, _) = cache.get_canonical_chain();
        if longest_chain.len() <= migration_depth {
            return;
        }

        // Find block to finalize
        let Some(current_index) = longest_chain
            .iter()
            .position(|x| x.0 == arc_block.block_hash)
        else {
            info!(
                "Validated block not in longest chain, block {} height: {}, skipping finalization",
                arc_block.block_hash, arc_block.height
            );
            return;
        };

        if current_index < migration_depth {
            return; // Block already finalized
        }

        let finalize_index = current_index - migration_depth;
        let (finalized_hash, finalized_height) = (
            longest_chain[finalize_index].0,
            longest_chain[finalize_index].1,
        );

        // Verify block isn't already finalized
        let binding = self.block_index_guard.clone();
        let bi = binding.read();
        if bi.num_blocks() > finalized_height && bi.num_blocks() > finalized_height {
            let finalized = bi.get_item(finalized_height).unwrap();
            if finalized.block_hash == finalized_hash {
                return;
            }
            panic!("Block tree and index out of sync");
        }

        match cache.get_block(&finalized_hash) {
            Some(block) => {
                let mut block = block.clone();
                block.poa.chunk = None;
                let migrated_block = Arc::new(block);
                // Broadcast BlockMigratedEvent event using the shared sender
                let block_migrated_event = BlockMigratedEvent {
                    block: migrated_block.clone(),
                };
                if let Err(e) = self
                    .service_senders
                    .block_migrated_events
                    .send(block_migrated_event)
                {
                    debug!("No reorg subscribers: {:?}", e);
                }
            }
            None => error!("migrated block {} not found in block_tree", finalized_hash),
        }

        debug!(?finalized_hash, ?finalized_height, "migrating irys block");
        // TODO: this is the wrong place for this, it should be at the prune depth not the chunk_migration depth
        if let Err(e) = self.reth_service_actor.try_send(ForkChoiceUpdateMessage {
            head_hash: BlockHashType::Irys(cache.tip),
            confirmed_hash: None,
            finalized_hash: Some(BlockHashType::Irys(finalized_hash)),
        }) {
            panic!("Unable to send finalization message to reth: {}", &e)
        }

        if self.send_storage_finalized_message(finalized_hash).is_err() {
            error!("Unable to send block finalized message");
        }
    }

    /// Handles pre-validated blocks received from the validation service.
    ///
    /// The handling differs based on whether the block was produced locally:
    /// - For locally mined blocks: Added as `BlockState::Unknown` to allow chain extension
    ///   while validation is pending
    /// - For peer blocks: Added normally via `add_block`
    ///
    /// After adding the block, it's scheduled for full validation and the previous
    /// block is marked for storage finalization.
    async fn on_block_prevalidated(
        &mut self,
        block: Arc<IrysBlockHeader>,
        all_txs: Arc<Vec<IrysTransactionHeader>>,
    ) -> eyre::Result<()> {
        let miner_address = self.miner_address;
        let ema_service = self.service_senders.ema.clone();

        let block_hash = &block.block_hash;

        // Scope the cache lock so it's dropped before the await
        let should_update_ema = {
            let mut cache = self.cache.write().expect("cache lock poisoned");

            //  Do we already have this block
            let existing = cache.get_block(block_hash);
            if let Some(existing) = existing {
                // TODO: block_discovery should not be sending us blocks we already know about >_<
                debug!(
                    "on_block_prevalidated: {} at height: {} already in block_tree",
                    existing.block_hash, existing.height
                );
                return Ok(());
            }

            // Handle block addition differently based on origin
            let add_result = if block.miner_address == miner_address {
                // For locally mined blocks: Add as `BlockState::Unknown `to allow chain
                // extension while full validation is still pending. This prevents blocking
                // new block production while validation completes.
                cache.add_validated_block((*block).clone(), BlockState::Unknown, all_txs)
            } else {
                // For blocks from peers: Add via standard path requiring validation
                cache.add_block(&block, all_txs)
            };

            if add_result.is_ok() {
                // Schedule block for full validation regardless of origin
                // HACK
                System::set_current(self.system.clone());
                let validation_service = ValidationService::from_registry();
                validation_service.do_send(RequestValidationMessage(block.clone()));

                // Update block state to reflect scheduled validation
                if cache
                    .mark_block_as_validation_scheduled(block_hash)
                    .is_err()
                {
                    error!("Unable to mark block as ValidationScheduled");
                }
                debug!(
                    "scheduling block for validation: {} height: {}",
                    block_hash, block.height
                );
                true
            } else {
                false
            }
        }; // cache lock is dropped here

        // Only update EMA if block was successfully added
        if should_update_ema {
            // block until EMA service is updated
            let (tx, rx) = tokio::sync::oneshot::channel();
            ema_service.send(EmaServiceMessage::NewPrevalidatedBlock { response: tx })?;
            rx.await?;
        }

        Ok(())
    }

    /// Handles the completion of full block validation.
    ///
    /// When a block passes validation:
    /// 1. Updates the block's state in the cache to `ValidBlock`
    /// 2. Moves the tip of the chain to this block if it is now the head of the longest chain
    /// 3. If the tip moves, checks whether it's a simple extension or a reorganization:
    ///    - **Extension**: The new tip's parent is the current tip
    ///    - **Reorg**: The new tip's parent is not the current tip (fork switch)
    /// 4. For reorgs, broadcasts a `ReorgEvent` containing:
    ///    - Blocks from the old fork (now orphaned)
    ///    - Blocks from the new fork (now canonical)
    ///    - The common ancestor where the fork occurred
    /// 5. Notifies services of the new confirmed block
    /// 6. Checks if any blocks are deep enough to be finalized to storage
    ///
    /// Invalid blocks are logged but currently remain in the cache.
    fn on_block_validation_finished(
        &mut self,
        block_hash: H256,
        validation_result: ValidationResult,
    ) {
        debug!(
            "\u{001b}[32mOn validation complete : result {} {:?}\u{001b}[0m",
            block_hash, validation_result
        );
        match validation_result {
            ValidationResult::Invalid => {
                // Do nothing - TODO probably remove from cache
                error!("{} INVALID BLOCK", block_hash.0.to_base58());
            }
            ValidationResult::Valid => {
                let binding = self.cache.clone();
                let mut cache = binding.write().unwrap();

                // Get the current tip before any changes
                // Note: We cant rely on canonical chain here, because the canonical chain was already updated when this
                //       block arrived and was added after pre-validation. The tip only moves after full validation.
                let old_tip = cache.tip;
                let old_tip_block = cache.get_block(&old_tip).unwrap().clone();

                // Mark block as validated in cache, this will update the canonical chain
                if let Err(err) = cache.mark_block_as_valid(&block_hash) {
                    error!("{}", err);
                    return;
                }

                // let (longest_chain, not_on_chain_count) = cache.get_canonical_chain();
                let Some((_block_entry, fork_blocks, _)) =
                    cache.get_earliest_not_onchain_in_longest_chain()
                else {
                    if block_hash == old_tip {
                        debug!(
                            "\u{001b}[32mSame Tip Marked current tip {} cdiff: {} height: {}\u{001b}[0m",
                            block_hash,
                            old_tip_block.cumulative_diff,
                            old_tip_block.height
                         );
                    } else {
                        debug!(
                            "\u{001b}[32mNo new tip found {}, current tip {} cdiff: {} height: {}\u{001b}[0m",
                            block_hash,
                            old_tip_block.block_hash,
                            old_tip_block.cumulative_diff,
                            old_tip_block.height
                        );
                    }
                    return;
                };

                // if the old tip isn't in the fork_blocks, it's a reorg
                let is_reorg = !fork_blocks.iter().any(|bh| bh.block_hash == old_tip);

                // Get block info before mutable operations
                let block_entry = cache.blocks.get(&block_hash).unwrap();
                let arc_block = Arc::new(block_entry.block.clone());
                let all_tx = block_entry.all_tx.clone();

                // Now do mutable operations
                if cache.mark_tip(&block_hash).is_ok() {
                    if is_reorg {
                        let mut orphaned_blocks = cache.get_fork_blocks(&old_tip_block);
                        orphaned_blocks.push(&old_tip_block);

                        let fork_hash = orphaned_blocks.first().unwrap().block_hash;
                        let fork_block = cache.get_block(&fork_hash).unwrap();
                        let fork_height = fork_block.height;

                        // Populate `old_canonical` by converting each orphaned block into a `ChainCacheEntry`.
                        let mut old_canonical = Vec::with_capacity(orphaned_blocks.len());
                        for block in &orphaned_blocks {
                            let entry = make_chain_cache_entry(block);
                            old_canonical.push(entry);
                        }
                        let new_canonical = cache.get_canonical_chain();

                        let (old_fork, new_fork) = prune_chains_at_ancestor(
                            old_canonical,
                            new_canonical.0,
                            fork_hash,
                            fork_height,
                        );

                        // Build Arc'd IrysBlockHeader lists for the ReorgEvent to minimize overhead of cloning
                        let old_fork_blocks: Vec<Arc<IrysBlockHeader>> = old_fork
                            .iter()
                            .map(|e| {
                                let mut block = cache.get_block(&e.0).unwrap().clone();
                                block.poa.chunk = None; // Strip out the chunk
                                Arc::new(block)
                            })
                            .collect();

                        let new_fork_blocks: Vec<Arc<IrysBlockHeader>> = new_fork
                            .iter()
                            .map(|e| {
                                let mut block = cache.get_block(&e.0).unwrap().clone();
                                block.poa.chunk = None; // Strip out the chunk
                                Arc::new(block)
                            })
                            .collect();

                        debug!(
                            "\u{001b}[32mReorg at block height {} with {}\u{001b}[0m",
                            arc_block.height, arc_block.block_hash
                        );
                        let event = ReorgEvent {
                            old_fork: Arc::new(old_fork_blocks),
                            new_fork: Arc::new(new_fork_blocks),
                            fork_parent: Arc::new(fork_block.clone()),
                            new_tip: block_hash,
                            timestamp: SystemTime::now(),
                        };

                        // Broadcast reorg event using the shared sender
                        if let Err(e) = self.service_senders.reorg_events.send(event) {
                            debug!("No reorg subscribers: {:?}", e);
                        }
                    } else {
                        debug!(
                            "\u{001b}[32mExtending longest chain to height {} with {} parent: {} height: {}\u{001b}[0m",
                            arc_block.height, arc_block.block_hash, old_tip_block.block_hash, old_tip_block.height
                        );
                    }

                    self.notify_services_of_block_confirmation(block_hash, &arc_block, all_tx);
                }

                // Handle block finalization (move chunks to disk and add to block_index)
                self.try_notify_services_of_block_finalization(&arc_block, &cache);
            }
        }
    }
}

fn make_chain_cache_entry(block: &IrysBlockHeader) -> ChainCacheEntry {
    let publish_txs = block.data_ledgers[DataLedger::Publish].tx_ids.0.clone();

    let submit_txs = block.data_ledgers[DataLedger::Submit].tx_ids.0.clone();

    (block.block_hash, block.height, publish_txs, submit_txs)
}

/// Prunes two canonical chains at the specified common ancestor, returning only the divergent portions
/// Returns (old_chain_from_fork, new_chain_from_fork)
pub fn prune_chains_at_ancestor(
    old_chain: Vec<ChainCacheEntry>,
    new_chain: Vec<ChainCacheEntry>,
    ancestor_hash: BlockHash,
    ancestor_height: u64,
) -> (Vec<ChainCacheEntry>, Vec<ChainCacheEntry>) {
    // Find the ancestor index in the old chain
    let old_ancestor_idx = old_chain
        .iter()
        .position(|(hash, height, _, _)| *hash == ancestor_hash && *height == ancestor_height)
        .expect("Common ancestor should exist in old chain");

    // Find the ancestor index in the new chain
    let new_ancestor_idx = new_chain
        .iter()
        .position(|(hash, height, _, _)| *hash == ancestor_hash && *height == ancestor_height)
        .expect("Common ancestor should exist in new chain");

    // Return the portions after the common ancestor (excluding the ancestor itself)
    let old_divergent = old_chain[old_ancestor_idx + 1..].to_vec();
    let new_divergent = new_chain[new_ancestor_idx + 1..].to_vec();

    (old_divergent, new_divergent)
}

#[derive(Debug)]
pub enum ValidationResult {
    Valid,
    Invalid,
}

/// Fetches full transaction headers from a ledger in a block.
/// Returns None if any headers are missing or on DB errors.
fn get_ledger_tx_headers<T: DbTx>(
    tx: &T,
    block_header: &IrysBlockHeader,
    ledger: DataLedger,
) -> Option<Vec<IrysTransactionHeader>> {
    match block_header.data_ledgers[ledger]
        .tx_ids
        .iter()
        .map(|txid| {
            tx_header_by_txid(tx, txid)
                .map_err(|e| eyre::eyre!("Failed to get tx header: {}", e))
                .and_then(|opt| {
                    opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                })
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(txs) => Some(txs),
        Err(e) => {
            error!(
                "Failed to collect tx headers for {:?} ledger: {}",
                ledger, e
            );
            None
        }
    }
}

/// Number of blocks to retain in cache from chain head
const BLOCK_CACHE_DEPTH: u64 = 50;

type ChainCacheEntry = (
    BlockHash,
    u64,
    Vec<IrysTransactionId>,
    Vec<IrysTransactionId>,
); // (block_hash, height, publish_txs, submit_txs)

#[derive(Debug)]
pub struct BlockTreeCache {
    // Main block storage
    blocks: HashMap<BlockHash, BlockEntry>,

    // Track solutions -> block hashes
    solutions: HashMap<H256, HashSet<BlockHash>>,

    // Current tip
    pub tip: BlockHash,

    // Track max cumulative difficulty
    max_cumulative_difficulty: (U256, BlockHash), // (difficulty, block_hash)

    // Height -> Hash mapping
    height_index: BTreeMap<u64, HashSet<BlockHash>>,

    // Cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    longest_chain_cache: (Vec<ChainCacheEntry>, usize),
}

#[derive(Debug)]
pub struct BlockEntry {
    block: IrysBlockHeader,
    all_tx: Arc<Vec<IrysTransactionHeader>>,
    chain_state: ChainState,
    timestamp: SystemTime,
    children: HashSet<H256>,
}

/// Represents the `ChainState` of a block, is it Onchain? or a valid fork?
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ChainState {
    /// Block is confirmed and part of the main chain
    Onchain,
    /// Block is validated but may not be on the main chain
    /// For locally produced blocks, can have `ValidationScheduled` `BlockState`
    /// while maintaining `ChainState` validity
    Validated(BlockState),
    /// Block exists but is awaiting block validation
    NotOnchain(BlockState),
}

/// Represents the validation state of a block, independent of its `ChainState`
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BlockState {
    /// Initial state, validation not yet started
    Unknown,
    /// Validation has been requested but not completed
    ValidationScheduled,
    /// Block has passed all validation checks
    ValidBlock,
}

/// The result of marking a new tip for the canonical chain
#[derive(Debug, Clone)]
pub enum TipChangeResult {
    NoChange,
    Extension,
    Reorg {
        orphaned_blocks: Vec<BlockHash>,
        fork_height: u64,
    },
}

impl BlockTreeCache {
    /// Create a new cache initialized with a starting block. The block is marked as
    /// on-chain and set as the tip.
    #[must_use]
    pub fn new(block: &IrysBlockHeader) -> Self {
        let block_hash = block.block_hash;
        let solution_hash = block.solution_hash;
        let height = block.height;
        let cumulative_diff = block.cumulative_diff;

        let mut blocks = HashMap::new();
        let mut solutions = HashMap::new();
        let mut height_index = BTreeMap::new();

        // No transactions to cache for genesis block - transaction data
        // has already been confirmed and stored in the permanent chain state
        let all_tx = Arc::new(vec![]);

        // Create initial block entry for genesis block, marking it as confirmed
        // and part of the canonical chain
        let block_entry = BlockEntry {
            block: block.clone(),
            all_tx,
            chain_state: ChainState::Onchain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
        };

        // Initialize all indices
        blocks.insert(block_hash, block_entry);
        solutions.insert(solution_hash, HashSet::from([block_hash]));
        height_index.insert(height, HashSet::from([block_hash]));

        // Initialize longest chain cache with just the genesis block
        let longest_chain_cache = (
            vec![(
                block_hash,
                height,
                block.data_ledgers[DataLedger::Publish].tx_ids.0.clone(), // Publish ledger txs
                block.data_ledgers[DataLedger::Submit].tx_ids.0.clone(),  // Submit ledger txs
            )],
            0,
        );

        Self {
            blocks,
            solutions,
            tip: block_hash,
            max_cumulative_difficulty: (cumulative_diff, block_hash),
            height_index,
            longest_chain_cache,
        }
    }

    /// Initializes the cache from a list of validated blocks.
    /// The most recent block in the list is marked as the tip.
    /// The input blocks must be sorted in descending order, from newest to oldest.
    pub fn initialize_from_list(
        block_index_guard: BlockIndexReadGuard,
        reth_service_actor: Addr<RethServiceActor>,
        db: DatabaseProvider,
    ) -> Self {
        let block_index = block_index_guard.read();
        assert!(block_index.num_blocks() > 0, "Block list must not be empty");

        //block_index.print_items();

        let tx = db.tx().unwrap();

        let start = block_index
            .num_blocks()
            .saturating_sub(BLOCK_CACHE_DEPTH - 1);
        let end = block_index.num_blocks();

        // Initialize cache with the start block
        let start_block_hash = block_index.get_item(start).unwrap().block_hash;
        let start_block = block_header_by_hash(&tx, &start_block_hash, false)
            .unwrap()
            .unwrap();
        debug!(
            "block tree start block - hash: {} height: {}",
            start_block_hash.0.to_base58(),
            start_block.height
        );
        let mut cache = Self::new(&start_block);

        // Add remaining blocks
        for block_height in (start + 1)..end {
            let block_hash = block_index.get_item(block_height).unwrap().block_hash;
            let block = block_header_by_hash(&tx, &block_hash, false)
                .unwrap()
                .unwrap();
            let all_txs = Arc::new(Self::get_all_txs_for_block(&block, db.clone()).unwrap());

            cache
                .add_validated_block(block, BlockState::ValidBlock, all_txs)
                .unwrap();
        }

        let tip_hash = block_index.get_latest_item().unwrap().block_hash;
        cache.mark_tip(&tip_hash).unwrap();

        reth_service_actor
            .try_send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Irys(tip_hash),
                confirmed_hash: None,
                finalized_hash: None,
            })
            .expect("could not send message to `RethServiceActor`");
        cache
    }

    fn get_all_txs_for_block(
        block: &IrysBlockHeader,
        db: DatabaseProvider,
    ) -> Result<Vec<IrysTransactionHeader>, eyre::Report> {
        // Collect submit transactions
        let submit_txs = block.data_ledgers[DataLedger::Submit]
            .tx_ids
            .iter()
            .map(|txid| {
                db.view_eyre(|tx| tx_header_by_txid(tx, txid))
                    .and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                error!("Failed to collect submit tx headers: {}", e);
                e
            })?;

        // Collect publish transactions
        let publish_txs = block.data_ledgers[DataLedger::Publish]
            .tx_ids
            .iter()
            .map(|txid| {
                db.view_eyre(|tx| tx_header_by_txid(tx, txid))
                    .and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                error!("Failed to collect publish tx headers: {}", e);
                e
            })?;

        // Combine transactions
        let mut all_txs = submit_txs;
        all_txs.extend(publish_txs);
        Ok(all_txs)
    }

    pub fn add_common(
        &mut self,
        hash: BlockHash,
        block: &IrysBlockHeader,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
        status: ChainState,
    ) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        // Get parent
        let prev_entry = self
            .blocks
            .get_mut(&prev_hash)
            .ok_or_else(|| eyre::eyre!("Previous block not found"))?;

        // Update indices
        prev_entry.children.insert(hash);
        self.solutions
            .entry(block.solution_hash)
            .or_default()
            .insert(hash);
        self.height_index
            .entry(block.height)
            .or_default()
            .insert(hash);

        debug!(
            "adding block: max_cumulative_difficulty: {} block.cumulative_diff: {} {}",
            self.max_cumulative_difficulty.0, block.cumulative_diff, block.block_hash
        );
        if block.cumulative_diff > self.max_cumulative_difficulty.0 {
            debug!(
                "setting max_cumulative_difficulty ({}, {}) for height: {}",
                block.cumulative_diff, hash, block.height
            );
            self.max_cumulative_difficulty = (block.cumulative_diff, hash);
        }

        self.blocks.insert(
            hash,
            BlockEntry {
                block: block.clone(),
                all_tx,
                chain_state: status,
                timestamp: SystemTime::now(),
                children: HashSet::new(),
            },
        );

        self.update_longest_chain_cache();
        Ok(())
    }

    pub fn add_block(
        &mut self,
        block: &IrysBlockHeader,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
    ) -> eyre::Result<()> {
        let hash = block.block_hash;

        debug!(
            "add_block() - {} height: {}",
            block.block_hash, block.height
        );

        if matches!(
            self.blocks.get(&hash).map(|b| b.chain_state.clone()),
            Some(ChainState::Onchain)
        ) {
            debug!(?hash, "already part of the main chian state");
            return Ok(());
        }
        self.add_common(
            hash,
            block,
            all_tx,
            ChainState::NotOnchain(BlockState::Unknown),
        )
    }

    /// Adds a validated block to the cache.
    ///
    /// During development, this function allows flexibility in the validation state
    /// of locally produced blocks. While blocks received from peers must have a
    /// `BlockState::ValidBlock` before being considered part of the longest chain,
    /// blocks produced by the local node can be added as `ChainState::Validated`
    /// but have have their `BlockState` overridden.This enables the node to
    /// continue building the chain while still performing complete validation
    /// checks, helping catch any validation errors during development without
    /// halting chain progress of the local node.
    pub fn add_validated_block(
        &mut self,
        block: IrysBlockHeader,
        block_state: BlockState,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
    ) -> eyre::Result<()> {
        let hash = block.block_hash;
        let prev_hash = block.previous_block_hash;

        debug!(
            "adding validated block - hash: {} height: {}",
            block.block_hash.0.to_base58(),
            block.height,
        );

        // Verify parent is validated
        ensure!(
            !matches!(
                self.blocks.get(&prev_hash).map(|b| b.chain_state.clone()),
                Some(ChainState::NotOnchain(_))
            ),
            "Previous block not validated"
        );

        self.add_common(hash, &block, all_tx, ChainState::Validated(block_state))
    }

    /// Helper function to delete a single block without recursion
    fn delete_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        let solution_hash = block_entry.block.solution_hash;
        let height = block_entry.block.height;
        let prev_hash = block_entry.block.previous_block_hash;

        // Update parent's children set
        if let Some(prev_entry) = self.blocks.get_mut(&prev_hash) {
            prev_entry.children.remove(block_hash);
        }

        // Update height index
        if let Some(height_set) = self.height_index.get_mut(&height) {
            height_set.remove(block_hash);
            if height_set.is_empty() {
                self.height_index.remove(&height);
            }
        }

        // Update solutions map
        if let Some(solutions) = self.solutions.get_mut(&solution_hash) {
            solutions.remove(block_hash);
            if solutions.is_empty() {
                self.solutions.remove(&solution_hash);
            }
        }

        // Remove the block
        self.blocks.remove(block_hash);

        // Update max_cumulative_difficulty if necessary
        if self.max_cumulative_difficulty.1 == *block_hash {
            self.max_cumulative_difficulty = self.find_max_difficulty();
        }

        self.update_longest_chain_cache();
        Ok(())
    }

    /// Removes a block and all its descendants recursively
    pub fn remove_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        // Get children before deleting the block
        let children = self
            .blocks
            .get(block_hash)
            .map(|entry| entry.children.iter().copied().collect::<Vec<_>>())
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        // Recursively remove all children first
        for child in children {
            self.remove_block(&child)?;
        }

        // Delete this block
        self.delete_block(block_hash)
    }

    // Helper to find new max difficulty when current max is removed
    fn find_max_difficulty(&self) -> (U256, BlockHash) {
        self.blocks
            .iter()
            .map(|(hash, entry)| (entry.block.cumulative_diff, *hash))
            .max_by_key(|(diff, _)| *diff)
            .unwrap_or((U256::zero(), BlockHash::default()))
    }

    /// Returns: cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    /// 0th element -- genesis block
    /// last element -- the latest block
    #[must_use]
    pub fn get_canonical_chain(&self) -> (Vec<ChainCacheEntry>, usize) {
        self.longest_chain_cache.clone()
    }

    fn update_longest_chain_cache(&mut self) {
        let pairs = {
            self.longest_chain_cache.0.clear();
            &mut self.longest_chain_cache.0
        };
        let mut not_onchain_count = 0;

        let mut current = self.max_cumulative_difficulty.1;
        let mut blocks_to_collect = BLOCK_CACHE_DEPTH;
        debug!(
            "updating canonical chain cache latest_cache_tip: {}",
            current
        );

        while let Some(entry) = self.blocks.get(&current) {
            match &entry.chain_state {
                // For blocks awaiting initial validation, restart chain from parent
                ChainState::NotOnchain(BlockState::Unknown | BlockState::ValidationScheduled) => {
                    // Reset everything and continue from parent block
                    pairs.clear();
                    not_onchain_count = 0;
                    current = entry.block.previous_block_hash;
                    blocks_to_collect = BLOCK_CACHE_DEPTH;
                    continue;
                }

                ChainState::Onchain => {
                    // Include OnChain blocks in pairs
                    let chain_cache_entry = make_chain_cache_entry(&entry.block);
                    pairs.push(chain_cache_entry);

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }

                // For Validated or other NotOnchain states
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    let chain_cache_entry = make_chain_cache_entry(&entry.block);
                    pairs.push(chain_cache_entry);
                    not_onchain_count += 1;

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }
            }

            if entry.block.height == 0 {
                break;
            } else {
                current = entry.block.previous_block_hash;
            }
        }

        pairs.reverse();
        self.longest_chain_cache.1 = not_onchain_count;
    }

    /// Helper to mark off-chain blocks in a set
    fn mark_off_chain(&mut self, children: HashSet<H256>, current: &BlockHash) {
        for child in children {
            if child == *current {
                continue;
            }
            if let Some(entry) = self.blocks.get_mut(&child) {
                if matches!(entry.chain_state, ChainState::Onchain) {
                    entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                    // Recursively mark children of this block
                    let children = entry.children.clone();
                    self.mark_off_chain(children, current);
                }
            }
        }
    }

    /// Helper to recursively mark blocks on-chain
    fn mark_on_chain(&mut self, block: &IrysBlockHeader) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        match self.blocks.get(&prev_hash) {
            None => Ok(()), // Reached the end
            Some(prev_entry) => {
                let prev_block = prev_entry.block.clone();
                let prev_children = prev_entry.children.clone();

                match prev_entry.chain_state {
                    ChainState::Onchain => {
                        // Mark other branches as not onchain (but preserve their validation state)
                        self.mark_off_chain(prev_children, &block.block_hash);
                        Ok(())
                    }
                    ChainState::NotOnchain(BlockState::ValidBlock) | ChainState::Validated(_) => {
                        // Update previous block to on_chain
                        if let Some(entry) = self.blocks.get_mut(&prev_hash) {
                            entry.chain_state = ChainState::Onchain;
                        }
                        // Recursively mark previous blocks
                        self.mark_on_chain(&prev_block)
                    }
                    ChainState::NotOnchain(_) => Err(eyre::eyre!("invalid_tip")),
                }
            }
        }
    }

    /// Marks a block as the new tip
    pub fn mark_tip(&mut self, block_hash: &BlockHash) -> eyre::Result<bool> {
        debug!("mark_tip({})", block_hash);
        // Get the current block
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found in cache"))?;

        let block = block_entry.block.clone();
        let old_tip = self.tip;

        // Recursively mark previous blocks
        self.mark_on_chain(&block)?;

        // Mark the tip block as on_chain
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            entry.chain_state = ChainState::Onchain;
        }

        self.tip = *block_hash;
        self.update_longest_chain_cache();

        debug!(
            "\u{001b}[32mmark tip: hash:{} height: {}\u{001b}[0m",
            block_hash, block.height
        );

        Ok(old_tip != *block_hash)
    }

    pub fn mark_block_as_validation_scheduled(
        &mut self,
        block_hash: &BlockHash,
    ) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if entry.chain_state == ChainState::NotOnchain(BlockState::Unknown) {
                entry.chain_state = ChainState::NotOnchain(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            } else if entry.chain_state == ChainState::Validated(BlockState::Unknown) {
                entry.chain_state = ChainState::Validated(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    pub fn mark_block_as_valid(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            match entry.chain_state {
                ChainState::NotOnchain(BlockState::ValidationScheduled) => {
                    entry.chain_state = ChainState::NotOnchain(BlockState::ValidBlock);
                    self.update_longest_chain_cache();
                    Ok(())
                }
                // When we add blocks to the block tree that we produced locally, they are added
                // as ChainState::Validated but we can still schedule them for full validation
                // with their BlockState.
                ChainState::Validated(BlockState::ValidationScheduled) => {
                    entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                    self.update_longest_chain_cache();
                    Ok(())
                }
                _ => Err(eyre::eyre!(
                    "unable to mark block as valid: chain_state {:?} {}",
                    entry.chain_state,
                    entry.block.block_hash,
                )),
            }
        } else {
            Err(eyre::eyre!(
                "unable to mark block as valid: block not found"
            ))
        }
    }

    /// Gets block by hash
    #[must_use]
    pub fn get_block(&self, block_hash: &BlockHash) -> Option<&IrysBlockHeader> {
        self.blocks.get(block_hash).map(|entry| &entry.block)
    }

    /// Gets block and its current validation status
    #[must_use]
    pub fn get_block_and_status(
        &self,
        block_hash: &BlockHash,
    ) -> Option<(&IrysBlockHeader, &ChainState)> {
        self.blocks
            .get(block_hash)
            .map(|entry| (&entry.block, &entry.chain_state))
    }

    /// Collect previous blocks up to the last on-chain block
    fn get_fork_blocks(&self, block: &IrysBlockHeader) -> Vec<&IrysBlockHeader> {
        let mut prev_hash = block.previous_block_hash;
        let mut fork_blocks = Vec::new();

        while let Some(prev_entry) = self.blocks.get(&prev_hash) {
            match prev_entry.chain_state {
                ChainState::Onchain => {
                    fork_blocks.push(&prev_entry.block);
                    break;
                }
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    fork_blocks.push(&prev_entry.block);
                    prev_hash = prev_entry.block.previous_block_hash;
                }
            }
        }

        fork_blocks
    }

    /// Finds the earliest not validated block, walking back the chain
    /// until finding a validated block, reaching block height 0, or exceeding cache depth
    #[must_use]
    pub fn get_earliest_not_onchain<'a>(
        &'a self,
        block: &'a BlockEntry,
    ) -> Option<(&'a BlockEntry, Vec<&'a IrysBlockHeader>, SystemTime)> {
        let mut current_entry = block;
        let mut prev_block = &current_entry.block;
        let mut depth_count = 0;

        while prev_block.height > 0 && depth_count < BLOCK_CACHE_DEPTH {
            let prev_hash = prev_block.previous_block_hash;
            let prev_entry = self.blocks.get(&prev_hash)?;
            debug!(
                "\u{001b}[32mget_earliest_not_onchain: prev_entry.chain_state: {:?} {} height: {}\u{001b}[0m",
                prev_entry.chain_state, prev_hash, prev_entry.block.height
            );
            match prev_entry.chain_state {
                ChainState::Validated(BlockState::ValidBlock) | ChainState::Onchain => {
                    return Some((
                        current_entry,
                        self.get_fork_blocks(prev_block),
                        current_entry.timestamp,
                    ));
                }
                ChainState::NotOnchain(_) | ChainState::Validated(_) => {
                    current_entry = prev_entry;
                    prev_block = &current_entry.block;
                    depth_count += 1;
                }
            }
        }

        // If we've reached height 0 or exceeded cache depth, return None
        None
    }

    /// Get the earliest unvalidated block from the longest chain
    /// Relies on the `longest_chain_cache`
    #[must_use]
    pub fn get_earliest_not_onchain_in_longest_chain(
        &self,
    ) -> Option<(&BlockEntry, Vec<&IrysBlockHeader>, SystemTime)> {
        // Get the block with max cumulative difficulty
        let (_max_cdiff, max_diff_hash) = self.max_cumulative_difficulty;

        // Get the tip's cumulative difficulty
        let tip_entry = self.blocks.get(&self.tip)?;
        let tip_cdiff = tip_entry.block.cumulative_diff;

        // Check if tip's difficulty exceeds max difficulty
        if tip_cdiff >= self.max_cumulative_difficulty.0 {
            return None;
        }

        // Get the block with max difficulty
        let entry = self.blocks.get(&max_diff_hash)?;

        debug!(
            "get_earliest_not_onchain_in_longest_chain() with max_diff_hash: {} height: {} state: {:?}",
            max_diff_hash, entry.block.height, entry.chain_state
        );

        // Check if it's part of a fork and get the start of the fork
        if let ChainState::NotOnchain(_) | ChainState::Validated(_) = &entry.chain_state {
            self.get_earliest_not_onchain(entry)
        } else {
            None
        }
    }

    /// Gets block with matching solution hash, excluding specified block.
    /// Returns a block meeting these requirements:
    /// - Has matching `solution_hash`
    /// - Is not the excluded block
    /// - Either has same `cumulative_diff` as input or meets double-signing criteria
    #[must_use]
    pub fn get_by_solution_hash(
        &self,
        solution_hash: &H256,
        excluding: &BlockHash,
        cumulative_difficulty: U256,
        previous_cumulative_difficulty: U256,
    ) -> Option<&IrysBlockHeader> {
        // Get set of blocks with this solution hash
        let block_hashes = self.solutions.get(solution_hash)?;

        let mut best_block = None;

        // Examine each block hash
        for &hash in block_hashes {
            // Skip the excluded block
            if hash == *excluding {
                continue;
            }

            if let Some(entry) = self.blocks.get(&hash) {
                let block = &entry.block;

                // Case 1: Exact cumulative_diff match - return immediately
                if block.cumulative_diff == cumulative_difficulty {
                    return Some(block);
                }

                // Case 2: Double signing case - return immediately
                if block.cumulative_diff > previous_cumulative_difficulty
                    && cumulative_difficulty > block.previous_cumulative_diff
                {
                    return Some(block);
                }

                // Store as best block seen so far if we haven't found one yet
                if best_block.is_none() {
                    best_block = Some(block);
                }
            }
        }

        // Return best block found (if any)
        best_block
    }

    /// Prunes blocks below specified depth from tip. When pruning an on-chain block,
    /// removes all its non-on-chain children regardless of their height.
    pub fn prune(&mut self, depth: u64) -> eyre::Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }

        let tip_height = self
            .blocks
            .get(&self.tip)
            .ok_or_else(|| eyre::eyre!("Tip block not found"))?
            .block
            .height;
        let min_keep_height = tip_height.saturating_sub(depth);

        let min_height = match self.height_index.keys().min() {
            Some(&h) => h,
            None => return Ok(()),
        };

        let mut current_height = min_height;
        while current_height < min_keep_height {
            let Some(hashes) = self.height_index.get(&current_height) else {
                current_height += 1;
                continue;
            };

            // Clone hashes to avoid borrow issues during removal
            let hashes: Vec<_> = hashes.iter().copied().collect();

            for hash in hashes {
                if let Some(entry) = self.blocks.get(&hash) {
                    if matches!(entry.chain_state, ChainState::Onchain) {
                        // First remove all non-on-chain children
                        let children = entry.children.clone();
                        for child in children {
                            if let Some(child_entry) = self.blocks.get(&child) {
                                if !matches!(child_entry.chain_state, ChainState::Onchain) {
                                    self.remove_block(&child)?;
                                }
                            }
                        }

                        // Now remove just this block
                        self.delete_block(&hash)?;
                    }
                }
            }

            current_height += 1;
        }

        Ok(())
    }

    /// Returns true if solution hash exists in cache
    #[must_use]
    pub fn is_known_solution_hash(&self, solution_hash: &H256) -> bool {
        self.solutions.contains_key(solution_hash)
    }
}

pub async fn get_optimistic_chain(tree: BlockTreeReadGuard) -> eyre::Result<Vec<(H256, u64)>> {
    let canonical_chain = tokio::task::spawn_blocking(move || {
        let cache = tree.read();

        let mut blocks_to_collect = BLOCK_CACHE_DEPTH;
        let mut chain_cache = Vec::with_capacity(
            blocks_to_collect
                .try_into()
                .expect("u64 must fit into usize"),
        );
        let mut current = cache.max_cumulative_difficulty.1;
        debug!("get_optimistic_chain with latest_cache_tip: {}", current);

        while let Some(entry) = cache.blocks.get(&current) {
            chain_cache.push((current, entry.block.height));

            if blocks_to_collect == 0 {
                break;
            }
            blocks_to_collect -= 1;

            if entry.block.height == 0 {
                break;
            } else {
                current = entry.block.previous_block_hash;
            }
        }

        chain_cache.reverse();
        chain_cache
    })
    .await?;
    Ok(canonical_chain)
}

/// Returns the canonical chain where the first item in the Vec is the oldest block
/// Implementation detail: utilises `tokio::task::spawn_blocking`
pub async fn get_canonical_chain(
    tree: BlockTreeReadGuard,
) -> eyre::Result<(Vec<(H256, u64, Vec<H256>, Vec<H256>)>, usize)> {
    let canonical_chain =
        tokio::task::spawn_blocking(move || tree.read().get_canonical_chain()).await?;
    Ok(canonical_chain)
}

/// Returns the block from the block tree at a given block hash
/// Implementation detail: utilises `tokio::task::spawn_blocking`
pub async fn get_block(
    block_tree_read_guard: BlockTreeReadGuard,
    block_hash: H256,
) -> eyre::Result<Option<Arc<IrysBlockHeader>>> {
    let res = tokio::task::spawn_blocking(move || {
        block_tree_read_guard
            .read()
            .get_block(&block_hash)
            .cloned()
            .map(Arc::new)
    })
    .await?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use eyre::ensure;

    #[actix::test]
    async fn test_block_cache() {
        let b1 = random_block(U256::from(0));

        // For the purposes of these tests, the block cache will not track transaction headers
        let all_tx = Arc::new(vec![]);

        // Initialize block tree cache from `b1`
        let mut cache = BlockTreeCache::new(&b1);

        // Verify cache returns `None` for unknown hashes
        assert_eq!(cache.get_block(&H256::random()), None);
        assert_eq!(
            cache.get_by_solution_hash(&H256::random(), &H256::random(), U256::one(), U256::one()),
            None
        );

        // Verify cache returns the expected block
        assert_eq!(cache.get_block(&b1.block_hash), Some(&b1));
        assert_eq!(
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &H256::random(),
                U256::one(),
                U256::one()
            ),
            Some(&b1)
        );

        // Verify getting by `solution_hash` excludes the expected block
        assert_matches!(
            cache.get_by_solution_hash(&b1.solution_hash, &b1.block_hash, U256::one(), U256::one()),
            None
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Adding `b1` again shouldn't change the state because it is confirmed
        // onchain
        let mut b1_test = b1.clone();
        b1_test.data_ledgers[DataLedger::Submit]
            .tx_ids
            .push(H256::random());
        assert_matches!(cache.add_block(&b1_test, all_tx.clone()), Ok(_));
        assert_eq!(
            cache.get_block(&b1.block_hash).unwrap().data_ledgers[DataLedger::Submit]
                .tx_ids
                .len(),
            0
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b1.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids
                .len(),
            0
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Same as above, `get_deepest_unvalidated_in_longest_chain` should not
        // modify state
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add b2 block as not_validated
        let mut b2 = extend_chain(random_block(U256::from(1)), &b1);
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2.block_hash
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add a TXID to b2, and re-add it to the cache, but still don't mark as validated
        let txid = H256::random();
        b2.data_ledgers[DataLedger::Submit].tx_ids.push(txid);
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        assert_eq!(
            cache.get_block(&b2.block_hash).unwrap().data_ledgers[DataLedger::Submit].tx_ids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &b1.block_hash, U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids[0],
            txid
        );

        // Remove b2_1
        assert_matches!(cache.remove_block(&b2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b2.block_hash), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b2_1 again
        assert_matches!(cache.remove_block(&b2.block_hash), Err(_));

        // Re-add b2_1 and add a competing b2 block called b1_2, it will be built
        // on b1 but share the same solution_hash
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        let mut b1_2 = extend_chain(random_block(U256::from(2)), &b1);
        b1_2.solution_hash = b1.solution_hash;
        assert_matches!(cache.add_block(&b1_2, all_tx.clone()), Ok(_));

        println!(
            "b1:   {} cdiff: {} solution_hash: {}",
            b1.block_hash, b1.cumulative_diff, b1.solution_hash
        );
        println!(
            "b2:   {} cdiff: {} solution_hash: {}",
            b2.block_hash, b2.cumulative_diff, b2.solution_hash
        );
        println!(
            "b1_2: {} cdiff: {} solution_hash: {}",
            b1_2.block_hash, b1_2.cumulative_diff, b1_2.solution_hash
        );

        // Verify if we exclude b1_2 we wont get it back
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &b1_2.block_hash,
                    U256::one(),
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );

        // Verify that we do get b1_2 back when not excluding it
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    b1_2.cumulative_diff,
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1_2.block_hash
        );

        // Get result with empty excluded hash
        let result = cache
            .get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::default(), // Empty/zeroed hash
                U256::one(),
                U256::one(),
            )
            .unwrap();

        // Assert result is either b1 or b1_2
        assert!(
            result.block_hash == b1.block_hash || result.block_hash == b1_2.block_hash,
            "Expected either b1 or b1_2 to be returned"
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Even though b2 is marked as a tip, it is still lower difficulty than b1_2 so will
        // not be included in the longest chain
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        assert_eq!(Some(&b1_2), cache.get_block(&b1_2.block_hash));
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b1_2, causing b2 to now be the tip of the heaviest chain
        assert_matches!(cache.remove_block(&b1_2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b1_2.block_hash), None);
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune to a depth of 1 behind the tip
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b1), cache.get_block(&b1.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune at the tip, removing all ancestors (verify b1 is pruned)
        assert_matches!(cache.prune(0), Ok(_));
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // Again, this time to make sure b1_2 is really gone
        assert_matches!(cache.prune(0), Ok(_));
        assert_eq!(None, cache.get_block(&b1_2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1_2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // <Reset the cache>
        // b1_2->b1 fork is the heaviest, but only b1 is validated. b2_2->b2->b1 is longer but
        // has a lower cdiff.
        let mut cache = BlockTreeCache::new(&b1);
        assert_matches!(cache.add_block(&b1_2, all_tx.clone()), Ok(_));
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        let b2_2 = extend_chain(random_block(U256::one()), &b2);
        println!(
            "b2_2: {} cdiff: {} solution_hash: {}",
            b2_2.block_hash, b2_2.cumulative_diff, b2_2.solution_hash
        );
        assert_matches!(cache.add_block(&b2_2, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b1_2.block_hash
        );

        // b2_3->b2_2->b2->b1 is longer and heavier but only b2->b1 are validated.
        let b2_3 = extend_chain(random_block(U256::from(3)), &b2_2);
        println!(
            "b2_3: {} cdiff: {} solution_hash: {}",
            b2_3.block_hash, b2_3.cumulative_diff, b2_3.solution_hash
        );
        assert_matches!(cache.add_block(&b2_3, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Err(_));
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Now b2_2->b2->b1 are validated.
        assert_matches!(
            cache.add_validated_block(b2_2.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b2_2.block_hash).unwrap(),
            (&b2_2, &ChainState::Validated(BlockState::ValidBlock))
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b2_2], 1, &cache), Ok(_));

        // Now the b3->b2->b1 fork is heaviest
        let b3 = extend_chain(random_block(U256::from(4)), &b2);
        println!(
            "b3:   {} cdiff: {} solution_hash: {}",
            b3.block_hash, b3.cumulative_diff, b3.solution_hash
        );
        assert_matches!(cache.add_block(&b3, all_tx.clone()), Ok(_));
        assert_matches!(
            cache.add_validated_block(b3.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_matches!(cache.mark_tip(&b3.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 0, &cache), Ok(_));

        // b3->b2->b1 fork is still heaviest
        assert_matches!(cache.mark_tip(&b2_2.block_hash), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // add not validated b4, b3->b2->b1 fork is still heaviest
        let b4 = extend_chain(random_block(U256::from(5)), &b3);
        println!(
            "b4:   {} cdiff: {} solution_hash: {}",
            b4.block_hash, b4.cumulative_diff, b4.solution_hash
        );
        assert_matches!(cache.add_block(&b4, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b4.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // Prune to a depth of 1 past the tip and verify b1 and the b1_2 branch are pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2, &b3], 1, &cache), Ok(_));

        // Mark a new tip for the longest chain, validating b2_3, and pruning again
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Ok(_));
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Also make sure b3 (the old tip) got pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // and that the not yet validated b4 was also pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Now make sure b2_2 and b2_3 are still in the cache/state
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b2_2), cache.get_block(&b2_2.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_2.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b2_3), cache.get_block(&b2_3.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_3.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Verify previously pruned b3 can be safely removed again, with longest chain cache staying stable
        assert_matches!(cache.remove_block(&b3.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Same safety check for b4 - removing already pruned block shouldn't affect chain state
        assert_matches!(cache.remove_block(&b4.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11);
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(cache.add_block(&b12, all_tx.clone()), Ok(_));
        let b13 = extend_chain(random_block(U256::one()), &b11);

        println!("---");
        println!(
            "b11: {} cdiff: {} solution_hash: {}",
            b11.block_hash, b11.cumulative_diff, b11.solution_hash
        );
        println!(
            "b12: {} cdiff: {} solution_hash: {}",
            b12.block_hash, b12.cumulative_diff, b12.solution_hash
        );
        println!(
            "b13: {} cdiff: {} solution_hash: {}",
            b13.block_hash, b13.cumulative_diff, b13.solution_hash
        );
        println!("tip: {} before mark_tip()", cache.tip);

        assert_matches!(
            cache.add_validated_block(b13.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        let reorg = cache.mark_tip(&b13.block_hash).unwrap();

        // The tip does change here, even though it's not part of the longest
        // chain, this seems like a bug
        println!("tip: {} after mark_tip()", cache.tip);
        assert!(reorg);

        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        // Although b13 becomes the tip, it's not included in the longest_chain_cache.
        // This is because the cache follows blocks from max_cumulative_difficulty, which
        // was set to b12 when it was first added. When multiple blocks have the same
        // cumulative difficulty, max_cumulative_difficulty preserves the first one seen.
        // Since b13's difficulty equals b12's (rather than exceeds it), b12 remains the
        // reference point for longest chain calculations.

        // Block tree state:
        //
        //                     [B13] cdiff=1, Validated
        //                    /  ⚡ marked as tip but not in longest chain
        //                   /     because B12 has same cdiff & was first
        //                  /
        // [B11] cdiff=0 --+-- [B12] cdiff=1, NotValidated (first added)
        // (genesis - tip)       ⚠ not counted as onchain due to not being validated
        //
        // ▶ Longest chain contains: [B11]
        // ▶ Not on chain count: 0
        // ▶ First added wins longest_chain with equal cdiff

        // DMac's Note:
        // Issue: tip and longest chain can become misaligned when marking a tip that has
        //   equal difficulty to an earlier block. The tip could point to b13 while the
        //   longest chain contains b12, as b12 was seen first with same difficulty.
        // Fix: mark_tip() should reject attempts to change tip to a block that has equal
        //   (rather than greater) difficulty compared to the current max_difficulty block.
        //   This would ensure tip always follows the longest chain. TBH the current behavior
        //   is likely aligned with the local miners economic interest and why things
        //   like "uncle" blocks exist on other chains.
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Extend the b13->b11 chain
        let b14 = extend_chain(random_block(U256::from(2)), &b13);
        assert_matches!(cache.add_block(&b14, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b14.block_hash
        );
        // by adding b14 we've now made the b13->b11 chain heavier and because
        // b13 is already validated it is included in the longest chain
        // b14 isn't validated so it doesn't count towards the not_onchain_count
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Try to mutate the state of the cache with some random validations
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&BlockHash::random()),
            Ok(_)
        );
        assert_matches!(cache.mark_block_as_valid(&BlockHash::random()), Err(_));
        // Attempt to mark the already onchain b13 to prior vdf states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b13.block_hash),
            Ok(_)
        );
        assert_matches!(cache.mark_block_as_valid(&b13.block_hash), Err(_));
        // Verify its state wasn't changed
        assert_eq!(
            cache.get_block_and_status(&b13.block_hash).unwrap(),
            (&b13, &ChainState::Onchain)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::Unknown))
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Move b14 though the vdf validation states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b14.block_hash),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &ChainState::NotOnchain(BlockState::ValidationScheduled)
            )
        );
        assert_matches!(
            check_earliest_not_onchian(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(_)
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // now mark b14 as vdf validated
        assert_matches!(cache.mark_block_as_valid(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(
            check_earliest_not_onchian(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(_)
        );
        // Now that b14 is vdf validated it can be considered a NotOnchain
        // part of the longest chain
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b15 block
        let b15 = extend_chain(random_block(U256::from(3)), &b14);
        assert_matches!(cache.add_block(&b15, all_tx.clone()), Ok(_));
        assert_matches!(
            check_earliest_not_onchian(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(_)
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Validate b14
        assert_matches!(
            cache.add_validated_block(b14.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_matches!(
            check_earliest_not_onchian(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Validated(BlockState::ValidBlock))
        );
        // b14 is validated, but wont be onchain until is tip_height or lower
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b16 block
        let b16 = extend_chain(random_block(U256::from(4)), &b15);
        assert_matches!(cache.add_block(&b16, all_tx.clone()), Ok(_));
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b16.block_hash),
            Ok(_)
        );
        assert_matches!(
            check_earliest_not_onchian(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Mark b16 as vdf validated eve though b15 is not
        assert_matches!(cache.mark_block_as_valid(&b16.block_hash), Ok(_));
        assert_matches!(
            check_earliest_not_onchian(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b16.block_hash).unwrap(),
            (&b16, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Make b14 the tip (making it OnChain)
        assert_matches!(cache.mark_tip(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Onchain)
        );
        assert_matches!(
            check_earliest_not_onchian(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11);
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(cache.add_block(&b12, all_tx.clone()), Ok(_));
        let _b13 = extend_chain(random_block(U256::one()), &b11);
        println!("---");
        assert_matches!(cache.mark_tip(&b11.block_hash), Ok(_));

        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Now add the subsequent block, but as awaitingValidation
        assert_matches!(
            cache.add_validated_block(b12.clone(), BlockState::ValidationScheduled, all_tx.clone()),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b12], 1, &cache), Ok(_));

        // When a locally produced block is added as validated "onchain" but it
        // hasn't yet been validated by the validation_service
        assert_matches!(
            check_earliest_not_onchian(
                &b12.block_hash,
                &ChainState::Validated(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(_)
        );

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11);
        assert_matches!(cache.mark_tip(&b11.block_hash), Ok(_));

        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_validated_block(b12.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_matches!(cache.mark_tip(&b12.block_hash), Ok(_));

        assert_matches!(check_longest_chain(&[&b11, &b12], 0, &cache), Ok(_));

        // Create a fork at b12
        let b13a = extend_chain(random_block(U256::from(2)), &b12);
        let b13b = extend_chain(random_block(U256::from(2)), &b12);

        assert_matches!(
            cache.add_validated_block(b13a.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_matches!(
            cache.add_validated_block(b13b.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );

        assert_matches!(check_longest_chain(&[&b11, &b12, &b13a], 1, &cache), Ok(_));

        assert_matches!(cache.mark_tip(&b13a.block_hash), Ok(_));
        assert_matches!(check_longest_chain(&[&b11, &b12, &b13a], 0, &cache), Ok(_));

        // extend the fork to make it canonical
        let b14b = extend_chain(random_block(U256::from(3)), &b13b);
        assert_matches!(
            cache.add_validated_block(b14b.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );

        assert_matches!(
            check_longest_chain(&[&b11, &b12, &b13b, &b14b], 2, &cache),
            Ok(_)
        );

        // Mark the new tip
        assert_matches!(cache.mark_tip(&b14b.block_hash), Ok(_));

        assert_matches!(
            check_longest_chain(&[&b11, &b12, &b13b, &b14b], 0, &cache),
            Ok(_)
        );
    }

    fn random_block(cumulative_diff: U256) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new_mock_header();
        block.block_hash = BlockHash::random();
        block.solution_hash = H256::random(); // Ensure unique solution hash
        block.height = 0; // Default to genesis
        block.cumulative_diff = cumulative_diff;
        block
    }

    const fn extend_chain(
        mut new_block: IrysBlockHeader,
        previous_block: &IrysBlockHeader,
    ) -> IrysBlockHeader {
        new_block.previous_block_hash = previous_block.block_hash;
        new_block.height = previous_block.height + 1;
        new_block.previous_cumulative_diff = previous_block.cumulative_diff;
        // Don't modify solution_hash - keep the random one from block creation
        new_block
    }

    fn check_earliest_not_onchian(
        block_hash: &BlockHash,
        chain_state: &ChainState,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let _x = 1;
        if let Some((block_entry, _, _)) = cache.get_earliest_not_onchain_in_longest_chain() {
            let c_s = &block_entry.chain_state;

            ensure!(
                block_entry.block.block_hash == *block_hash,
                "Wrong unvalidated block found: {} expected:{}",
                block_entry.block.block_hash,
                block_hash
            );

            ensure!(
                chain_state == c_s,
                "Wrong validation_state found: {:?}",
                c_s
            );
        } else if let ChainState::NotOnchain(_) = chain_state {
            return Err(eyre::eyre!("No unvalidated blocks found in longest chain"));
        }

        Ok(())
    }

    fn check_longest_chain(
        expected_blocks: &[&IrysBlockHeader],
        expected_not_onchain: usize,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let (canonical_blocks, not_onchain_count) = cache.get_canonical_chain();
        let actual_blocks: Vec<_> = canonical_blocks
            .iter()
            .map(|(hash, _, _, _)| *hash)
            .collect();

        ensure!(
            actual_blocks
                == expected_blocks
                    .iter()
                    .map(|b| b.block_hash)
                    .collect::<Vec<_>>(),
            "Canonical chain does not match expected blocks"
        );
        ensure!(
            not_onchain_count == expected_not_onchain,
            format!(
                "Number of not-onchain blocks ({}) does not match expected ({})",
                not_onchain_count, expected_not_onchain
            )
        );
        Ok(())
    }
}
