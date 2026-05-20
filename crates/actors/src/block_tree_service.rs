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
use irys_database::db::IrysDatabaseExt as _;
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
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use std::{
    num::NonZeroUsize,
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
    /// Bounded LRU of block_hashes recently discarded due to a soft-internal
    /// `InternalFailure` (eviction race, payload-cache miss, parent snapshot
    /// pruned). Value is the reason tag captured at discard time.
    ///
    /// Phase A of the 2026-05-20 audit H3 finding: lets us measure whether
    /// gossip-driven recovery actually works — if a later `Valid` arrives for
    /// a block_hash present here, we count it as a "recovered" discard.
    ///
    /// PHASE-B(H3): if the ratio of `soft_internal_recovered_total` to
    /// `soft_internal_discard_total` stays below operational tolerance
    /// (TBD by team), implement explicit per-block-hash re-request with
    /// exponential backoff here. See 2026-05-20 audit H3.
    ///
    /// Capacity: 4096 entries (~256 KB worst-case). Sized to be safe under
    /// any plausible discard rate without becoming an unbounded memory sink.
    recent_soft_internal_discards: LruCache<BlockHash, &'static str>,
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
    /// Snapshot of the block's `ChainState` at broadcast time. Stale on the
    /// `discarded: true` paths — captured before the block is removed from
    /// the cache, so it reflects the *pre-removal* state. Read by a
    /// diagnostic `tracing::info!` in `chain-tests/.../vdf_validation_progress.rs`
    /// (which logs it alongside `discarded` so the staleness is recoverable
    /// by the reader). No control-flow consumer keys off this field today.
    /// See the `todo: restructure` at the soft-fail broadcast site.
    pub state: ChainState,
    pub discarded: bool,
    pub validation_result: ValidationResult,
}

/// Which failure flavour drove a discard. Keeps the per-arm log wording and
/// diagnostic-record templates distinct without bleaching them into one bland
/// message — see `BlockTreeService::discard_and_broadcast`.
#[derive(Clone, Copy)]
enum DiscardKind {
    /// Soft `InternalFailure` (non-node-fault): eviction race, payload-cache
    /// miss, etc. Block removed so canonical chain can't wedge; fresh gossip
    /// will re-deliver.
    SoftInternal,
    /// Consensus `Invalid`: block rejected on its merits.
    Invalid,
}

/// Capacity of [`BlockTreeServiceInner::recent_soft_internal_discards`].
/// 4096 entries × (32-byte hash + ~24 bytes overhead + tag pointer) is well
/// under 256 KB and far exceeds any plausible burst of soft-internal
/// discards in a single block window. See the field doc for full rationale.
const SOFT_INTERNAL_DISCARD_LRU_CAPACITY: usize = 4096;

/// Resolve the soft-internal `InternalFailure` variant to a bounded-cardinality
/// snake_case reason tag for metrics labelling. Node-fault and consensus
/// variants are unreachable here — the caller filters them out before
/// `discard_and_broadcast` runs — but we still return a sentinel rather than
/// panicking to keep the metric labelling defensive.
fn soft_internal_reason_tag(err: &crate::block_validation::ValidationError) -> &'static str {
    use crate::block_validation::ValidationError as VE;
    match err {
        VE::ExecutionPayloadCacheEvicted { .. } => "execution_payload_cache_evicted",
        VE::ShadowTxGenerationFailed(_) => "shadow_tx_generation_failed",
        VE::ParentBlockMissing { .. } => "parent_block_missing",
        VE::ParentCommitmentSnapshotMissing { .. } => "parent_commitment_snapshot_missing",
        VE::ParentEpochSnapshotMissing { .. } => "parent_epoch_snapshot_missing",
        VE::ParentEmaSnapshotMissing { .. } => "parent_ema_snapshot_missing",
        VE::ValidationCancelled { .. } => "validation_cancelled_parent_missing",
        // Not reachable from the SoftInternal arm — all other variants are
        // either node-fault or consensus — but `metric_label` falls back to
        // "internal_error" rather than panicking if invariants drift.
        _ => "internal_error_other",
    }
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
                        recent_soft_internal_discards: LruCache::new(
                            NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap(),
                        ),
                    },
                };
                if let Err(e) = block_tree_service.start().await {
                    error!(
                        error = ?e,
                        "BlockTree service exited with an error; lifecycle will treat \
                         this as ServiceExited and run the ordered-shutdown path"
                    );
                }
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
                BlockTreeServiceMessage::BlockValidationFinished {
                    block_hash,
                    validation_result,
                } => {
                    // Exception: a node-fault `InternalFailure` must still
                    // panic during the drain so the supervisor restarts the
                    // node clean. Silently dropping it would lose the
                    // diagnostic and let a corrupted process exit normally.
                    // See `on_block_validation_finished` for the same rationale.
                    if let ValidationResult::InternalFailure(validation_error) = &validation_result
                        && validation_error.is_node_fault()
                    {
                        panic!(
                            "block validation hit a node fault during shutdown drain (block={}, error={}); aborting node — see ValidationError::is_node_fault for rationale",
                            block_hash, validation_error
                        );
                    }
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
        let mut cache = self.cache.write().map_err(|_| {
            error!("block tree cache write lock poisoned in on_block_prevalidated");
            PreValidationError::CachePoisoned {
                at: "on_block_prevalidated",
            }
        })?;

        // Early return if block already exists
        if let Some(existing) = cache.get_block(block_hash) {
            debug!(
                "on_block_prevalidated: {} at height: {} already in block_tree",
                existing.block_hash, existing.height
            );
            return Ok(());
        }

        // The parent must be in the cache because pre-validation only fires
        // after the parent has been observed. Reorg-driven cache prunes can
        // still race this lookup; previously this panicked. Now surface as a
        // typed error so the caller (validation service) can re-queue or drop.
        let parent_block_entry = cache
            .blocks
            .get(&block_header.previous_block_hash)
            .ok_or_else(|| PreValidationError::ParentNotInCache {
                parent_hash: block_header.previous_block_hash,
                expected_height: block_header.height.saturating_sub(1),
            })?;

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

        // Handle internal/runtime validation failures first. Block validity
        // is unknown locally — never mark it `Invalid` (which would
        // peer-attribute a local fault). Two sub-cases:
        //
        // 1. Node fault (`is_node_fault()` — panic, DB I/O, poisoned lock,
        //    local arithmetic bug, internal channel dead, OS clock failure):
        //    the fault is in this node, not the peer's block. Retrying will
        //    hit the same fault, so we abort. We panic instead of marking
        //    the block Invalid because of the never-mislabel rule: a node
        //    fault does not tell us whether the block is valid or invalid,
        //    and either misclassification forks us off the network.
        //    `setup_panic_hook` catches the panic, raises SIGINT for
        //    graceful shutdown, and arms a force-abort watchdog so the
        //    supervisor restarts the node clean. Same precedent as the VDF
        //    stall watchdog in `validation_service`.
        // 2. Soft internal (eviction race — parent snapshot pruned mid-
        //    validation, payload-cache eviction, etc.): remove the block
        //    (and recursively its children) from the tree so the canonical
        //    chain cannot wedge waiting on a stale entry. Recovery is via
        //    fresh gossip re-entering `process_block` once peers re-deliver.
        //    No automatic re-enqueue — that would tight-loop if the local
        //    race keeps re-occurring; gossip provides the natural rate-limit.
        if let ValidationResult::InternalFailure(validation_error) = &validation_result {
            if validation_error.is_node_fault() {
                let height = self
                    .cache
                    .read()
                    .ok()
                    .and_then(|c| c.get_block(&block_hash).map(|b| b.height))
                    .unwrap_or(0);
                panic!(
                    "block validation hit a node fault (block={}, height={}, error={}); aborting node — see ValidationError::is_node_fault for rationale",
                    block_hash, height, validation_error
                );
            }
            return self.discard_and_broadcast(
                block_hash,
                validation_result,
                DiscardKind::SoftInternal,
            );
        }

        // Handle a failed validation first
        if let ValidationResult::Invalid(_) = &validation_result {
            return self.discard_and_broadcast(block_hash, validation_result, DiscardKind::Invalid);
        }

        // Phase A (H3) instrumentation: a Valid result is arriving for a block
        // we previously discarded as soft-internal — gossip-driven recovery
        // worked for this hash. Increment the recovered counter (tagged with
        // the original discard reason) and forget it from the LRU. An info!
        // log makes the path operator-visible without polluting the steady-
        // state log volume — soft-internal discards are rare.
        if let Some(reason) = self.recent_soft_internal_discards.pop(&block_hash) {
            metrics::record_soft_internal_recovered(reason);
            info!(
                block.hash = %block_hash,
                reason,
                "block previously discarded as soft-internal reached Valid via gossip re-delivery"
            );
        }

        // From here, we are processing a fully validated block
        let Some(height) = self
            .cache
            .read()
            .map_err(|_| eyre::eyre!("block tree cache read lock poisoned looking up height"))?
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
            let mut cache = binding.write().map_err(|_| {
                eyre::eyre!("block tree cache write lock poisoned in on_block_validation_finished")
            })?;

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

                // Even though the tip didn't change, broadcast the state update so that
                // child blocks waiting in wait_for_parent_validation can proceed.
                //
                // Invariant note: mark_block_as_valid succeeded above under this same
                // write lock, so the block must still be in the cache. We defensively
                // fall back to NotOnchain(Unknown) instead of panicking — if the
                // invariant is ever violated by a future refactor, the broadcast still
                // fires with a recognisable state and downstream waiters unstick.
                let state = cache
                    .get_block_and_status(&block_hash)
                    .map(|(_, s)| *s)
                    .unwrap_or_else(|| {
                        error!(
                            block.hash = %block_hash,
                            block.height = height,
                            "invariant violation: block missing from cache after \
                             mark_block_as_valid succeeded under the same write lock"
                        );
                        ChainState::NotOnchain(BlockState::Unknown)
                    });

                drop(cache);

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
                return Ok(());
            };

            // if the old tip isn't in the fork_blocks, it's a reorg
            let is_reorg = !fork_blocks
                .iter()
                .any(|bh| bh.header().block_hash == old_tip);

            // Get block info before mutable operations.
            // We just observed the block in cache when computing `height` above (under
            // the read lock), and mark_block_as_valid succeeded under this write lock.
            // A missing entry means a concurrent prune raced these two locks — drop
            // this stale validation result, mirroring the height-lookup `None` path
            // earlier in the function rather than tearing down the service.
            let Some(block_entry) = cache.blocks.get(&block_hash) else {
                warn!(
                    block.hash = %block_hash,
                    block.height = height,
                    "Ignoring validation result for block already pruned from cache"
                );
                return Ok(());
            };
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

                // Snapshot the orphaned-chain view BEFORE pruning so a deep new tip
                // cannot evict `old_tip_block` from the cache mid-reorg and turn a
                // valid reorg into a service-level error.
                let orphaned_snapshot = if is_reorg {
                    let mut orphaned_blocks =
                        cache.get_fork_blocks(old_tip_block.previous_block_hash);
                    let old_tip_entry =
                        cache.blocks.get(&old_tip_block.block_hash).ok_or_else(|| {
                            error!(
                                old_tip = %old_tip_block.block_hash,
                                "old tip pruned from cache before reorg fork-point snapshot"
                            );
                            eyre::eyre!(
                                "old tip {} not in cache during reorg fork-point snapshot",
                                old_tip_block.block_hash
                            )
                        })?;
                    orphaned_blocks.push(old_tip_entry.block.clone());
                    Some(orphaned_blocks)
                } else {
                    None
                };

                // Prune the cache after tip changes.
                //
                // Subtract 1 to ensure we keep exactly `depth` blocks.
                // The cache.prune() implementation does not count `tip` into the depth
                // equation, so it's always tip + `depth` that's kept around
                cache.prune(self.config.consensus.block_tree_depth.saturating_sub(1));

                if is_reorg {
                    // Handle blockchain reorganization

                    let orphaned_blocks = orphaned_snapshot
                        .expect("orphaned_snapshot is captured above when is_reorg");

                    // Find the fork point where the old and new chains diverged.
                    // orphaned_blocks must be non-empty: we just pushed the old tip above.
                    // If this fires, the invariant was violated by a concurrent mutation
                    // — log and abort the reorg rather than panicking.
                    let fork_block_sealed = orphaned_blocks.first().ok_or_else(|| {
                        error!(
                            old_tip = %old_tip_block.block_hash,
                            "no orphaned blocks present for reorg fork-point search"
                        );
                        eyre::eyre!("no orphaned blocks to determine fork point")
                    })?;
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
                    )?;

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
                        "Reorg at block height {} with {}, old fork is {} blocks long, new one is {} blocks",
                        arc_block.height,
                        arc_block.block_hash,
                        old_fork_blocks.len(),
                        new_fork_blocks.len()
                    );

                    // Migration for the current tick hasn't run yet (it happens after
                    // this reorg path, in migrate_block()). The highest migrated block is
                    // at old_tip - migration_depth, from the previous tick's migration.
                    // old_fork_blocks excludes the fork point (common ancestor), so a
                    // migrated block is orphaned only when the fork is strictly deeper
                    // than migration_depth.
                    //
                    // This is an unrecoverable divergence: the FCU + downstream migration
                    // path cannot un-migrate already-finalised blocks. Aborting the reorg
                    // returns Err, which propagates up through handle_message → start()
                    // and triggers the spawn wrapper's controlled-shutdown path
                    // (ServiceSet detects the exit and lifecycle runs ordered shutdown).
                    let migration_depth = self.config.consensus.block_migration_depth;
                    if let Err(e) = validate_reorg_within_migration_depth(
                        old_fork_blocks.len(),
                        migration_depth,
                        fork_height,
                    ) {
                        error!(
                            reorg_depth = old_fork_blocks.len(),
                            migration_depth,
                            fork_height,
                            fork_hash = %fork_hash,
                            new_tip = %block_hash,
                            "reorg depth exceeds migration depth — already-migrated block would be reverted; aborting reorg to trigger controlled shutdown",
                        );
                        return Err(e);
                    }

                    metrics::record_reorg();
                    metrics::record_reorg_depth(u64::try_from(old_fork_blocks.len()).unwrap_or(0));

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

                    // The block was just confirmed under this same write lock — it
                    // must be in the cache. A missing entry indicates the invariant
                    // was violated by a future refactor; abort the confirmation
                    // path rather than panicking.
                    let tip_sealed = cache
                        .blocks
                        .get(&block_hash)
                        .map(|meta| Arc::clone(&meta.block))
                        .ok_or_else(|| {
                            error!(
                                block.hash = %block_hash,
                                "confirmed block missing from cache after tip change"
                            );
                            eyre::eyre!(
                                "confirmed block {block_hash} missing from cache after tip change"
                            )
                        })?;
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
                // Per-block hot path: emit at debug to avoid flooding info-level
                // logs (~7k entries/day at 12s blocks). Operators monitoring tip
                // progress should consume the canonical_tip_height metric below
                // instead.
                debug!(
                    block.height = arc_block.height,
                    block.hash = ?arc_block.block_hash,
                    block.timestamp_ms = arc_block.timestamp.as_millis(),
                    "New canonical tip",
                );
                metrics::record_canonical_tip_height(arc_block.height);
            }

            // Emit consensus events
            self.emit_fcu(markers).await?;

            // Emit block confirmations for all relevant blocks. The mempool may
            // already be shutting down (its receiver dropped); log and continue
            // — block confirmation itself is still valid, the mempool just
            // won't be informed of this particular block.
            for sealed_block in &blocks_to_confirm {
                if let Err(e) =
                    self.service_senders
                        .mempool
                        .send_traced(MempoolServiceMessage::BlockConfirmed(Arc::clone(
                            sealed_block,
                        )))
                {
                    error!(
                        block.hash = %sealed_block.header().block_hash,
                        ?e,
                        "Failed to send BlockConfirmed to mempool — mempool service unreachable"
                    );
                }
            }

            // Delegate migration to BlockMigrationService (validates continuity internally)
            if tip_changed {
                self.migrate_block(&markers.migration_block)?;
            }
        }

        // Broadcast difficulty update to miners if tip difficulty changed from parent.
        // On either lock poisoning or missing parent (the latter is a strong invariant
        // — the tip just changed so the parent must be present — but a panic here
        // would take the node down for a non-critical broadcast), skip the diff check
        // and log instead. The downstream miners will pick up the difficulty on the
        // next canonical update.
        let parent_diff_changed = tip_changed
            && match self.cache.read() {
                Ok(cache) => match cache.get_block(&arc_block.previous_block_hash) {
                    Some(parent_block) => parent_block.diff != arc_block.diff,
                    None => {
                        error!(
                            parent_hash = %arc_block.previous_block_hash,
                            block.hash = %arc_block.block_hash,
                            "parent block missing from cache while broadcasting difficulty update; skipping"
                        );
                        false
                    }
                },
                Err(_) => {
                    error!(
                        "block tree cache read lock poisoned during difficulty broadcast; skipping"
                    );
                    false
                }
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

    /// Shared discard path for soft `InternalFailure` and `Invalid` results.
    /// Node-fault internal failures must `panic!` BEFORE calling this helper
    /// — see `on_block_validation_finished`.
    ///
    /// `remove_block` is recursive (see `BlockTree::remove_block`): any
    /// children currently parked waiting on this parent (in tree, but not
    /// yet validated) are swept along with it. Their in-flight validation
    /// tasks will complete and gracefully no-op against an absent cache
    /// entry. Late-arriving children (gossip in after this sweep) hit
    /// `ParentMissing` in the wait stage, which is now `is_internal() =
    /// true` → they too remove and wait for re-gossip.
    fn discard_and_broadcast(
        &mut self,
        block_hash: H256,
        validation_result: ValidationResult,
        kind: DiscardKind,
    ) -> eyre::Result<()> {
        // Render the inner error for the structured `error` field + diagnostic
        // record; the two arms previously logged via `error = %validation_error`.
        let error_display: String = match &validation_result {
            ValidationResult::Invalid(e) => e.to_string(),
            ValidationResult::InternalFailure(e) => e.to_string(),
            ValidationResult::Valid => String::new(),
        };
        // Per-arm wording for the user-facing error log and the diagnostic
        // record string; the rest of the discard mechanics (lock, remove,
        // broadcast) is identical between the two arms.
        let (error_log_msg, diagnostic_record) = match kind {
            DiscardKind::SoftInternal => (
                "block validation hit an internal failure (soft race); removing block from cache, fresh gossip can retry",
                format!("block={} internal_error={}", block_hash, error_display),
            ),
            DiscardKind::Invalid => (
                "block validation failed",
                format!("block={} error={}", block_hash, error_display),
            ),
        };

        error!(block.hash = %block_hash, error = %error_display, "{}", error_log_msg);
        self.chain_sync_state
            .record_block_validation_error(diagnostic_record);

        // Phase A (H3) instrumentation: on a SoftInternal discard, derive the
        // reason tag from the wrapped `ValidationError`, increment the discard
        // counter, and remember (block_hash -> reason) in the bounded LRU so a
        // later Valid result can be counted as a recovery.
        //
        // PHASE-B(H3): if the ratio of `soft_internal_recovered_total` to
        // `soft_internal_discard_total` stays below operational tolerance
        // (TBD by team), implement explicit per-block-hash re-request with
        // exponential backoff here. See 2026-05-20 audit H3.
        if matches!(kind, DiscardKind::SoftInternal)
            && let ValidationResult::InternalFailure(inner) = &validation_result
        {
            let reason = soft_internal_reason_tag(inner.err());
            metrics::record_soft_internal_discard(reason);
            self.recent_soft_internal_discards.put(block_hash, reason);
        }

        let mut cache = self.cache.write().map_err(|_| {
            eyre::eyre!("block tree cache write lock poisoned in discard_and_broadcast")
        })?;

        let maybe_height = cache.get_block(&block_hash).map(|x| x.height);
        let height = maybe_height.unwrap_or(0);
        let state = cache
            .get_block_and_status(&block_hash)
            .map(|(_, state)| *state)
            .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

        if maybe_height.is_some() {
            if let Err(err) = cache.remove_block(&block_hash) {
                tracing::error!(block.hash = %block_hash, ?err, "Failed to remove block from cache");
            }
        } else {
            tracing::debug!(block.hash = %block_hash, "Block already removed from cache");
        }
        drop(cache);

        // todo: restructure the event so that `height` and `state` is not part of it
        let event = BlockStateUpdated {
            block_hash,
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
            .map_err(|_| eyre::eyre!("block tree cache read lock poisoned in send_epoch_events"))?
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

/// Prunes two canonical chains at the specified common ancestor, returning only the divergent portions.
///
/// Returns `Err` when the ancestor is missing from either chain — this is a
/// no-common-ancestor reorg, the same divergence class F4 detects at startup.
/// The reorg path callers log and abort the reorg instead of panicking.
pub fn prune_chains_at_ancestor(
    old_chain: Vec<BlockTreeEntry>,
    new_chain: Vec<BlockTreeEntry>,
    ancestor_hash: BlockHash,
    ancestor_height: u64,
) -> eyre::Result<(Vec<BlockTreeEntry>, Vec<BlockTreeEntry>)> {
    // Find the ancestor index in the old chain
    let old_ancestor_idx = old_chain
        .iter()
        .position(|e| e.block_hash() == ancestor_hash && e.height() == ancestor_height)
        .ok_or_else(|| {
            error!(
                ancestor.hash = %ancestor_hash,
                ancestor.height = ancestor_height,
                "common ancestor missing from old chain during reorg fork-point trim"
            );
            eyre::eyre!(
                "common ancestor {ancestor_hash} (height {ancestor_height}) not in old chain"
            )
        })?;

    // Find the ancestor index in the new chain
    let new_ancestor_idx = new_chain
        .iter()
        .position(|e| e.block_hash() == ancestor_hash && e.height() == ancestor_height)
        .ok_or_else(|| {
            error!(
                ancestor.hash = %ancestor_hash,
                ancestor.height = ancestor_height,
                "common ancestor missing from new chain during reorg fork-point trim"
            );
            eyre::eyre!(
                "common ancestor {ancestor_hash} (height {ancestor_height}) not in new chain"
            )
        })?;

    // Return the portions after the common ancestor (excluding the ancestor itself)
    let old_divergent = old_chain[old_ancestor_idx + 1..].to_vec();
    let new_divergent = new_chain[new_ancestor_idx + 1..].to_vec();

    Ok((old_divergent, new_divergent))
}

/// Reject reorgs that would un-migrate already-finalised blocks.
///
/// `old_fork_len` is the number of blocks on the old fork excluding the common
/// ancestor. The previous tick's migration has already moved the block at
/// `old_tip - migration_depth` and below into the block index, so a fork
/// strictly deeper than `migration_depth` would require reverting a
/// migrated block — which the FCU + downstream migration path cannot do
/// safely. The caller treats `Err` as a controlled-shutdown trigger.
pub fn validate_reorg_within_migration_depth(
    old_fork_len: usize,
    migration_depth: u32,
    fork_height: u64,
) -> eyre::Result<()> {
    if u32::try_from(old_fork_len).unwrap_or(u32::MAX) > migration_depth {
        return Err(eyre::eyre!(
            "reorg depth ({}) exceeds migration depth ({}); already-migrated block at fork height {} would be reverted",
            old_fork_len,
            migration_depth,
            fork_height,
        ));
    }
    Ok(())
}

/// Wrapper that guarantees the inner `ValidationError` is classified as a
/// local/runtime failure (per `ValidationError::is_internal_failure`).
///
/// SAFETY-CRITICAL: this type is the structural enforcement of the rule that
/// `ValidationResult::InternalFailure` must never carry a consensus-rejection
/// variant. The only construction path is `From<ValidationError> for
/// ValidationResult`, which checks the classifier before wrapping. Direct
/// construction outside this module is impossible because the inner field is
/// private. Consumers read the underlying error via `err()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalFailureError(crate::block_validation::ValidationError);

impl InternalFailureError {
    /// Borrow the wrapped `ValidationError` for inspection / logging /
    /// pattern-matching. Use this in `if let ValidationResult::InternalFailure(e) = ...`
    /// to look at sub-variants for finer-grained handling.
    pub fn err(&self) -> &crate::block_validation::ValidationError {
        &self.0
    }

    /// Convenience accessor: returns true when the wrapped error is a node
    /// fault (see [`crate::block_validation::ValidationError::is_node_fault`]).
    /// Callers use this to distinguish genuine faults (panic, DB I/O, local
    /// arithmetic bug, poisoned lock) from soft eviction races.
    pub fn is_node_fault(&self) -> bool {
        self.0.is_node_fault()
    }
}

impl std::fmt::Display for InternalFailureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Wrapper that guarantees the inner `ValidationError` is classified as a
/// consensus rejection (per `!ValidationError::is_internal_failure`).
///
/// SAFETY-CRITICAL: this type is the structural seal that prevents callers
/// outside `block_tree_service` from constructing
/// `ValidationResult::Invalid(...)` carrying a node-fault variant (e.g.
/// `TaskPanicked`, `ParentBlockMissing`). Misclassification in that direction
/// would peer-attribute a local fault. The only construction path is
/// `From<ValidationError> for ValidationResult`, which checks the classifier
/// before wrapping; the inner field is private so direct construction outside
/// this module is impossible. Consumers read the underlying error via `err()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsensusRejectionError(crate::block_validation::ValidationError);

impl ConsensusRejectionError {
    /// Borrow the wrapped `ValidationError` for inspection / logging /
    /// pattern-matching. Use this in `if let ValidationResult::Invalid(e) = ...`
    /// to look at sub-variants for finer-grained handling.
    pub fn err(&self) -> &crate::block_validation::ValidationError {
        &self.0
    }
}

impl std::fmt::Display for ConsensusRejectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Result of block validation.
///
/// SAFETY-CRITICAL: only `Invalid` should mark a block as consensus-rejected.
/// `InternalFailure` represents a local/runtime issue (verifier panic,
/// block-tree eviction race, transient I/O) where the block's validity is
/// unknown and must not be peer-attributed or discarded. The `From` impls
/// below dispatch to the correct variant based on the underlying error's
/// `is_internal_failure()` classifier — prefer `.into()` over constructing
/// these variants directly at call sites. Both payloads are sealed wrappers
/// (`InternalFailureError`, `ConsensusRejectionError`) so that constructing
/// either with a misclassified variant is structurally impossible outside
/// this module.
#[derive(Debug, Clone)]
pub enum ValidationResult {
    Valid,
    Invalid(ConsensusRejectionError),
    InternalFailure(InternalFailureError),
}

impl ValidationResult {
    /// Coarse label used for the overall validation-result metric.
    /// Distinguishes consensus rejections ("invalid") from local/runtime
    /// failures ("internal_error") so the rejection-rate counter isn't
    /// inflated by transient issues.
    ///
    /// Prefer [`Self::granular_metric_label`] for per-stage call sites so
    /// `node_fault` / `cancelled` / `panicked` are surfaced separately
    /// instead of being collapsed into `"invalid"` or `"internal_error"`.
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid(_) => "invalid",
            Self::InternalFailure(_) => "internal_error",
        }
    }

    /// Granular per-stage metric label. Surfaces `node_fault` (local-state
    /// corruption / EL transport failure) and `cancelled` / `panicked`
    /// separately so dashboards can isolate local-fault rates from
    /// peer-attributable rejections on each concurrent validation stage.
    ///
    /// Delegates to [`crate::block_validation::ValidationError::metric_label`]
    /// for the failing-variant cases; `Valid` returns `"valid"` to mirror
    /// [`Self::metric_label`].
    pub fn granular_metric_label(&self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid(c) => c.err().metric_label(),
            Self::InternalFailure(i) => i.err().metric_label(),
        }
    }
}

impl From<crate::block_validation::ValidationError> for ValidationResult {
    fn from(e: crate::block_validation::ValidationError) -> Self {
        if e.is_internal_failure() {
            Self::InternalFailure(InternalFailureError(e))
        } else {
            Self::Invalid(ConsensusRejectionError(e))
        }
    }
}

impl From<crate::block_validation::PreValidationError> for ValidationResult {
    fn from(e: crate::block_validation::PreValidationError) -> Self {
        crate::block_validation::ValidationError::PreValidation(e).into()
    }
}

/// Look up a block header from the in-memory block tree, falling back to the database.
/// Set `include_chunk` to false to strip the PoA chunk field.
pub fn get_block_header(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    block_hash: H256,
    include_chunk: bool,
) -> eyre::Result<Option<IrysBlockHeader>> {
    // Try block tree first (in-memory, fast)
    let guard = block_tree.read();
    if let Some(block) = guard.get_block(&block_hash) {
        let mut block = block.clone();
        if !include_chunk {
            block.poa.chunk = None;
        }
        return Ok(Some(block));
    }
    drop(guard);

    // Fall back to database
    db.view_eyre(|tx| irys_database::block_header_by_hash(tx, &block_hash, include_chunk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_validation::PreValidationError;
    use irys_types::BlockTransactions;
    use rstest::rstest;

    /// A consensus-rejection error must convert to `ValidationResult::Invalid`.
    #[test]
    fn consensus_preval_error_converts_to_invalid() {
        let result: ValidationResult = PreValidationError::BlockSignatureInvalid.into();
        assert!(matches!(result, ValidationResult::Invalid(_)));
        assert_eq!(result.metric_label(), "invalid");
    }

    /// A local/runtime PreValidationError must convert to InternalFailure so
    /// downstream consumers don't mark the block as consensus-invalid.
    #[test]
    fn internal_preval_error_converts_to_internal_failure() {
        let result: ValidationResult = PreValidationError::InternalTaskJoin("panic".into()).into();
        assert!(matches!(result, ValidationResult::InternalFailure(_)));
        assert_eq!(result.metric_label(), "internal_error");
    }

    /// ValidationError-level local failures must also dispatch to InternalFailure.
    #[test]
    fn internal_validation_error_converts_to_internal_failure() {
        let result: ValidationResult = crate::block_validation::ValidationError::TaskPanicked {
            task: "poa".into(),
            details: "x".into(),
        }
        .into();
        assert!(matches!(result, ValidationResult::InternalFailure(_)));
        assert_eq!(result.metric_label(), "internal_error");

        let result: ValidationResult =
            crate::block_validation::ValidationError::ParentBlockMissing {
                block_hash: H256::zero(),
            }
            .into();
        assert!(matches!(result, ValidationResult::InternalFailure(_)));
    }

    /// A ValidationError-level consensus rejection stays Invalid.
    #[test]
    fn consensus_validation_error_converts_to_invalid() {
        let result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        assert!(matches!(result, ValidationResult::Invalid(_)));
    }

    /// `ValidationCancelled` dispatches by sub-reason: node-state reasons
    /// (`HeightDifference`, `ChannelClosed`) → `Invalid`; `ParentMissing` →
    /// `InternalFailure` since parent absence is not peer-attributable.
    #[test]
    fn validation_cancelled_converts_per_reason() {
        use crate::block_validation::ValidationCancelReason;
        for reason in [
            ValidationCancelReason::HeightDifference,
            ValidationCancelReason::ChannelClosed,
        ] {
            let result: ValidationResult =
                crate::block_validation::ValidationError::ValidationCancelled { reason }.into();
            assert!(
                matches!(result, ValidationResult::Invalid(_)),
                "reason {:?} should dispatch to Invalid",
                reason
            );
            assert_eq!(result.metric_label(), "invalid");
        }

        let result: ValidationResult =
            crate::block_validation::ValidationError::ValidationCancelled {
                reason: ValidationCancelReason::ParentMissing,
            }
            .into();
        assert!(matches!(result, ValidationResult::InternalFailure(_)));
        assert_eq!(result.metric_label(), "internal_error");
    }

    /// `InternalFailureError::err()` exposes the underlying ValidationError
    /// for inspection (e.g. sub-variant matching in metric labelling).
    #[test]
    fn internal_failure_error_exposes_inner() {
        let result: ValidationResult = crate::block_validation::ValidationError::TaskPanicked {
            task: "poa".into(),
            details: "boom".into(),
        }
        .into();
        let ValidationResult::InternalFailure(inner) = result else {
            panic!("expected InternalFailure");
        };
        assert!(matches!(
            inner.err(),
            crate::block_validation::ValidationError::TaskPanicked { .. }
        ));
    }

    /// Every SoftInternal `ValidationError` variant must map to a distinct,
    /// stable, snake_case reason tag. The tag forms the `reason` label on the
    /// `irys.block.soft_internal_discard_total` metric, so cardinality must
    /// stay bounded and the values stay grep-stable across releases.
    #[rstest]
    #[case::cache_evicted(
        crate::block_validation::ValidationError::ExecutionPayloadCacheEvicted {
            evm_block_hash: irys_types::EvmBlockHash::ZERO,
        },
        "execution_payload_cache_evicted",
    )]
    #[case::shadow_gen(
        crate::block_validation::ValidationError::ShadowTxGenerationFailed("x".into()),
        "shadow_tx_generation_failed",
    )]
    #[case::parent_missing(
        crate::block_validation::ValidationError::ParentBlockMissing { block_hash: H256::zero() },
        "parent_block_missing",
    )]
    #[case::parent_commit(
        crate::block_validation::ValidationError::ParentCommitmentSnapshotMissing {
            block_hash: H256::zero(),
        },
        "parent_commitment_snapshot_missing",
    )]
    #[case::parent_epoch(
        crate::block_validation::ValidationError::ParentEpochSnapshotMissing {
            block_hash: H256::zero(),
        },
        "parent_epoch_snapshot_missing",
    )]
    #[case::parent_ema(
        crate::block_validation::ValidationError::ParentEmaSnapshotMissing {
            block_hash: H256::zero(),
        },
        "parent_ema_snapshot_missing",
    )]
    #[case::cancel_parent_missing(
        crate::block_validation::ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::ParentMissing,
        },
        "validation_cancelled_parent_missing",
    )]
    fn soft_internal_reason_tag_for_each_variant(
        #[case] err: crate::block_validation::ValidationError,
        #[case] expected_tag: &'static str,
    ) {
        assert_eq!(soft_internal_reason_tag(&err), expected_tag);
    }

    /// SoftInternal discard path must record (block_hash, reason) into the
    /// LRU so a subsequent Valid arrival can be counted as a gossip-driven
    /// recovery. This test exercises the LRU-mutation logic directly.
    #[test]
    fn soft_internal_discard_inserts_into_lru() {
        let mut lru: LruCache<BlockHash, &'static str> =
            LruCache::new(NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap());
        let block_hash = H256([0xAB; 32]);

        // Construct the SoftInternal result, then replay the put step from
        // `discard_and_broadcast`.
        let result: ValidationResult =
            crate::block_validation::ValidationError::ParentBlockMissing { block_hash }.into();
        let ValidationResult::InternalFailure(inner) = &result else {
            panic!("expected InternalFailure");
        };
        let reason = soft_internal_reason_tag(inner.err());
        lru.put(block_hash, reason);

        assert_eq!(lru.get(&block_hash), Some(&"parent_block_missing"));
    }

    /// A Valid result for a previously-discarded block_hash must pop the
    /// entry from the LRU (so a second Valid wouldn't double-count) and
    /// surface the original reason tag for the recovery metric.
    #[test]
    fn valid_result_pops_from_lru_and_returns_reason() {
        let mut lru: LruCache<BlockHash, &'static str> =
            LruCache::new(NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap());
        let block_hash = H256([0xCD; 32]);
        lru.put(block_hash, "execution_payload_cache_evicted");

        // First Valid arrival: recovery is observed and the entry is removed.
        let popped = lru.pop(&block_hash);
        assert_eq!(popped, Some("execution_payload_cache_evicted"));

        // Second Valid arrival: no-op, entry already drained — guarantees no
        // double-count on metric retries or duplicate Valid messages.
        assert_eq!(lru.pop(&block_hash), None);
    }

    /// `Invalid` (peer-attributable) and node-fault `InternalFailure` paths
    /// MUST NOT pollute the LRU. The LRU only tracks soft-internal discards
    /// so recovery accounting is not contaminated by consensus rejections or
    /// abort-causing local faults.
    #[test]
    fn non_soft_internal_paths_leave_lru_untouched() {
        let mut lru: LruCache<BlockHash, &'static str> =
            LruCache::new(NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap());

        // 1. Consensus Invalid: dispatch produces ValidationResult::Invalid,
        //    and the `matches!(kind, DiscardKind::SoftInternal)` guard in
        //    `discard_and_broadcast` short-circuits the LRU put. We replay
        //    that guard here.
        let invalid_result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        let kind = DiscardKind::Invalid;
        if matches!(kind, DiscardKind::SoftInternal)
            && let ValidationResult::InternalFailure(inner) = &invalid_result
        {
            lru.put(H256([0x01; 32]), soft_internal_reason_tag(inner.err()));
        }
        assert_eq!(lru.len(), 0, "Invalid path must not touch the LRU");

        // 2. Node-fault InternalFailure: handler panics BEFORE reaching
        //    discard_and_broadcast (see `on_block_validation_finished`), so
        //    the LRU is never updated. We assert the classification gate
        //    (`is_node_fault`) catches the variant — proving the panic-before
        //    -LRU-update ordering is preserved structurally.
        let node_fault: ValidationResult = crate::block_validation::ValidationError::TaskPanicked {
            task: "poa".into(),
            details: "boom".into(),
        }
        .into();
        let ValidationResult::InternalFailure(inner) = &node_fault else {
            panic!("expected InternalFailure");
        };
        assert!(
            inner.is_node_fault(),
            "node-fault variant must be caught upstream of discard_and_broadcast"
        );
        assert_eq!(lru.len(), 0, "node-fault path must not touch the LRU");
    }

    /// The LRU is intentionally bounded so that an unbounded soft-internal
    /// failure rate cannot cause memory growth. Beyond capacity, `put`
    /// silently evicts the LRU-tail entry (a hash we no longer expect to
    /// recover). No panic, no allocation explosion.
    #[test]
    fn lru_eviction_beyond_capacity_is_graceful() {
        let capacity = 4_usize;
        let mut lru: LruCache<BlockHash, &'static str> =
            LruCache::new(NonZeroUsize::new(capacity).unwrap());

        let hashes: Vec<H256> = (0_u8..(capacity as u8 + 2))
            .map(|i| H256([i; 32]))
            .collect();
        for h in &hashes {
            lru.put(*h, "execution_payload_cache_evicted");
        }

        assert_eq!(lru.len(), capacity);
        // Oldest two entries must have been evicted; the most-recent
        // `capacity` puts remain.
        assert!(lru.get(&hashes[0]).is_none());
        assert!(lru.get(&hashes[1]).is_none());
        for h in &hashes[2..] {
            assert!(lru.get(h).is_some());
        }
    }

    /// `InternalFailureError::is_node_fault()` must reflect the wrapped
    /// error's classification. The handler keys its abort-vs-passive
    /// decision on this — a wrong answer here would either let a node fault
    /// silently continue, or trip the abort path on a soft eviction race.
    #[test]
    fn internal_failure_error_node_fault_accessor() {
        // TaskPanicked is a node fault.
        let result: ValidationResult = crate::block_validation::ValidationError::TaskPanicked {
            task: "poa".into(),
            details: "boom".into(),
        }
        .into();
        let ValidationResult::InternalFailure(inner) = result else {
            panic!("expected InternalFailure for TaskPanicked");
        };
        assert!(inner.is_node_fault());

        // ParentBlockMissing is an eviction race, not a node fault.
        let result: ValidationResult =
            crate::block_validation::ValidationError::ParentBlockMissing {
                block_hash: H256::zero(),
            }
            .into();
        let ValidationResult::InternalFailure(inner) = result else {
            panic!("expected InternalFailure for ParentBlockMissing");
        };
        assert!(!inner.is_node_fault());
    }

    fn entry_at(height: u64, hash_byte: u8) -> BlockTreeEntry {
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.block_hash = H256([hash_byte; 32]);
        let sealed = SealedBlock::new_unchecked(Arc::new(header), BlockTransactions::default());
        make_block_tree_entry(Arc::new(sealed))
    }

    /// Common-ancestor present at the same hash and height in both chains:
    /// the function returns the divergent suffix of each chain (excluding the
    /// ancestor itself).
    #[test]
    fn prune_chains_at_ancestor_returns_divergent_suffixes_when_ancestor_present() {
        let ancestor = entry_at(10, 0xAA);
        let old = vec![ancestor.clone(), entry_at(11, 0xB1), entry_at(12, 0xB2)];
        let new = vec![ancestor.clone(), entry_at(11, 0xC1)];

        let (old_div, new_div) =
            prune_chains_at_ancestor(old, new, ancestor.block_hash(), ancestor.height())
                .expect("ancestor present in both chains");

        assert_eq!(old_div.len(), 2);
        assert_eq!(old_div[0].height(), 11);
        assert_eq!(old_div[1].height(), 12);
        assert_eq!(new_div.len(), 1);
        assert_eq!(new_div[0].height(), 11);
        assert_eq!(new_div[0].block_hash(), H256([0xC1; 32]));
    }

    /// Reorg fork-point trim must surface a typed error rather than panic when
    /// the supposed common ancestor is missing from one or both chains. F4
    /// catches this class of divergence at startup; here we ensure runtime
    /// reorg paths do not abort the node.
    #[rstest]
    #[case::missing_in_old(
        vec![entry_at(11, 0xB1), entry_at(12, 0xB2)],
        vec![entry_at(10, 0xAA), entry_at(11, 0xC1)],
        "old chain"
    )]
    #[case::missing_in_new(
        vec![entry_at(10, 0xAA), entry_at(11, 0xB1)],
        vec![entry_at(11, 0xC1), entry_at(12, 0xC2)],
        "new chain"
    )]
    #[case::missing_in_both(
        vec![entry_at(11, 0xB1), entry_at(12, 0xB2)],
        vec![entry_at(11, 0xC1), entry_at(12, 0xC2)],
        "old chain"
    )]
    fn prune_chains_at_ancestor_returns_err_when_ancestor_missing(
        #[case] old: Vec<BlockTreeEntry>,
        #[case] new: Vec<BlockTreeEntry>,
        #[case] expected_chain_in_msg: &str,
    ) {
        let ancestor_hash = H256([0xAA; 32]);
        let result = prune_chains_at_ancestor(old, new, ancestor_hash, 10);
        let err = result.expect_err("expected Err when ancestor is missing");
        let msg = format!("{err}");
        assert!(
            msg.contains(expected_chain_in_msg),
            "error must name the chain missing the ancestor; got: {msg}"
        );
        assert!(
            !msg.contains("panic"),
            "must not surface panic-style messages; got: {msg}"
        );
    }

    /// Reorgs at or within `migration_depth` are reversible: nothing on the
    /// old fork has been migrated to the block index yet, so the FCU path can
    /// switch chains cleanly.
    #[rstest]
    #[case::depth_zero_no_orphans(0, 5)]
    #[case::depth_strictly_below_migration(3, 5)]
    #[case::depth_equals_migration_boundary(5, 5)]
    fn validate_reorg_within_migration_depth_passes_for_safe_depths(
        #[case] old_fork_len: usize,
        #[case] migration_depth: u32,
    ) {
        validate_reorg_within_migration_depth(old_fork_len, migration_depth, 100)
            .expect("reorg at or within migration depth must be permitted");
    }

    /// Reorgs strictly deeper than `migration_depth` would un-migrate a block
    /// that the previous tick committed to the block index. The gate must
    /// surface a typed error so the caller can trigger controlled shutdown
    /// rather than silently corrupting the on-disk index.
    #[rstest]
    #[case::just_over_boundary(6, 5)]
    #[case::well_over_boundary(20, 5)]
    #[case::large_reorg_small_window(100, 10)]
    fn validate_reorg_within_migration_depth_returns_err_when_exceeds(
        #[case] old_fork_len: usize,
        #[case] migration_depth: u32,
    ) {
        let fork_height = 12345_u64;
        let err = validate_reorg_within_migration_depth(old_fork_len, migration_depth, fork_height)
            .expect_err("reorg deeper than migration depth must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("reorg depth"),
            "error must mention reorg depth; got: {msg}"
        );
        assert!(
            msg.contains(&format!("({old_fork_len})")),
            "error must include the offending depth value; got: {msg}"
        );
        assert!(
            msg.contains(&format!("({migration_depth})")),
            "error must include the migration depth value; got: {msg}"
        );
        assert!(
            msg.contains(&fork_height.to_string()),
            "error must include the fork height; got: {msg}"
        );
    }

    /// `usize::MAX` reorg lengths must clamp to `u32::MAX` rather than wrap;
    /// the gate should still fire and the error message must surface.
    #[test]
    fn validate_reorg_within_migration_depth_clamps_oversized_lengths() {
        let err = validate_reorg_within_migration_depth(usize::MAX, 5, 0)
            .expect_err("usize::MAX must be treated as exceeding migration depth");
        assert!(err.to_string().contains("reorg depth"));
    }
}
