use crate::{
    block_index_service::BlockIndexService,
    broadcast_mining_service::{BroadcastMiningService, BroadcastPartitionsExpiration},
    chunk_migration_service::ChunkMigrationService,
    mempool_service::MempoolServiceMessage,
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    validation_service::ValidationServiceMessage,
    BlockFinalizedMessage, StorageModuleServiceMessage,
};
use actix::prelude::*;
use base58::ToBase58 as _;
use eyre::{eyre, Context as _};
use irys_config::StorageSubmodulesConfig;
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::{
    block_index_guard::BlockIndexReadGuard, create_commitment_snapshot_for_block,
    create_epoch_snapshot_for_block, make_block_tree_entry, BlockState, BlockTree, BlockTreeEntry,
    BlockTreeReadGuard, ChainState, EpochReplayData,
};
use irys_types::{
    Address, BlockHash, CommitmentTransaction, Config, DataLedger, DataTransactionHeader,
    DatabaseProvider, H256List, IrysBlockHeader, TokioServiceHandle, H256,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    sync::{Arc, RwLock},
    time::SystemTime,
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{debug, error, info, warn};

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
        commitment_txs: Arc<Vec<CommitmentTransaction>>,
        response: oneshot::Sender<eyre::Result<()>>,
    },
    BlockValidationFinished {
        block_hash: H256,
        validation_result: ValidationResult,
    },
    FastTrackStorageFinalized {
        block_header: IrysBlockHeader,
        response: oneshot::Sender<eyre::Result<Option<Addr<RethServiceActor>>>>,
    },
    ReloadCacheFromDb {
        response: oneshot::Sender<eyre::Result<()>>,
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
    pub miner_address: Address,
    /// Read view of the `block_index`
    pub block_index_guard: BlockIndexReadGuard,
    /// Global storage config
    pub config: Config,
    /// Storage submodules configuration
    pub storage_submodules_config: StorageSubmodulesConfig,
    /// Channels for communicating with the services
    pub service_senders: ServiceSenders,
    /// Reth service actor sender
    pub reth_service_actor: Addr<RethServiceActor>,
    /// Current actix system
    pub system: System,
    /// Tracing span
    pub span: tracing::Span,
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

#[derive(Debug, Clone)]
pub struct BlockStateUpdated {
    pub block_hash: BlockHash,
    pub height: u64,
    pub state: ChainState,
    pub discarded: bool,
}

impl BlockTreeService {
    /// Spawn a new BlockTree service
    pub fn spawn_service(
        rx: UnboundedReceiver<BlockTreeServiceMessage>,
        db: DatabaseProvider,
        block_index_guard: BlockIndexReadGuard,
        epoch_replay_data: &EpochReplayData,
        storage_submodules_config: &StorageSubmodulesConfig,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_service_actor: Addr<RethServiceActor>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning block tree service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        // Dereference miner_address here, before the closure
        let miner_address = config.node_config.miner_address();
        let service_senders = service_senders.clone();
        let system = System::current();
        let bi_guard = block_index_guard;
        let epoch_replay_data = (*epoch_replay_data).clone();
        let config = config.clone();
        let storage_submodules_config = storage_submodules_config.clone();
        let span = tracing::Span::current();

        let handle = runtime_handle.spawn(async move {
            let cache = BlockTree::restore_from_db(
                bi_guard.clone(),
                epoch_replay_data,
                db.clone(),
                &storage_submodules_config,
                config.clone(),
            );

            //  Notify reth service
            let tip_hash = {
                let block_index = bi_guard.read();
                block_index.get_latest_item().unwrap().block_hash
            };

            reth_service_actor
                .try_send(ForkChoiceUpdateMessage {
                    head_hash: BlockHashType::Irys(tip_hash),
                    confirmed_hash: None,
                    finalized_hash: None,
                })
                .expect("could not send message to `RethServiceActor`");

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
                    reth_service_actor,
                    system,
                    span,
                    storage_submodules_config: storage_submodules_config.clone(),
                },
            };
            block_tree_service
                .start()
                .await
                .expect("BlockTree encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "block_tree_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

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

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        tracing::info!("shutting down BlockTree service gracefully");
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
                commitment_txs,
                response,
            } => {
                let result = self.on_block_prevalidated(block, commitment_txs);
                let _ = response.send(result);
            }
            BlockTreeServiceMessage::BlockValidationFinished {
                block_hash,
                validation_result,
            } => {
                self.on_block_validation_finished(block_hash, validation_result)
                    .await?;
            }
            BlockTreeServiceMessage::FastTrackStorageFinalized {
                block_header,
                response,
            } => {
                let result = self
                    .fast_track_storage_finalized_message(block_header)
                    .await;
                let _ = response.send(result);
            }
            BlockTreeServiceMessage::ReloadCacheFromDb { response } => {
                let res = self.reload_cache_from_db().await;
                let _ = response.send(res);
            }
        }
        Ok(())
    }

    async fn reload_cache_from_db(&self) -> eyre::Result<()> {
        let replay_data =
            EpochReplayData::query_replay_data(&self.db, &self.block_index_guard, &self.config)
                .await?;
        debug!("Reloading block tree cache from database");
        let new_block_tree_cache = BlockTree::restore_from_db(
            self.block_index_guard.clone(),
            replay_data,
            self.db.clone(),
            &self.storage_submodules_config,
            self.config.clone(),
        );
        *self.cache.write().unwrap() = new_block_tree_cache;

        //  Notify reth service
        let tip_hash = {
            let block_index = self.block_index_guard.read();
            block_index.get_latest_item().unwrap().block_hash
        };

        self.reth_service_actor
            .try_send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Irys(tip_hash),
                confirmed_hash: None,
                finalized_hash: None,
            })
            .expect("could not send message to `RethServiceActor`");

        Ok(())
    }

    /// Fast tracks the storage finalization of a block by retrieving transaction headers. Do
    /// after the block has been migrated.
    async fn fast_track_storage_finalized_message(
        &self,
        block_header: IrysBlockHeader,
    ) -> eyre::Result<Option<Addr<RethServiceActor>>> {
        let submit_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Submit)
            .await?;
        let publish_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Publish)
            .await?;

        let mut all_txs = vec![];
        all_txs.extend(publish_txs);
        all_txs.extend(submit_txs);

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
        Ok(Some(self.reth_service_actor.clone()))
    }

    async fn send_storage_finalized_message(&self, block_hash: BlockHash) -> eyre::Result<()> {
        // retrieve block header from the mempool or database
        let block_header = {
            let (tx_block, rx_block) = oneshot::channel();
            self.service_senders
                .mempool
                .send(MempoolServiceMessage::GetBlockHeader(
                    block_hash, false, tx_block,
                ))?;
            match rx_block.await? {
                Some(h) => h,
                None => self
                    .db
                    .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
                    .ok_or_else(|| eyre!("No block header found for hash {}", block_hash,))?,
            }
        };

        let submit_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Submit)
            .await?;
        let publish_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Publish)
            .await?;

        let mut all_txs = vec![];
        all_txs.extend(publish_txs);
        all_txs.extend(submit_txs);

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
            .send(MempoolServiceMessage::BlockConfirmed(
                confirmed_block.clone(),
            ))
            .expect("mempool service has unexpectedly become unreachable");
    }

    /// Checks if a block that is `block_migration_depth` blocks behind `arc_block`
    /// should be migrated. If eligible, sends migration message unless block
    /// is already in `block_index`. Panics if the `block_tree` and `block_index` are
    /// inconsistent.
    async fn try_notify_services_of_block_migration(&self, arc_block: &Arc<IrysBlockHeader>) {
        let finalized_hash = {
            let binding = self.cache.clone();
            let cache = binding.write().unwrap();
            let migration_depth = self.config.consensus.block_migration_depth as usize;

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
                .position(|x| x.block_hash == arc_block.block_hash)
            else {
                info!("Validated block not in longest chain, block {} height: {}, skipping finalization",arc_block.block_hash, arc_block.height);
                return;
            };

            if current_index < migration_depth {
                return; // Block already finalized
            }

            let finalize_index = current_index - migration_depth;
            let finalized_hash = longest_chain[finalize_index].block_hash;
            let finalized_height = longest_chain[finalize_index].height;

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
                        block: migrated_block,
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
            // TODO: this is the wrong place for this, it should be at the prune depth not the block_migration_depth
            if let Err(e) = self.reth_service_actor.try_send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Irys(cache.tip),
                confirmed_hash: None,
                finalized_hash: Some(BlockHashType::Irys(finalized_hash)),
            }) {
                panic!("Unable to send finalization message to reth: {}", &e)
            }

            finalized_hash
        }; // RwLockWriteGuard is dropped here, before the await

        if let Err(e) = self.send_storage_finalized_message(finalized_hash).await {
            error!("Unable to send block finalized message: {:?}", e);
        }
    }

    /// Handles pre-validated blocks received from the validation service.
    fn on_block_prevalidated(
        &mut self,
        block: Arc<IrysBlockHeader>,
        commitment_txs: Arc<Vec<CommitmentTransaction>>,
    ) -> eyre::Result<()> {
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

        let parent_block_entry = cache
            .blocks
            .get(&block.previous_block_hash)
            .expect("previous block to be in block tree");

        // Get te parent block's commitment snapshot
        let prev_commitment_snapshot = parent_block_entry.commitment_snapshot.clone();

        // Create epoch snapshot for this block
        let arc_epoch_snapshot =
            create_epoch_snapshot_for_block(&block, parent_block_entry, &self.config.consensus);

        // Create commitment snapshot for this block
        let commitment_snapshot = create_commitment_snapshot_for_block(
            &block,
            &commitment_txs,
            &prev_commitment_snapshot,
            arc_epoch_snapshot.clone(),
            &self.config.consensus,
        );

        // Create ema snapshot for this block
        let ema_snapshot = parent_block_entry.ema_snapshot.next_snapshot(
            &block,
            &parent_block_entry.block,
            &self.config.consensus,
        )?;

        let add_result = cache.add_block(
            &block,
            commitment_snapshot,
            arc_epoch_snapshot,
            ema_snapshot,
        );

        if add_result.is_ok() {
            // Schedule validation and mark as scheduled
            self.service_senders
                .validation_service
                .send(ValidationServiceMessage::ValidateBlock {
                    block: block.clone(),
                })
                .context("validation service unreachable!")?;

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
    async fn on_block_validation_finished(
        &mut self,
        block_hash: H256,
        validation_result: ValidationResult,
    ) -> eyre::Result<()> {
        let span = self.span.clone();
        let _span = span.enter();

        let height = self
            .cache
            .read()
            .unwrap()
            .get_block(&block_hash)
            .expect("block to be in cache")
            .height;

        debug!(
            "\u{001b}[32mOn validation complete : result {} {:?} at height: {}\u{001b}[0m",
            block_hash, validation_result, height
        );

        if validation_result == ValidationResult::Invalid {
            error!(block_hash = %block_hash.0.to_base58(),"invalid block");
            let mut cache = self.cache.write().unwrap();

            error!(block_hash = %block_hash.0.to_base58(),"invalid block");
            let Some(block_entry) = cache.get_block(&block_hash) else {
                // block not in the tree
                return Ok(());
            };
            // Get block state info before removal for the event
            let height = block_entry.height;
            let state = cache
                .get_block_and_status(&block_hash)
                .map(|(_, state)| *state)
                .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

            // Remove the block
            let _ = cache
                .remove_block(&block_hash)
                .inspect_err(|err| tracing::error!(?err));

            let event = BlockStateUpdated {
                block_hash,
                height,
                state,
                discarded: true,
            };
            let _ = self.service_senders.block_state_events.send(event);

            return Ok(());
        }

        let state;

        let (arc_block, epoch_block, reorg_event) = {
            let binding = self.cache.clone();
            let mut cache = binding.write().unwrap();

            // Get the current tip before any changes
            // Note: We can't rely on canonical chain here, because the canonical chain was already updated when this
            //       block arrived and was added after pre-validation. The tip only moves after full validation.
            let old_tip = cache.tip;
            let old_tip_block = cache.get_block(&old_tip).unwrap().clone();

            // Mark block as validated in cache, this will update the canonical chain
            if let Err(err) = cache.mark_block_as_valid(&block_hash) {
                error!("{}", err);
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
            let block_entry = cache.blocks.get(&block_hash).unwrap();
            let arc_block = Arc::new(block_entry.block.clone());

            // Now do mutable operations
            let mark_tip_result = if cache.mark_tip(&block_hash).is_ok() {
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
                    let fork_hash = orphaned_blocks.first().unwrap().block_hash;
                    let fork_block = cache.get_block(&fork_hash).unwrap();
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
                            let mut block = cache.get_block(&e.block_hash).unwrap().clone();
                            block.poa.chunk = None; // Remove chunk data to reduce memory footprint
                            Arc::new(block)
                        })
                        .collect();

                    let new_fork_blocks: Vec<Arc<IrysBlockHeader>> = new_fork
                        .iter()
                        .map(|e| {
                            let mut block = cache.get_block(&e.block_hash).unwrap().clone();
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

                    (arc_block, new_epoch_block, Some(event))
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

                    (arc_block, new_epoch_block, None)
                }
            } else {
                (arc_block, None, None)
            };

            state = cache
                .get_block_and_status(&block_hash)
                .map(|(_, state)| *state)
                .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

            mark_tip_result
        }; // RwLockWriteGuard is dropped here, before the await

        // Send epoch events which require a Read lock
        if let Some(epoch_block) = epoch_block {
            // Send the epoch events
            self.send_epoch_events(&epoch_block);
        }

        // Now that the epoch events are sent, let the node know about the reorg
        if let Some(reorg_event) = reorg_event {
            // Broadcast reorg event using the shared sender
            if let Err(e) = self.service_senders.reorg_events.send(reorg_event) {
                debug!("No reorg subscribers: {:?}", e);
            }
        }

        self.notify_services_of_block_confirmation(block_hash, &arc_block);

        // Handle block migration (move chunks to disk and add to block_index)
        self.try_notify_services_of_block_migration(&arc_block)
            .await;

        let event = BlockStateUpdated {
            block_hash,
            height,
            state,
            discarded: false,
        };
        let _ = self.service_senders.block_state_events.send(event);

        Ok(())
    }

    fn is_epoch_block(&self, block_header: &Arc<IrysBlockHeader>) -> bool {
        block_header.height % self.config.consensus.epoch.num_blocks_in_epoch == 0
    }

    fn send_epoch_events(&self, epoch_block: &Arc<IrysBlockHeader>) {
        // Get the epoch snapshot
        let epoch_snapshot = self
            .cache
            .read()
            .unwrap()
            .get_epoch_snapshot(&epoch_block.block_hash);

        let epoch_snapshot = epoch_snapshot.unwrap_or_else(|| {
            panic!(
                "Epoch block should have a snapshot in cache {}",
                epoch_block.block_hash
            )
        });
        let expired_partition_hashes = &epoch_snapshot.expired_partition_hashes;

        // Let the mining actors know about expired partitions
        System::set_current(self.system.clone());
        let mining_broadcaster_addr = BroadcastMiningService::from_registry();
        mining_broadcaster_addr.do_send(BroadcastPartitionsExpiration(H256List(
            expired_partition_hashes.clone(),
        )));

        // Let the node know about any newly assigned partition hashes to local storage modules
        let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();
        if let Err(e) = self.service_senders.storage_modules.send(
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: storage_module_infos.into(),
            },
        ) {
            error!("Failed to send partition assignments update: {}", e);
        }
    }

    /// Fetches full transaction headers from mempool using the txids from a ledger in a block
    async fn get_data_ledger_tx_headers_from_mempool(
        &self,
        block_header: &IrysBlockHeader,
        ledger: DataLedger,
    ) -> eyre::Result<Vec<DataTransactionHeader>> {
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValidationResult {
    Valid,
    Invalid,
}
