use crate::{
    StorageModuleServiceMessage,
    block_migration_service::BlockMigrationService,
    block_validation::PreValidationError,
    mempool_service::MempoolServiceMessage,
    metrics,
    mining_bus::{BroadcastDifficultyUpdate, BroadcastPartitionsExpiration},
    reth_service::{ForkChoiceUpdateMessage, RethServiceMessage},
    services::ServiceSenders,
    validation_service::ValidationServiceMessage,
};
use eyre::OptionExt as _;
use irys_domain::{
    BlockState, BlockTree, BlockTreeEntry, BlockTreeReadGuard, ChainState,
    block_index_guard::BlockIndexReadGuard, chain_sync_state::ChainSyncState,
    create_commitment_snapshot_for_block, create_epoch_snapshot_for_block,
    forkchoice_markers::ForkChoiceMarkers, make_block_tree_entry,
};
use irys_types::{
    BlockHash, Config, DatabaseProvider, H256, H256List, IrysAddress, IrysBlockHeader, SealedBlock,
    SendTraced as _, SystemLedger, TokioServiceHandle, Traced,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    sync::{Arc, RwLock},
    time::SystemTime,
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{Instrument as _, debug, error, info, warn};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

// Messages that the CommitmentCache service supports
#[derive(Debug)]
pub enum BlockTreeServiceMessage {
    GetBlockTreeReadGuard {
        response: oneshot::Sender<BlockTreeReadGuard>,
    },
    BlockPreValidated {
        block: Arc<SealedBlock>,
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
    msg_rx: UnboundedReceiver<Traced<BlockTreeServiceMessage>>,
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
    /// Channels for communicating with the services
    pub service_senders: ServiceSenders,
    /// Block migration orchestration and DB persistence
    block_migration_service: BlockMigrationService,
    /// Chain sync state for diagnostics
    pub chain_sync_state: ChainSyncState,
}

#[derive(Debug, Clone)]
pub struct ReorgEvent {
    pub old_fork: Arc<Vec<Arc<SealedBlock>>>,
    pub new_fork: Arc<Vec<Arc<SealedBlock>>>,
    pub fork_parent: Arc<IrysBlockHeader>,
    pub new_tip: BlockHash,
    pub timestamp: SystemTime,
    pub db: Option<DatabaseProvider>,
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
        rx: UnboundedReceiver<Traced<BlockTreeServiceMessage>>,
        db: DatabaseProvider,
        block_index_guard: BlockIndexReadGuard,
        config: &Config,
        service_senders: &ServiceSenders,
        chain_sync_state: ChainSyncState,
        block_migration_service: BlockMigrationService,
        cache: Arc<RwLock<BlockTree>>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning block tree service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let miner_address = config.node_config.miner_address();
        let service_senders = service_senders.clone();
        let config = config.clone();

        let handle = runtime_handle.spawn(
            async move {
                let block_tree_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: BlockTreeServiceInner {
                        block_migration_service,
                        db,
                        cache,
                        miner_address,
                        block_index_guard,
                        config,
                        service_senders,
                        chain_sync_state,
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

    #[tracing::instrument(name = "block_tree_service_start", level = "trace", skip_all)]
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
                traced = self.msg_rx.recv() => {
                    match traced {
                        Some(traced) => {
                            let (msg, parent_span) = traced.into_parts();
                            self.inner.handle_message(msg, parent_span).await?;
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
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, parent_span) = traced.into_parts();
            match msg {
                // Skip: talks to downstream services (reth FCU, migration) that
                // may already be stopped during shutdown.
                BlockTreeServiceMessage::BlockValidationFinished { .. } => {
                    debug!("Skipping BlockValidationFinished during shutdown drain");
                }
                msg => self.inner.handle_message(msg, parent_span).await?,
            }
        }

        tracing::info!("shutting down BlockTree service gracefully");
        Ok(())
    }
}

impl BlockTreeServiceInner {
    /// Dispatches received messages to appropriate handler methods and sends responses
    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn handle_message(
        &mut self,
        msg: BlockTreeServiceMessage,
        parent_span: tracing::Span,
    ) -> eyre::Result<()> {
        match msg {
            BlockTreeServiceMessage::GetBlockTreeReadGuard { response } => {
                let guard = BlockTreeReadGuard::new(self.cache.clone());
                if let Err(_guard) = response.send(guard) {
                    tracing::warn!("Block tree guard response channel was closed by receiver");
                }
            }
            BlockTreeServiceMessage::BlockPreValidated {
                block,
                skip_vdf_validation: skip_vdf,
                response,
            } => {
                let block_hash = block.header().block_hash;
                let block_height = block.header().height;
                let _guard = tracing::info_span!(parent: &parent_span, "block_tree.pre_validate", block.hash = %block_hash, block.height = block_height).entered();
                let result = self.on_block_prevalidated(block, skip_vdf);
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
                    .instrument(tracing::info_span!(parent: &parent_span, "block_tree.validation_finished", block.hash = %block_hash))
                    .await?;
            }
        }
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
            .send_traced(RethServiceMessage::ForkChoice {
                update: ForkChoiceUpdateMessage {
                    head_hash: markers.head.block_hash,
                    confirmed_hash: markers.migration_block.block_hash,
                    finalized_hash: markers.prune_block.block_hash,
                },
                response: tx,
            })
            .map_err(|e| eyre::eyre!("Reth service channel closed, cannot send FCU: {e}"))?;

        rx.await
            .map_err(|e| eyre::eyre!("Failed waiting for Reth FCU ack: {e}"))
    }

    /// Migrates finalized blocks into the block index and DB via `BlockMigrationService`.
    fn migrate_block(&mut self, block: &Arc<IrysBlockHeader>) -> eyre::Result<()> {
        self.block_migration_service.migrate_blocks(block)
    }

    /// Handles pre-validated blocks received from the validation service.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block.header().block_hash, block.height = block.header().height))]
    fn on_block_prevalidated(
        &mut self,
        block: Arc<SealedBlock>,
        skip_vdf: bool,
    ) -> eyre::Result<(), PreValidationError> {
        let block_header = block.header();
        let block_hash = &block_header.block_hash;
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
            .get(&block_header.previous_block_hash)
            .unwrap_or_else(|| {
                panic!(
                    "block {} needs to be in cache at height: {}",
                    block_header.previous_block_hash,
                    block_header.height - 1
                )
            });

        // Get the parent block's commitment snapshot
        let prev_commitment_snapshot = parent_block_entry.commitment_snapshot.clone();

        // Create epoch snapshot for this block
        let arc_epoch_snapshot = create_epoch_snapshot_for_block(
            block_header,
            parent_block_entry,
            &self.config.consensus,
        )
        .map_err(|x| PreValidationError::InvalidEpochSnapshot {
            error: x.to_string(),
        })?;

        // Create commitment snapshot for this block
        let commitment_snapshot = create_commitment_snapshot_for_block(
            block_header,
            block
                .transactions()
                .get_ledger_system_txs(SystemLedger::Commitment),
            &prev_commitment_snapshot,
            arc_epoch_snapshot.clone(),
            &self.config.consensus,
        );

        // Create ema snapshot for this block
        let ema_snapshot = parent_block_entry
            .ema_snapshot
            .next_snapshot(
                block_header,
                parent_block_entry.block.header(),
                &self.config.consensus,
            )
            .map_err(|e| PreValidationError::EmaSnapshotError(e.to_string()))?;

        cache
            .add_block(
                &block,
                commitment_snapshot,
                arc_epoch_snapshot,
                ema_snapshot,
            )
            .map_err(|e| {
                error!(
                    block.hash = ?block_hash,
                    block.height = block_header.height,
                    ?e,
                    "Failed to add block to block tree"
                );
                PreValidationError::AddBlockFailed {
                    block_hash: *block_hash,
                    reason: e.to_string(),
                }
            })?;

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

        // Record validation started for diagnostics
        self.chain_sync_state.record_validation_started(*block_hash);

        self.service_senders
            .validation_service
            .send_traced(ValidationServiceMessage::ValidateBlock {
                block: block.clone(),
                skip_vdf_validation: skip_vdf,
            })
            .map_err(|_| PreValidationError::ValidationServiceUnreachable)?;

        debug!(
            "scheduling block for validation: {} height: {}",
            block_hash, block_header.height
        );

        Ok(())
    }

    /// Handles the completion of full block validation.
    ///
    /// Successfully validated blocks update the canonical chain and may trigger a reorg.
    /// Cache locks are released before async operations to prevent deadlocks.
    #[tracing::instrument(level = "trace", skip_all, err, fields(block_hash, validation_result))]
    async fn on_block_validation_finished(
        &mut self,
        block_hash: H256,
        validation_result: ValidationResult,
    ) -> eyre::Result<()> {
        // Record validation finished for diagnostics (regardless of result)
        self.chain_sync_state
            .record_validation_finished(&block_hash);

        // Handle a failed validation first
        if let ValidationResult::Invalid(validation_error) = &validation_result {
            error!(
                block.hash = %block_hash,
                error = %validation_error,
                "block validation failed"
            );

            // Record validation error for diagnostics
            let error_message = format!("block={} error={}", block_hash, validation_error);
            self.chain_sync_state
                .record_block_validation_error(error_message);

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
            "On validation complete: result {} {:?} at height: {}",
            block_hash, validation_result, height
        );

        let (
            arc_block,
            epoch_block,
            reorg_event,
            tip_changed,
            state,
            new_canonical_markers,
            blocks_to_confirm,
        ) = {
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
                        "Same Tip Marked current tip {} cdiff: {} height: {}",
                        block_hash, old_tip_block.cumulative_diff, old_tip_block.height
                    );
                } else {
                    debug!(
                        "No new tip found {}, current tip {} cdiff: {} height: {}",
                        block_hash,
                        old_tip_block.block_hash,
                        old_tip_block.cumulative_diff,
                        old_tip_block.height
                    );
                }
                return Ok(());
            };

            // if the old tip isn't in the fork_blocks, it's a reorg
            let is_reorg = !fork_blocks
                .iter()
                .any(|bh| bh.header().block_hash == old_tip);

            // Get block info before mutable operations
            let block_entry = cache
                .blocks
                .get(&block_hash)
                .unwrap_or_else(|| panic!("block entry {block_hash} not found in cache"));
            let arc_block = block_entry.block.header().clone();

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

            let (epoch_block, reorg_event, fcu_markers, blocks_to_confirm) = if tip_changed {
                let block_index_read = self.block_index_guard.read();
                let markers = ForkChoiceMarkers::from_block_tree(
                    &cache,
                    block_index_read,
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
                    // Handle blockchain reorganization

                    // Collect all blocks that are being orphaned (from the prior canonical chain)
                    let mut orphaned_blocks =
                        cache.get_fork_blocks(old_tip_block.previous_block_hash);
                    orphaned_blocks.push(
                        cache
                            .blocks
                            .get(&old_tip_block.block_hash)
                            .expect("old tip must be in cache")
                            .block
                            .clone(),
                    );

                    // Find the fork point where the old and new chains diverged
                    let fork_block_sealed = orphaned_blocks
                        .first()
                        .expect("no orphaned blocks to determine fork point");
                    let fork_hash = fork_block_sealed.header().block_hash;
                    let fork_height = fork_block_sealed.header().height;
                    let fork_block = fork_block_sealed.header().clone();

                    // Convert orphaned blocks to BlockTreeEntry to make a snapshot of the old canonical chain
                    let mut old_canonical = Vec::with_capacity(orphaned_blocks.len());
                    for block in &orphaned_blocks {
                        let entry = make_block_tree_entry(Arc::clone(block));
                        old_canonical.push(entry);
                    }

                    // Get the new canonical chain that's replacing the orphaned blocks
                    let new_canonical = cache.get_canonical_chain();

                    for o in old_canonical.iter() {
                        debug!("old_canonical({}) - {}", o.height(), o.block_hash());
                    }

                    for o in new_canonical.0.iter() {
                        debug!("new_canonical({}) - {}", o.height(), o.block_hash());
                    }

                    debug!("fork_height: {} fork_hash: {}", fork_height, fork_hash);

                    // Trim both chains back to their common ancestor to isolate the divergent portions
                    let (old_fork, new_fork) = prune_chains_at_ancestor(
                        old_canonical,
                        new_canonical.0,
                        fork_hash,
                        fork_height,
                    );

                    // Pass sealed blocks directly (cheap Arc::clone, no deep copy)
                    let old_fork_blocks: Vec<Arc<SealedBlock>> = old_fork
                        .iter()
                        .map(|e| Arc::clone(e.sealed_block()))
                        .collect();

                    let new_fork_blocks: Vec<Arc<SealedBlock>> = new_fork
                        .iter()
                        .map(|e| Arc::clone(e.sealed_block()))
                        .collect();

                    let blocks_to_confirm = new_fork_blocks.clone();

                    debug!(
                        "Reorg at block height {} with {}",
                        arc_block.height, arc_block.block_hash
                    );
                    metrics::record_reorg();

                    // Create reorg event with all necessary data for downstream processing
                    let event = ReorgEvent {
                        old_fork: Arc::new(old_fork_blocks),
                        new_fork: Arc::new(new_fork_blocks),
                        fork_parent: fork_block,
                        new_tip: block_hash,
                        timestamp: SystemTime::now(),
                        db: Some(self.db.clone()),
                    };

                    // Was there a new epoch block found in the reorg
                    let new_epoch_block = event
                        .new_fork
                        .iter()
                        .find(|sb| self.is_epoch_block(sb.header()))
                        .map(|sb| Arc::clone(sb.header()));

                    (
                        new_epoch_block,
                        Some(event),
                        new_fcu_markers,
                        blocks_to_confirm,
                    )
                } else {
                    // Handle normal chain extension
                    debug!(
                        "Extending longest chain to height {} with {} parent: {} height: {}",
                        arc_block.height,
                        arc_block.block_hash,
                        old_tip_block.block_hash,
                        old_tip_block.height
                    );

                    let new_epoch_block = if self.is_epoch_block(&arc_block) {
                        Some(arc_block.clone())
                    } else {
                        None
                    };

                    let tip_sealed = cache
                        .blocks
                        .get(&block_hash)
                        .map(|meta| Arc::clone(&meta.block))
                        .expect("confirmed block must be in block tree cache");
                    let blocks_to_confirm = vec![tip_sealed];

                    (new_epoch_block, None, new_fcu_markers, blocks_to_confirm)
                }
            } else {
                let blocks_to_confirm: Vec<Arc<SealedBlock>> = vec![];
                (None, None, None, blocks_to_confirm)
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
                blocks_to_confirm,
            )
        }; // RwLockWriteGuard is dropped here, before the await

        // Send epoch events which require a Read lock
        if let Some(epoch_block) = epoch_block {
            // Send the epoch events
            self.send_epoch_events(&epoch_block)?;
        }

        // Persist metadata atomically (same code path for both normal blocks and reorgs)
        {
            let old_fork: &[Arc<SealedBlock>] = reorg_event
                .as_ref()
                .map_or(&[] as &[_], |e| e.old_fork.as_ref());
            self.block_migration_service
                .persist_metadata(old_fork, &blocks_to_confirm)?;
        }

        // Broadcast reorg event if applicable
        if let Some(reorg_event) = reorg_event
            && let Err(e) = self.service_senders.reorg_events.send(reorg_event)
        {
            error!(
                "Failed to broadcast reorg event - mempool state may be stale: {:?}",
                e
            );
        }

        if let Some(markers) = &new_canonical_markers {
            if tip_changed {
                info!(
                    block.height = arc_block.height,
                    block.hash = ?arc_block.block_hash,
                    block.timestamp_ms = arc_block.timestamp.as_millis(),
                    "New canonical tip",
                );
                metrics::record_canonical_tip_height(arc_block.height);
            }

            // Emit consensus events
            self.emit_fcu(markers).await?;

            // Emit block confirmations for all relevant blocks
            for sealed_block in &blocks_to_confirm {
                self.service_senders
                    .mempool
                    .send_traced(MempoolServiceMessage::BlockConfirmed(Arc::clone(
                        sealed_block,
                    )))
                    .expect("mempool service has unexpectedly become unreachable");
            }

            // Delegate migration to BlockMigrationService (validates continuity internally)
            if tip_changed {
                self.migrate_block(&markers.migration_block)?;
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
            if let Err(e) = self.service_senders.chunk_cache.send_traced(
                crate::cache_service::CacheServiceAction::OnEpochProcessed(
                    epoch_snapshot.clone(),
                    None,
                ),
            ) {
                error!(
                    "Failed to send EpochProcessed event to CacheService for block {} (height {}): {}",
                    block_hash,
                    epoch_block.height(),
                    e
                );
            }
        }

        // Let the node know about any newly assigned partition hashes to local storage modules
        let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();
        self.service_senders.storage_modules.send_traced(
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: storage_module_infos.into(),
                update_height: epoch_block.height,
            },
        )?;
        Ok(())
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
        .position(|e| e.block_hash() == ancestor_hash && e.height() == ancestor_height)
        .expect("Common ancestor should exist in old chain");

    // Find the ancestor index in the new chain
    let new_ancestor_idx = new_chain
        .iter()
        .position(|e| e.block_hash() == ancestor_hash && e.height() == ancestor_height)
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
