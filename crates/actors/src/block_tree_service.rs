use crate::{
    block_index_service::BlockIndexServiceMessage,
    block_validation::PreValidationError,
    chunk_migration_service::ChunkMigrationServiceMessage,
    mempool_service::MempoolServiceMessage,
    mining_bus::{BroadcastDifficultyUpdate, BroadcastPartitionsExpiration},
    reth_service::{ForkChoiceUpdateMessage, RethServiceMessage},
    services::ServiceSenders,
    validation_service::ValidationServiceMessage,
    StorageModuleServiceMessage,
};
use eyre::ensure;
use eyre::OptionExt as _;
use irys_config::StorageSubmodulesConfig;
use irys_domain::{
    block_index_guard::BlockIndexReadGuard, create_commitment_snapshot_for_block,
    create_epoch_snapshot_for_block, forkchoice_markers::ForkChoiceMarkers, make_block_tree_entry,
    BlockState, BlockTree, BlockTreeEntry, BlockTreeReadGuard, ChainState, EpochReplayData,
};
use irys_types::{
    BlockHash, BlockTransactions, Config, DataLedger, DataTransactionHeader, DatabaseProvider,
    H256List, IrysAddress, IrysBlockHeader, TokioServiceHandle, H256,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::SystemTime,
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{debug, error, info, warn, Instrument as _};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

// Messages that the CommitmentCache service supports
#[derive(Debug)]
pub enum BlockTreeServiceMessage {
    GetBlockTreeReadGuard {
        response: oneshot::Sender<BlockTreeReadGuard>,
    },
    BlockPreValidated {
        block: Arc<IrysBlockHeader>,
        transactions: BlockTransactions,
        skip_vdf_validation: bool,
        response: oneshot::Sender<Result<(), PreValidationError>>,
    },
    BlockValidationFinished {
        block_hash: H256,
        validation_result: ValidationResult,
    },
}

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockTreeService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<BlockTreeServiceMessage>,
    inner: BlockTreeServiceInner,
}

#[derive(Debug)]
pub struct BlockTreeServiceInner {
    db: DatabaseProvider,
    /// Block tree internal state
    pub cache: Arc<RwLock<BlockTree>>,
    /// The wallet address of the local miner
    pub miner_address: IrysAddress,
    /// Read view of the `block_index`
    pub block_index_guard: BlockIndexReadGuard,
    /// Global storage config
    pub config: Config,
    /// Storage submodules configuration
    pub storage_submodules_config: StorageSubmodulesConfig,
    /// Channels for communicating with the services
    pub service_senders: ServiceSenders,
}

#[derive(Debug, Clone)]
pub struct ReorgEvent {
    pub old_fork: Arc<Vec<Arc<IrysBlockHeader>>>,
    pub new_fork: Arc<Vec<Arc<IrysBlockHeader>>>,
    pub fork_parent: Arc<IrysBlockHeader>,
    pub new_tip: BlockHash,
    pub timestamp: SystemTime,
    pub db: Option<DatabaseProvider>,
}

#[derive(Debug, Clone)]
pub struct BlockMigratedEvent {
    pub block: Arc<IrysBlockHeader>,
}

/// Event broadcast when a block's state changes in the block tree.
#[derive(Debug, Clone)]
pub struct BlockStateUpdated {
    pub block_hash: BlockHash,
    pub height: u64,
    pub state: ChainState,
    pub discarded: bool,
    pub validation_result: ValidationResult,
}

impl BlockTreeService {
    /// Spawn a new BlockTree service
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_block_tree")]
    pub fn spawn_service(
        rx: UnboundedReceiver<BlockTreeServiceMessage>,
        db: DatabaseProvider,
        block_index_guard: BlockIndexReadGuard,
        epoch_replay_data: &EpochReplayData,
        storage_submodules_config: &StorageSubmodulesConfig,
        config: &Config,
        service_senders: &ServiceSenders,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning block tree service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        // Dereference miner_address here, before the closure
        let miner_address = config.node_config.miner_address();
        let service_senders = service_senders.clone();
        let bi_guard = block_index_guard;
        let epoch_replay_data = (*epoch_replay_data).clone();
        let config = config.clone();
        let storage_submodules_config = storage_submodules_config.clone();

        let handle = runtime_handle.spawn(
            async move {
                let cache = match BlockTree::restore_from_db(
                    bi_guard.clone(),
                    epoch_replay_data,
                    db.clone(),
                    &storage_submodules_config,
                    config.clone(),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        // Choosing to panic and stop the node, if we cannot restore BlockTree, we cannot continue
                        panic!("Failed to restore BlockTree from DB: {}", e);
                    }
                };

                let block_tree_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: BlockTreeServiceInner {
                        db,
                        cache: Arc::new(RwLock::new(cache)),
                        miner_address,
                        block_index_guard: bi_guard,
                        config,
                        service_senders,
                        storage_submodules_config: storage_submodules_config.clone(),
                    },
                };
                block_tree_service
                    .start()
                    .await
                    .expect("BlockTree encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "block_tree_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting BlockTree service");

        loop {
            tokio::select! {
                biased;

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for block tree service");
                    break;
                }
                // Handle messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg).await?;
                        }
                        None => {
                            warn!("Message channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        tracing::info!("shutting down BlockTree service gracefully");
        Ok(())
    }
}

impl BlockTreeServiceInner {
    /// Dispatches received messages to appropriate handler methods and sends responses
    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn handle_message(&mut self, msg: BlockTreeServiceMessage) -> eyre::Result<()> {
        match msg {
            BlockTreeServiceMessage::GetBlockTreeReadGuard { response } => {
                let guard = BlockTreeReadGuard::new(self.cache.clone());
                if let Err(_guard) = response.send(guard) {
                    tracing::warn!("Block tree guard response channel was closed by receiver");
                }
            }
            BlockTreeServiceMessage::BlockPreValidated {
                block,
                transactions,
                skip_vdf_validation: skip_vdf,
                response,
            } => {
                let block_hash = block.block_hash;
                let block_height = block.height;
                let result = self.on_block_prevalidated(block, transactions, skip_vdf);
                if let Err(send_err) = response.send(result) {
                    tracing::warn!(
                        block.hash = ?block_hash,
                        block.height = block_height,
                        custom.send_error = ?send_err,
                        "Failed to send pre-validation result to caller - receiver dropped"
                    );
                }
            }
            BlockTreeServiceMessage::BlockValidationFinished {
                block_hash,
                validation_result,
            } => {
                self.on_block_validation_finished(block_hash, validation_result)
                    .await?;
            }
        }
        Ok(())
    }

    /// Sends block-migration notifications to services after a block reaches migration depth.
    ///
    /// This method:
    /// - Resolves the full `IrysBlockHeader` for the provided `block_hash` from the mempool or the database
    /// - Fetches the Submit and Publish data-transaction headers from the mempool
    /// - Emits a block migration message to the `BlockIndexService` and `ChunkMigrationService`
    ///
    /// Errors
    /// Returns an error if the block header cannot be fetched or if any mempool/database access fails.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block_header.block_hash, block.height = block_header.height))]
    async fn send_block_migration_message(
        &self,
        block_header: Arc<IrysBlockHeader>,
    ) -> eyre::Result<()> {
        let submit_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Submit)
            .await?;
        let publish_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Publish)
            .await?;

        // TODO: Migrate block_index to use the HashMap so we don't have to close these headers
        let mut all_txs = vec![];
        all_txs.extend(publish_txs.clone());
        all_txs.extend(submit_txs.clone());

        let mut all_txs_map: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        all_txs_map.insert(DataLedger::Submit, submit_txs);
        all_txs_map.insert(DataLedger::Publish, publish_txs);

        info!(
            "Migrating to block_index - hash: {} height: {}",
            &block_header.block_hash, &block_header.height
        );

        let arc_block = block_header;
        let arc_all_txs = Arc::new(all_txs);

        // Let block_index know about the migrated block
        let (tx, rx) = oneshot::channel();
        self.service_senders
            .block_index
            .send(BlockIndexServiceMessage::MigrateBlock {
                block_header: arc_block.clone(),
                all_txs: arc_all_txs.clone(),
                response: tx,
            })?;
        rx.await
            .map_err(|e| eyre::eyre!("Failed to receive BlockIndexService response: {e}"))?
            .map_err(|e| eyre::eyre!("BlockIndexService error during migration: {e}"))?;

        // Let the chunk_migration_service know about the block migration
        self.service_senders
            .chunk_migration
            .send(ChunkMigrationServiceMessage::BlockMigrated(
                arc_block,
                Arc::new(all_txs_map),
            ))
            .map_err(|e| eyre::eyre!("Failed to send BlockMigrated message: {}", e))?;

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(fcu.head = %markers.head.block_hash, fcu.migration = %markers.migration_block.block_hash))]
    async fn emit_fcu(&self, markers: &ForkChoiceMarkers) -> eyre::Result<()> {
        let tip_block = &markers.head;
        debug!(
            fcu.head = %tip_block.block_hash,
            fcu.migration = %markers.migration_block.block_hash,
            fcu.prune = %markers.prune_block.block_hash,
            "broadcasting canonical chain update",
        );

        let (tx, rx) = oneshot::channel();

        self.service_senders
            .reth_service
            .send(RethServiceMessage::ForkChoice {
                update: ForkChoiceUpdateMessage {
                    head_hash: markers.head.block_hash,
                    confirmed_hash: markers.migration_block.block_hash,
                    finalized_hash: markers.prune_block.block_hash,
                },
                response: tx,
            })
            .expect("Unable to send confirmation FCU message to reth");

        rx.await
            .map_err(|e| eyre::eyre!("Failed waiting for Reth FCU ack: {e}"))
    }

    fn emit_block_confirmed(&self, markers: &ForkChoiceMarkers) {
        let tip_block = Arc::clone(&markers.head);
        self.service_senders
            .mempool
            .send(MempoolServiceMessage::BlockConfirmed(tip_block))
            .expect("mempool service has unexpectedly become unreachable");
    }

    /// Ensures blocks can be migrated by verifying height continuity and chain linkage.
    fn validate_migration_continuity(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<()> {
        let blocks_to_migrate = self.get_blocks_to_migrate(migration_block)?;

        // Nothing to validate if no blocks need migration
        let Some(first_block) = blocks_to_migrate.first() else {
            return Ok(());
        };

        let block_index = self.block_index_guard.read();
        let latest_indexed = block_index
            .get_latest_item()
            .ok_or_eyre("Block index is empty")?;

        // Ensure continuous height progression
        ensure!(
            block_index.latest_height() + 1 == first_block.height,
            "Height gap detected: block index at height {} ({}), trying to migrate height {} ({})",
            &block_index.latest_height(),
            &latest_indexed.block_hash,
            &first_block.height,
            &first_block.block_hash
        );

        // Ensure proper chain linkage
        ensure!(
            latest_indexed.block_hash == first_block.previous_block_hash,
            "Chain break detected: migration block ({}) doesn't link to indexed chain ({})",
            &latest_indexed.block_hash,
            &first_block.previous_block_hash
        );

        Ok(())
    }

    /// Collect blocks to migrate by walking backwards from the current migration block
    /// to the head of the `block_index`
    fn get_blocks_to_migrate(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<Vec<Arc<IrysBlockHeader>>> {
        let mut blocks_to_migrate = vec![];

        // Get the last migrated block hash
        let bi = self.block_index_guard.read();
        let last_migrated = bi
            .get_latest_item()
            .ok_or_eyre("must have at least a single item in block index")?;
        let last_migrated_hash = last_migrated.block_hash;
        drop(bi);

        // Get the block tree
        let block_tree = self.cache.read().expect("poisoned lock");

        // Walk backwards from the given block to the last migrated block
        let mut current_hash = migration_block.block_hash;

        while current_hash != last_migrated_hash {
            // Get the block
            let current_block = block_tree.get_block(&current_hash).ok_or_else(|| {
                eyre::eyre!(
                    "block {} not found while collecting blocks for migration",
                    current_hash
                )
            })?;

            // Add to list
            let arc_block = Arc::new(current_block.clone());
            blocks_to_migrate.push(arc_block.clone());

            // Move to previous block
            current_hash = arc_block.previous_block_hash;
        }

        // Reverse to get oldest-first order
        blocks_to_migrate.reverse();
        Ok(blocks_to_migrate)
    }

    /// Checks if a block that is `block_migration_depth` blocks behind `arc_block`
    /// should be migrated. If eligible, sends migration message unless block
    /// is already in `block_index`. Panics if the `block_tree` and `block_index` are
    /// inconsistent.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = block.height))]
    async fn migrate_block(&self, block: &Arc<IrysBlockHeader>) -> eyre::Result<()> {
        // Check if the block is already in the block index
        // let binding = self.block_index_guard.clone();

        debug!(block.hash = %block.block_hash, block.height = block.height, "migrating irys block");

        // Collect blocks to migrate by walking backwards from the current migration block
        let blocks_to_migrate = self.get_blocks_to_migrate(block)?;

        // Send all blocks in order (oldest to newest)
        for block_to_migrate in blocks_to_migrate {
            // NOTE: order of events is very important! block migration event
            // writes chunks to db, which is expected by `send_block_migration_message`.
            let block_migrated_event = BlockMigratedEvent {
                block: Arc::clone(&block_to_migrate),
            };
            if let Err(e) = self
                .service_senders
                .block_migrated_events
                .send(block_migrated_event)
            {
                debug!("No reorg subscribers: {:?}", e);
            }
            let block_hash = block_to_migrate.block_hash;
            let block_height = block_to_migrate.height;
            self.send_block_migration_message(block_to_migrate)
                .await
                .inspect_err(|e| {
                    error!(
                        "Unable to send block migration message for block {} (height {}): {:?}",
                        block_hash, block_height, e
                    )
                })?
        }
        Ok(())
    }

    /// Handles pre-validated blocks received from the validation service.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = block.height))]
    fn on_block_prevalidated(
        &mut self,
        block: Arc<IrysBlockHeader>,
        transactions: BlockTransactions,
        skip_vdf: bool,
    ) -> eyre::Result<(), PreValidationError> {
        let block_hash = &block.block_hash;
        let mut cache = self.cache.write().expect("cache lock poisoned");

        // Early return if block already exists
        if let Some(existing) = cache.get_block(block_hash) {
            debug!(
                "on_block_prevalidated: {} at height: {} already in block_tree",
                existing.block_hash, existing.height
            );
            return Ok(());
        }

        let parent_block_entry =
            cache
                .blocks
                .get(&block.previous_block_hash)
                .unwrap_or_else(|| {
                    panic!(
                        "block {} needs to be in cache at height: {}",
                        block.previous_block_hash,
                        block.height - 1
                    )
                });

        // Get the parent block's commitment snapshot
        let prev_commitment_snapshot = parent_block_entry.commitment_snapshot.clone();

        // Create epoch snapshot for this block
        let arc_epoch_snapshot =
            create_epoch_snapshot_for_block(&block, parent_block_entry, &self.config.consensus)
                .map_err(|x| PreValidationError::InvalidEpochSnapshot {
                    error: x.to_string(),
                })?;

        // Create commitment snapshot for this block
        let commitment_snapshot = create_commitment_snapshot_for_block(
            &block,
            &transactions.commitment_txs,
            &prev_commitment_snapshot,
            arc_epoch_snapshot.clone(),
            &self.config.consensus,
        );

        // Create ema snapshot for this block
        let ema_snapshot = parent_block_entry
            .ema_snapshot
            .next_snapshot(&block, &parent_block_entry.block, &self.config.consensus)
            .map_err(|e| PreValidationError::EmaSnapshotError(e.to_string()))?;

        let add_result = cache.add_block(
            &block,
            commitment_snapshot,
            arc_epoch_snapshot,
            ema_snapshot,
        );

        if add_result.is_ok() {
            // Mark as scheduled and schedule validation
            if let Err(err) = cache.mark_block_as_validation_scheduled(block_hash) {
                error!(
                    "Unable to mark block {} as ValidationScheduled: {:?}",
                    block_hash, err
                );
                return Err(PreValidationError::UpdateCacheForScheduledValidationError(
                    *block_hash,
                ));
            }
            self.service_senders
                .validation_service
                .send(ValidationServiceMessage::ValidateBlock {
                    block: block.clone(),
                    transactions,
                    skip_vdf_validation: skip_vdf,
                })
                .map_err(|_| PreValidationError::ValidationServiceUnreachable)?;

            debug!(
                "scheduling block for validation: {} height: {}",
                block_hash, block.height
            );
        }

        Ok(())
    }

    // Handles the completion of full block validation.
    ///
    /// When a block passes validation:
    /// 1. Updates the block's state in the cache to `ValidBlock`
    /// 2. Moves the tip of the chain to this block if it is now the head of the longest chain
    /// 3. If the tip moves, checks whether it's a simple extension or a reorganization:
    /// 4. For reorgs, broadcasts a `ReorgEvent` containing:
    ///    - Blocks from the old fork (now orphaned)
    ///    - Blocks from the new fork (now canonical)
    ///    - The common ancestor where the fork occurred
    /// 5. Detects and sends epoch events for any epoch blocks found (both in extensions and reorgs)
    /// 6. Notifies services of the new confirmed block
    /// 7. Handles block migration (migrates chunks to disk and updates block index)
    /// 8. Broadcasts `BlockStateUpdated` event to inform subscribers of the block's new state
    ///
    /// When a block fails validation:
    /// 1. Logs the invalid block
    /// 2. Removes the block from the cache (also removes any children)
    /// 3. Broadcasts `BlockStateUpdated` event marking the block as discarded
    ///
    /// The function carefully manages cache locks to avoid deadlocks during async operations,
    /// releasing the write lock before sending events that may trigger callbacks.
    #[tracing::instrument(level = "trace", skip_all, err, fields(block_hash, validation_result))]
    async fn on_block_validation_finished(
        &mut self,
        block_hash: H256,
        validation_result: ValidationResult,
    ) -> eyre::Result<()> {
        // Handle a failed validation first
        if let ValidationResult::Invalid(validation_error) = &validation_result {
            error!(
                block.hash = %block_hash,
                error = %validation_error,
                "block validation failed"
            );
            let mut cache = self
                .cache
                .write()
                .expect("block tree cache write lock poisoned");

            let height = cache.get_block(&block_hash).map(|x| x.height).unwrap_or(0);
            let state = cache
                .get_block_and_status(&block_hash)
                .map(|(_, state)| *state)
                .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

            // Remove the block
            if let Err(err) = cache.remove_block(&block_hash) {
                tracing::error!(
                    block.hash = %block_hash,
                    ?err,
                    "Failed to remove block from cache"
                );
            }

            let event = BlockStateUpdated {
                block_hash,
                // todo: restructure the event so that `height` and `state` is not part of it
                height,
                state,
                discarded: true,
                validation_result,
            };
            if let Err(e) = self.service_senders.block_state_events.send(event) {
                tracing::trace!(
                    block.hash = ?block_hash,
                    block.height = height,
                    "Failed to broadcast block state update event: {}", e
                );
            }

            return Ok(());
        }

        // From here, we are processing a fully validated block
        let Some(height) = self
            .cache
            .read()
            .expect("cache read lock poisoned")
            .get_block(&block_hash)
            .map(|block| block.height)
        else {
            // most likely the block was stuck in the validation queue for a bit and it got pruned out of the block_tree
            tracing::warn!(
                "block validation returned a result for a block that's no longer in block cache"
            );
            return Ok(());
        };
        debug!(
            "\u{001b}[32mOn validation complete : result {} {:?} at height: {}\u{001b}[0m",
            block_hash, validation_result, height
        );

        let (arc_block, epoch_block, reorg_event, tip_changed, state, new_canonical_markers) = {
            let binding = self.cache.clone();
            let mut cache = binding.write().expect("cache write lock poisoned");

            // Get the current tip before any changes
            // Note: We can't rely on canonical chain here, because the canonical chain was already updated when this
            //       block arrived and was added after pre-validation. While the canonical head advances immediately, the
            //       `cache.tip` only moves after full validation.
            let old_tip = cache.tip;
            let old_tip_block = cache
                .get_block(&old_tip)
                .ok_or_else(|| eyre::eyre!("old tip block {old_tip} not found in cache"))?
                // todo: expensive clone here
                .clone();

            // Mark block as validated in cache, this will update the canonical chain
            if let Err(err) = cache.mark_block_as_valid(&block_hash) {
                error!("Failed to mark block {} as valid: {}", block_hash, err);
                return Ok(());
            }

            let Some((_block_entry, fork_blocks, _)) =
                cache.get_earliest_not_onchain_in_longest_chain()
            else {
                if block_hash == old_tip {
                    debug!(
                    "\u{001b}[32mSame Tip Marked current tip {} cdiff: {} height: {}\u{001b}[0m",
                    block_hash, old_tip_block.cumulative_diff, old_tip_block.height
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
                return Ok(());
            };

            // if the old tip isn't in the fork_blocks, it's a reorg
            let is_reorg = !fork_blocks.iter().any(|bh| bh.block_hash == old_tip);

            // Get block info before mutable operations
            let block_entry = cache
                .blocks
                .get(&block_hash)
                .unwrap_or_else(|| panic!("block entry {block_hash} not found in cache"));
            let arc_block = Arc::new(block_entry.block.clone());

            let tip_changed = {
                let old_tip_block = cache
                    .get_block(&cache.tip)
                    .ok_or_eyre("tip block must always be present")?;

                // only mark the tip if the new tip has higher cumulative difficulty than the old one
                if old_tip_block.cumulative_diff >= arc_block.cumulative_diff {
                    // this also means that the tip can point to a block in a chain that is not
                    // the canonical one (aka which the self.max_cumulative_difficulty is pointing at).
                    // That is valid because the blocks below self.max_cumulative_difficulty
                    // could still be undergoing validation, which is not guaranteed to succeed
                    false
                } else {
                    cache.mark_tip(&block_hash)?
                }
            };

            let (epoch_block, reorg_event, fcu_markers) = if tip_changed {
                let block_index_read = self.block_index_guard.read();
                let markers = ForkChoiceMarkers::from_block_tree(
                    &cache,
                    &block_index_read,
                    &self.db,
                    self.config.consensus.block_migration_depth as usize,
                    self.config.consensus.block_tree_depth as usize,
                )?;
                let new_fcu_markers = Some(markers);

                // Prune the cache after tip changes.
                //
                // Subtract 1 to ensure we keep exactly `depth` blocks.
                // The cache.prune() implementation does not count `tip` into the depth
                // equation, so it's always tip + `depth` that's kept around
                cache.prune(self.config.consensus.block_tree_depth.saturating_sub(1));

                if is_reorg {
                    // =====================================
                    // BLOCKCHAIN REORGANIZATION HANDLING
                    // =====================================

                    // Collect all blocks that are being orphaned (from the prior canonical chain)
                    let mut orphaned_blocks = cache.get_fork_blocks(&old_tip_block);
                    orphaned_blocks.push(&old_tip_block);

                    // Find the fork point where the old and new chains diverged
                    let fork_hash = orphaned_blocks
                        .first()
                        .expect("no orphaned blocks to determine fork point")
                        .block_hash;
                    let fork_block = cache
                        .get_block(&fork_hash)
                        .unwrap_or_else(|| panic!("fork block {fork_hash} not found in cache"));
                    let fork_height = fork_block.height;

                    // Convert orphaned blocks to BlockTreeEntry to make a snapshot of the old canonical chain
                    let mut old_canonical = Vec::with_capacity(orphaned_blocks.len());
                    for block in &orphaned_blocks {
                        let entry = make_block_tree_entry(block);
                        old_canonical.push(entry);
                    }

                    // Get the new canonical chain that's replacing the orphaned blocks
                    let new_canonical = cache.get_canonical_chain();

                    for o in old_canonical.iter() {
                        debug!("old_canonical({}) - {}", o.height, o.block_hash);
                    }

                    for o in new_canonical.0.iter() {
                        debug!("new_canonical({}) - {}", o.height, o.block_hash);
                    }

                    debug!("fork_height: {} fork_hash: {}", fork_height, fork_hash);

                    // Trim both chains back to their common ancestor to isolate the divergent portions
                    let (old_fork, new_fork) = prune_chains_at_ancestor(
                        old_canonical,
                        new_canonical.0,
                        fork_hash,
                        fork_height,
                    );

                    // Prepare lightweight block headers for reorg event (remove heavy chunk data)
                    let old_fork_blocks: Vec<Arc<IrysBlockHeader>> = old_fork
                        .iter()
                        .map(|e| {
                            let mut block = cache
                                .get_block(&e.block_hash)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "block {} not found in cache while preparing reorg event",
                                        e.block_hash
                                    )
                                })
                                .clone();
                            block.poa.chunk = None; // Remove chunk data to reduce memory footprint
                            Arc::new(block)
                        })
                        .collect();

                    let new_fork_blocks: Vec<Arc<IrysBlockHeader>> = new_fork
                        .iter()
                        .map(|e| {
                            let mut block = cache
                                .get_block(&e.block_hash)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "block {} not found in cache while preparing reorg event",
                                        e.block_hash
                                    )
                                })
                                .clone();
                            block.poa.chunk = None; // Remove chunk data to reduce memory footprint
                            Arc::new(block)
                        })
                        .collect();

                    debug!(
                        "\u{001b}[32mReorg at block height {} with {}\u{001b}[0m",
                        arc_block.height, arc_block.block_hash
                    );

                    // Create reorg event with all necessary data for downstream processing
                    let event = ReorgEvent {
                        old_fork: Arc::new(old_fork_blocks),
                        new_fork: Arc::new(new_fork_blocks),
                        fork_parent: Arc::new(fork_block.clone()),
                        new_tip: block_hash,
                        timestamp: SystemTime::now(),
                        db: Some(self.db.clone()),
                    };

                    // Was there an new epoch block found in the reorg
                    let new_epoch_block = event
                        .new_fork
                        .iter()
                        .find(|bh| self.is_epoch_block(bh))
                        .cloned();

                    (new_epoch_block, Some(event), new_fcu_markers)
                } else {
                    // =====================================
                    // NORMAL CHAIN EXTENSION
                    // =====================================
                    // New block extends the current longest chain without reorganization
                    debug!(
                        "\u{001b}[32mExtending longest chain to height {} with {} parent: {} height: {}\u{001b}[0m",
                        arc_block.height, arc_block.block_hash, old_tip_block.block_hash, old_tip_block.height
                    );

                    let new_epoch_block = if self.is_epoch_block(&arc_block) {
                        Some(arc_block.clone())
                    } else {
                        None
                    };

                    (new_epoch_block, None, new_fcu_markers)
                }
            } else {
                (None, None, None)
            };

            let state = cache
                .get_block_and_status(&block_hash)
                .map(|(_, state)| *state)
                .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

            (
                arc_block,
                epoch_block,
                reorg_event,
                tip_changed,
                state,
                fcu_markers,
            )
        }; // RwLockWriteGuard is dropped here, before the await

        // Send epoch events which require a Read lock
        if let Some(epoch_block) = epoch_block {
            // Send the epoch events
            self.send_epoch_events(&epoch_block)?;
        }

        // Now that the epoch events are sent, let the node know about the reorg
        if let Some(reorg_event) = reorg_event {
            // Broadcast reorg event using the shared sender
            if let Err(e) = self.service_senders.reorg_events.send(reorg_event) {
                debug!("No reorg subscribers: {:?}", e);
            }
        }

        if let Some(markers) = &new_canonical_markers {
            // Validate migration will succeed BEFORE emitting any events
            if tip_changed {
                self.validate_migration_continuity(&markers.migration_block)?;
                info!(
                    block.height = arc_block.height,
                    block.hash = ?arc_block.block_hash,
                    block.timestamp_ms = arc_block.timestamp.as_millis(),
                    "New canonical tip",
                );
            }

            // Emit consensus events (only after we know migration will succeed)
            self.emit_fcu(markers).await?;
            self.emit_block_confirmed(markers);

            // Perform the actual migration
            if tip_changed {
                self.migrate_block(&markers.migration_block).await?;
            }
        }

        // Broadcast difficulty update to miners if tip difficulty changed from parent
        let parent_diff_changed = tip_changed && {
            let cache = self.cache.read().expect("cache read lock poisoned");
            let parent_block = cache
                .get_block(&arc_block.previous_block_hash)
                .unwrap_or_else(|| {
                    panic!(
                        "parent block {} not found in cache while broadcasting difficulty update",
                        arc_block.previous_block_hash
                    )
                });
            parent_block.diff != arc_block.diff
        };
        if parent_diff_changed {
            self.service_senders
                .send_mining_difficulty(BroadcastDifficultyUpdate(arc_block.clone()));
        }

        let event = BlockStateUpdated {
            block_hash,
            height,
            state,
            discarded: false,
            validation_result: ValidationResult::Valid,
        };
        if let Err(e) = self.service_senders.block_state_events.send(event) {
            tracing::trace!(
                block.hash = ?block_hash,
                block.height = height,
                "Failed to broadcast block state update event: {}", e
            );
        }

        if state == ChainState::Onchain && self.config.consensus.enable_custody_proofs {
            let msg = crate::custody_proof_service::CustodyProofMessage::NewBlock {
                vdf_output: arc_block.vdf_limiter_info.output,
                block_height: height,
            };
            if let Err(e) = self.service_senders.custody_proof.send(msg) {
                tracing::warn!(
                    block.hash = ?block_hash,
                    block.height = height,
                    error = %e,
                    "Failed to send custody proof new block trigger",
                );
            }
        }

        Ok(())
    }

    fn is_epoch_block(&self, block_header: &Arc<IrysBlockHeader>) -> bool {
        block_header
            .height()
            .is_multiple_of(self.config.consensus.epoch.num_blocks_in_epoch)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %epoch_block.block_hash(), block.height = epoch_block.height()))]
    fn send_epoch_events(&self, epoch_block: &Arc<IrysBlockHeader>) -> eyre::Result<()> {
        // Get the epoch snapshot
        let block_hash = epoch_block.block_hash();
        let epoch_snapshot = self
            .cache
            .read()
            .expect("cache read lock poisoned")
            .get_epoch_snapshot(&block_hash)
            .ok_or_else(|| {
                eyre::eyre!(
                    "epoch block should have a snapshot in cache {:?}",
                    block_hash
                )
            })?;

        // Check for partitions expired at this epoch boundary
        if let Some(expired_partition_infos) = &epoch_snapshot.expired_partition_infos {
            let expired_partition_hashes: Vec<_> = expired_partition_infos
                .iter()
                .map(|i| i.partition_hash)
                .collect();

            // Let miners know about expired partitions
            self.service_senders
                .send_partitions_expiration(BroadcastPartitionsExpiration(H256List(
                    expired_partition_hashes,
                )));

            // Let the cache service know some term ledger slots expired
            if let Err(e) = self.service_senders.chunk_cache.send(
                crate::cache_service::CacheServiceAction::OnEpochProcessed(
                    epoch_snapshot.clone(),
                    None,
                ),
            ) {
                error!("Failed to send EpochProcessed event to CacheService for block {} (height {}): {}", block_hash, epoch_block.height(), e);
            }
        }

        // Let the node know about any newly assigned partition hashes to local storage modules
        let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();
        self.service_senders.storage_modules.send(
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: storage_module_infos.into(),
                update_height: epoch_block.height,
            },
        )?;
        Ok(())
    }

    /// Fetches full transaction headers from mempool using the txids from a ledger in a block
    async fn get_data_ledger_tx_headers_from_mempool(
        &self,
        block_header: &IrysBlockHeader,
        ledger: DataLedger,
    ) -> eyre::Result<Vec<DataTransactionHeader>> {
        // FIXME: when we add multiple term ledgers this will not work as there may be gaps in the index range
        // Explicitly cast enum to index
        let ledger_index = ledger as usize;

        let data_tx_ids = block_header
            .data_ledgers
            .get(ledger_index)
            .ok_or_else(|| eyre::eyre!("Ledger index {} out of bounds", ledger_index))?
            .tx_ids
            .0
            .clone();
        let mempool = self.service_senders.mempool.clone();

        let (tx, rx) = oneshot::channel();
        mempool
            .send(MempoolServiceMessage::GetDataTxs(data_tx_ids.clone(), tx))
            .map_err(|_| eyre::eyre!("Failed to send request to mempool"))?;

        let received = rx
            .await
            .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?
            .into_iter()
            .flatten()
            .collect::<Vec<DataTransactionHeader>>();

        if received.len() != data_tx_ids.len() {
            return Err(eyre::eyre!(
                "Mismatch in {:?} tx count: expected {}, got {}",
                ledger,
                data_tx_ids.len(),
                received.len()
            ));
        }

        Ok(received)
    }
}

/// Prunes two canonical chains at the specified common ancestor, returning only the divergent portions
/// Returns (old_chain_from_fork, new_chain_from_fork)
pub fn prune_chains_at_ancestor(
    old_chain: Vec<BlockTreeEntry>,
    new_chain: Vec<BlockTreeEntry>,
    ancestor_hash: BlockHash,
    ancestor_height: u64,
) -> (Vec<BlockTreeEntry>, Vec<BlockTreeEntry>) {
    // Find the ancestor index in the old chain
    let old_ancestor_idx = old_chain
        .iter()
        .position(|e| e.block_hash == ancestor_hash && e.height == ancestor_height)
        .expect("Common ancestor should exist in old chain");

    // Find the ancestor index in the new chain
    let new_ancestor_idx = new_chain
        .iter()
        .position(|e| e.block_hash == ancestor_hash && e.height == ancestor_height)
        .expect("Common ancestor should exist in new chain");

    // Return the portions after the common ancestor (excluding the ancestor itself)
    let old_divergent = old_chain[old_ancestor_idx + 1..].to_vec();
    let new_divergent = new_chain[new_ancestor_idx + 1..].to_vec();

    (old_divergent, new_divergent)
}

/// Result of block validation.
#[derive(Debug, Clone)]
pub enum ValidationResult {
    Valid,
    Invalid(crate::block_validation::ValidationError),
}
