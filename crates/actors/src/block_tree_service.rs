use crate::{
    StorageModuleServiceMessage,
    block_migration_service::BlockMigrationService,
    block_validation::{
        PreValidationError, build_fork_local_step_window, resolve_header_tree_or_db,
    },
    chunk_ingress_service::ChunkIngressMessage,
    mempool_service::MempoolServiceMessage,
    metrics,
    mining_bus::{BroadcastDifficultyUpdate, BroadcastPartitionsExpiration},
    reth_service::{ForkChoiceUpdateMessage, RethServiceMessage},
    services::ServiceSenders,
    validation_service::ValidationServiceMessage,
};
use eyre::OptionExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::StorageModulesReadGuard;
use irys_domain::{
    BlockState, BlockTree, BlockTreeReadGuard, ChainState, ReorgSplit, ReorgTips,
    block_index_guard::BlockIndexReadGuard, chain_sync_state::ChainSyncState,
    create_commitment_snapshot_for_block, create_epoch_snapshot_for_block,
    forkchoice_markers::ForkChoiceMarkers,
};
use irys_types::{
    BlockHash, Config, DatabaseProvider, H256, H256List, IrysAddress, IrysBlockHeader, SealedBlock,
    SendTraced as _, SystemLedger, TokioServiceHandle, Traced, block_production::Seed,
};
use irys_vdf::{
    ReanchorRequest, first_divergent_boundary, reorg_crossed_divergent_boundary,
    state::{VdfStateReadonly, calc_capacity},
};
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::VecDeque,
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
    SetStorageModulesGuard(StorageModulesReadGuard),
}

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockTreeService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<BlockTreeServiceMessage>>,
    inner: BlockTreeServiceInner,
}

/// Atomic lifecycle timestamps for the block-tree service.
///
/// Exposes two unix-ms timestamps tracking the last canonical advance and the
/// last reorg. Two consumers read these: the OTEL gauges
/// `irys.block_tree.last_block_at_ms` / `irys.block_tree.last_reorg_at_ms`
/// (for dashboards), and the `/v1/tip` HTTP route (in-process, so the
/// endpoint does not have to scrape itself). Update both via
/// [`Self::note_canonical_advance`] / [`Self::note_reorg`].
#[derive(Debug, Default)]
pub struct BlockTreeLifecycleTimestamps {
    last_block_at_ms: std::sync::atomic::AtomicI64,
    last_reorg_at_ms: std::sync::atomic::AtomicI64,
}

impl BlockTreeLifecycleTimestamps {
    pub fn last_block_at_ms(&self) -> i64 {
        self.last_block_at_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn last_reorg_at_ms(&self) -> i64 {
        self.last_reorg_at_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn note_canonical_advance(&self) {
        let now = Self::now_ms();
        self.last_block_at_ms
            .store(now, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::record_block_tree_last_block_at_ms(now);
    }

    pub fn note_reorg(&self) {
        let now = Self::now_ms();
        self.last_reorg_at_ms
            .store(now, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::record_block_tree_last_reorg_at_ms(now);
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
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
    /// Instrumentation only: lets us measure whether gossip-driven recovery
    /// actually works — if a later `Valid` arrives for a block_hash present
    /// here, we count it as a "recovered" discard via the
    /// `soft_internal_recovered_total` counter.
    ///
    /// PHASE-B: if the recovered/discard ratio stays below operational
    /// tolerance (TBD by team), implement explicit per-block-hash re-request
    /// with exponential backoff here.
    ///
    /// Capacity: 4096 entries (~256 KB worst-case). Sized to be safe under
    /// any plausible discard rate without becoming an unbounded memory sink.
    recent_soft_internal_discards: LruCache<BlockHash, &'static str>,
    /// Shared with `ApiState` so `/v1/tip` can expose the same in-process
    /// canonical-advance / reorg timestamps the OTEL gauges record.
    pub lifecycle_timestamps: Arc<BlockTreeLifecycleTimestamps>,
    /// Lock-free VDF view; the re-anchor gate uses the live step so free-run
    /// boundary crossings that tip headers have not yet reached are visible.
    vdf_state: VdfStateReadonly,
    /// Last `(tip hash, live step)` the suspect-watchdog published; cleared when
    /// suspect clears so identical inputs are not re-fired.
    vdf_reanchor_watchdog_last_sent: Option<(BlockHash, u64)>,
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

/// A tip move made by `BlockTreeServiceInner::try_move_tip_to` under the block tree
/// write lock. `publish_tip_move` sends it to the other services after the lock is
/// released, because those sends wait on the other services.
struct TipMove {
    /// The new tip.
    new_tip: Arc<IrysBlockHeader>,
    /// Fork-choice markers of the new canonical chain.
    fcu_markers: ForkChoiceMarkers,
    /// Set when the move abandons blocks of the old canonical chain.
    reorg_event: Option<ReorgEvent>,
    /// The first epoch block among the new canonical blocks.
    epoch_block: Option<Arc<IrysBlockHeader>>,
    /// The new canonical blocks after the fork point, oldest first.
    blocks_to_confirm: Vec<Arc<SealedBlock>>,
}

/// Capacity of [`BlockTreeServiceInner::recent_soft_internal_discards`].
/// 4096 entries × (32-byte hash + ~24 bytes overhead + tag pointer) is well
/// under 256 KB and far exceeds any plausible burst of soft-internal
/// discards in a single block window. See the field doc for full rationale.
const SOFT_INTERNAL_DISCARD_LRU_CAPACITY: usize = 4096;

/// Watchdog cadence for re-anchor retries on a quiet network (matches
/// `BOUNDARY_GATE_WARN_INTERVAL`).
const VDF_REANCHOR_WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Emit the discard log line at the correct level for the given `DiscardKind`.
///
/// SoftInternal is a documented non-fault recovery path (eviction race,
/// payload-cache miss, parent snapshot pruned) — `warn!` keeps alert noise
/// tied to genuine peer rejections (the `Invalid` arm), which stay at
/// `error!`. The structured fields are identical between the two arms so
/// log-aggregation queries see the same shape.
///
/// Extracted from `discard_and_broadcast` so the level split is directly
/// testable via `tracing-test` (see `log_level_split_tests`). A regression
/// that flattens both arms to a single level would break those tests.
fn emit_discard_log(
    kind: DiscardKind,
    block_hash: H256,
    error_display: &str,
    error_log_msg: &'static str,
) {
    match kind {
        DiscardKind::SoftInternal => {
            warn!(block.hash = %block_hash, error = %error_display, "{}", error_log_msg);
        }
        DiscardKind::Invalid => {
            error!(block.hash = %block_hash, error = %error_display, "{}", error_log_msg);
        }
    }
}

/// Resolve the soft-internal `InternalFailure` variant to a bounded-cardinality
/// snake_case reason tag for metrics labelling.
///
/// Reachability invariant: only variants that
/// [`ValidationError::classify`](crate::block_validation::ValidationError::classify)
/// returns `ErrorClass::SoftInternal` for can reach this function.
/// `on_block_validation_finished` panics on node-fault `InternalFailure`
/// (the `is_node_fault()` branch) before calling `discard_and_broadcast`,
/// and routes `Consensus` outcomes to `DiscardKind::Invalid` (skipping the
/// soft-internal recording site in `discard_and_broadcast`). Anything else here is an upstream
/// classification drift and surfaces as a compile error (via the exhaustive
/// match) or a panic (via the `unreachable!` arms below).
///
/// `ValidationCancelled` is additionally gated out of the LRU/counter
/// recording site in `discard_and_broadcast` (cancellations are not
/// gossip-recoverable retries), so its tag is a defensive sentinel that
/// should never actually appear in metrics; if it does, it indicates the
/// gate drifted.
///
/// SAFETY: the match is exhaustive — no `_` wildcard. Adding a new
/// `ValidationError` variant without a label here is a compile error by
/// design. If the new variant is `SoftInternal`, add a real snake_case tag;
/// otherwise add an explicit `unreachable!` arm naming the filtering site.
fn soft_internal_reason_tag(err: &crate::block_validation::ValidationError) -> &'static str {
    use crate::block_validation::ValidationError as VE;
    match err {
        // === SoftInternal variants (per `ValidationError::classify`) ===
        VE::ExecutionPayloadCacheEvicted { .. } => "execution_payload_cache_evicted",
        VE::ParentBlockMissing { .. } => "parent_block_missing",
        VE::ParentCommitmentSnapshotMissing { .. } => "parent_commitment_snapshot_missing",
        VE::ParentEpochSnapshotMissing { .. } => "parent_epoch_snapshot_missing",
        VE::ParentEmaSnapshotMissing { .. } => "parent_ema_snapshot_missing",
        VE::CommitmentDedupLookupFailed(_) => "commitment_dedup_lookup_failed",
        VE::RecallRangeStepsUnavailable(_) => "recall_range_steps_unavailable",
        // PreValidation has a sub-classifier — only its SoftInternal inner
        // variants (`ParentNotInCache`) reach here. We delegate to the
        // inner's `metric_reason()` so each one gets a distinct,
        // grep-stable snake_case tag rather than collapsing to a single
        // "pre_validation" bucket.
        VE::PreValidation(inner) => inner.metric_reason(),
        // Should never be observed in the metric — the discard-site gate
        // in `discard_and_broadcast` skips both the LRU put and the
        // counter increment for any ValidationCancelled variant (cancels
        // are not gossip-recoverable retries). Tag retained as a stable
        // defensive sentinel so a future drift in that gate surfaces here
        // as a distinct label rather than silently mislabelling. Inner
        // reason is intentionally not pattern-matched — the
        // `ValidationCancelled` discriminator is sufficient and
        // inner-reason cardinality is a separate concern.
        VE::ValidationCancelled { .. } => "validation_cancelled",

        // === NodeFault variants — filtered by `is_node_fault()` in
        // `on_block_validation_finished` (panics before `discard_and_broadcast`). ===
        VE::TaskPanicked { .. } => {
            unreachable!(
                "TaskPanicked is NodeFault; on_block_validation_finished panics before discard_and_broadcast"
            )
        }
        VE::ExecutionLayerTransportFailed(_) => {
            unreachable!(
                "ExecutionLayerTransportFailed is NodeFault; on_block_validation_finished panics before discard_and_broadcast"
            )
        }
        VE::ShadowTxNodeFault(_) => {
            unreachable!(
                "ShadowTxNodeFault is NodeFault; on_block_validation_finished panics before discard_and_broadcast"
            )
        }

        // === Consensus variants — routed to `DiscardKind::Invalid` in
        // `on_block_validation_finished`, which skips the soft-internal
        // recording site in `discard_and_broadcast`. ===
        VE::VdfValidationFailed(_)
        | VE::SeedDataInvalid(_)
        | VE::ExecutionLayerFailed(_)
        | VE::RecallRangeInvalid(_)
        | VE::ShadowTransactionInvalid(_)
        | VE::CommitmentValueInvalid { .. }
        | VE::CommitmentVersionInvalid { .. }
        | VE::CommitmentTypeNotAllowed { .. }
        | VE::CommitmentOrderingFailed(_)
        | VE::CommitmentSnapshotRejected { .. }
        | VE::DuplicateCommitmentTransaction { .. }
        | VE::UnpledgePartitionNotOwned { .. }
        | VE::EpochCommitmentMismatch { .. }
        | VE::EpochExtraCommitment { .. }
        | VE::EpochMissingCommitment { .. }
        | VE::CommitmentWrongOrder { .. } => {
            unreachable!(
                "Consensus variant routed to DiscardKind::Invalid at on_block_validation_finished, never reaches soft_internal_reason_tag"
            )
        }
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
        lifecycle_timestamps: Arc<BlockTreeLifecycleTimestamps>,
        vdf_state: VdfStateReadonly,
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
                        lifecycle_timestamps,
                        vdf_state,
                        vdf_reanchor_watchdog_last_sent: None,
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

        // VDF re-anchor watchdog: the suspect buffer's message-driven retry
        // sources all need an inbound canonical block, so on a quiet network a
        // skipped heal would otherwise have no retry at all. Rationale, cure
        // scope, and the never-clears-suspect rule live on
        // `retry_vdf_reanchor_if_suspect`.
        let mut vdf_reanchor_watchdog = tokio::time::interval(VDF_REANCHOR_WATCHDOG_INTERVAL);
        vdf_reanchor_watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for block tree service");
                    break;
                }
                // At most one tick per interval, so despite the biased order
                // this cannot starve the message arm below.
                _ = vdf_reanchor_watchdog.tick() => {
                    self.inner.retry_vdf_reanchor_if_suspect();
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
                msg @ (BlockTreeServiceMessage::GetBlockTreeReadGuard { .. }
                | BlockTreeServiceMessage::BlockPreValidated { .. }
                | BlockTreeServiceMessage::SetStorageModulesGuard(_)) => {
                    self.inner.handle_message(msg, parent_span).await?
                }
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
            BlockTreeServiceMessage::SetStorageModulesGuard(guard) => {
                self.block_migration_service
                    .set_storage_modules_guard(guard);
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

    /// Whether the durable block-stream producer is running (same gate as `/internal/*` routes).
    fn block_stream_enabled(&self) -> bool {
        self.config.node_config.http.expose_internal_api
    }

    /// Migrates finalized blocks into the block index and DB via `BlockMigrationService`.
    fn migrate_block(&mut self, block: &Arc<IrysBlockHeader>) -> eyre::Result<()> {
        let emit_stream = self.block_stream_enabled();
        let block_stream = self.service_senders.block_stream.clone();
        self.block_migration_service.migrate_blocks(
            block,
            emit_stream,
            |_sealed, stream_frame| {
                if let Some(frame) = stream_frame
                    && block_stream.send(Arc::new(frame)).is_err()
                {
                    tracing::debug!(
                        "block-stream producer gone; dropping durable finalized fanout"
                    );
                }
            },
        )?;
        Ok(())
    }

    fn fanout_durable_stream_frames(&self, frames: Vec<irys_types::block_stream::StreamFrame>) {
        if frames.is_empty() {
            return;
        }
        let block_stream = &self.service_senders.block_stream;
        for frame in frames {
            if block_stream.send(Arc::new(frame)).is_err() {
                debug!("block-stream producer gone; dropping durable stream fanout");
                break;
            }
        }
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

        // Release the tree write lock before notifying chunk ingress: the
        // pending-proof drain takes a tree read lock to resolve anchors.
        drop(cache);

        if let Err(e) = self.service_senders.chunk_ingress.send_traced(
            ChunkIngressMessage::ProcessPendingIngressProofs(*block_hash),
        ) {
            debug!(
                block.hash = %block_hash,
                ?e,
                "Failed to send ProcessPendingIngressProofs after prevalidation"
            );
        }

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
            self.discard_and_broadcast(block_hash, validation_result, DiscardKind::SoftInternal)?;
            return self.move_tip_after_discard().await;
        }

        // Handle a failed validation
        if let ValidationResult::Invalid(validation_error) = &validation_result {
            // Symmetric counters: every ValidationError increments exactly
            // one — pre-validation failures go to
            // `irys.block.pre_validation_failed_total`, full-validation
            // failures (VDF / EL / shadow tx / commitment) go to
            // `irys.block.validation_failed_total`. Reason tags share the
            // same enum-driven `metric_reason()` taxonomy. `.err()` pulls
            // the inner `ValidationError` out of the sealed
            // `ConsensusRejectionError` wrapper.
            let inner = validation_error.err();
            let reason = inner.metric_reason();
            if inner.is_pre_validation() {
                crate::metrics::record_block_pre_validation_failed(reason);
            } else {
                crate::metrics::record_block_validation_failed(reason);
            }
            self.discard_and_broadcast(block_hash, validation_result, DiscardKind::Invalid)?;
            return self.move_tip_after_discard().await;
        }

        // From here, we are processing a fully validated block.
        //
        // Confirm cache presence BEFORE touching the soft-internal recovery
        // LRU. A spurious `Valid` arrival for a hash no longer in the cache
        // (eviction race with `remove_block`, duplicate gossip racing
        // classification, or a soft-internal-parked block whose entry was
        // just removed by another path) must not pop the LRU entry — doing
        // so would (a) lose the recovery marker that a legitimate fresh
        // re-delivery would have matched and (b) overcount this spurious
        // path as a recovery on the metric.
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
            // Emit BlockStateUpdated even on cache-miss Valid (symmetric with
            // the cache-miss discard path) so downstream waiters
            // (read_block_from_state, wait_for_parent_validation) don't hang
            // on a cache-miss Valid arrival. height = 0 and
            // state = NotOnchain(Unknown) are sentinel values; the block is
            // gone from the cache so we have no better information.
            let event = BlockStateUpdated {
                block_hash,
                height: 0,
                state: ChainState::NotOnchain(BlockState::Unknown),
                discarded: false,
                validation_result: ValidationResult::Valid,
            };
            if let Err(e) = self.service_senders.block_state_events.send(event) {
                tracing::trace!(
                    block.hash = ?block_hash,
                    "Failed to broadcast block state update event on cache-miss Valid: {}", e
                );
            }
            return Ok(());
        };

        // Soft-internal-recovery instrumentation: pop + metric are deferred to
        // the two confirmed-success boundaries below (after mark_block_as_valid
        // + second cache check) so that early-returns from either failure path do
        // not pop the LRU or increment the recovery counter spuriously.
        debug!(
            "On validation complete: result {} {:?} at height: {}",
            block_hash, validation_result, height
        );

        let (tip_move, state) = {
            let binding = self.cache.clone();
            let mut cache = binding.write().map_err(|_| {
                eyre::eyre!("block tree cache write lock poisoned in on_block_validation_finished")
            })?;

            // Get the current tip before any changes. The canonical chain follows
            // `cache.tip`, which only moves after full validation (in `mark_tip` below).
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

            let Some(_) = cache.get_earliest_not_onchain_in_longest_chain() else {
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

                // Confirmed-success boundary 1 of 2: mark_block_as_valid
                // succeeded above; this is a confirmed valid arrival, so pop
                // the recovery LRU and record the metric now (not before the
                // write lock, where the result is still unknown).
                if let Some(reason) = self.recent_soft_internal_discards.pop(&block_hash) {
                    metrics::record_soft_internal_recovered(reason);
                    info!(
                        block.hash = %block_hash,
                        reason,
                        "block previously discarded as soft-internal reached Valid via gossip re-delivery"
                    );
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
                return Ok(());
            };

            // Get block info before mutable operations.
            // We just observed the block in cache when computing `height` above (under
            // the read lock), and mark_block_as_valid succeeded under this write lock.
            // A missing entry means a concurrent prune raced these two locks — drop
            // this stale validation result, mirroring the height-lookup `None` path
            // earlier in the function rather than tearing down the service.
            let Some(_) = cache.blocks.get(&block_hash) else {
                warn!(
                    block.hash = %block_hash,
                    block.height = height,
                    "Ignoring validation result for block already pruned from cache"
                );
                // Concurrent-prune race (symmetric with the cache-miss Valid
                // path above and the cache-miss discard path): a concurrent
                // prune raced the write lock, but mark_block_as_valid already
                // succeeded — emit BlockStateUpdated so downstream waiters
                // don't hang. height is known; state falls back to
                // NotOnchain(Unknown) because the entry is gone.
                //
                // NOTE: We do NOT pop the recovery LRU here. A concurrent prune is not
                // a confirmed gossip-driven recovery — the block never reached a stable
                // onchain state. Leaving the LRU entry intact means a later re-delivery
                // can still be counted correctly.
                //
                // DEFERRED TEST: directly reproducing this concurrent-prune race in a
                // unit test requires a test seam (e.g. a post-mark_block_as_valid hook
                // that removes the entry before the second get) that the current harness
                // doesn't support. The correct shape is verified by code review and the
                // cache-miss-Valid + discard-path tests that exercise the analogous
                // emit-before-return pattern.
                drop(cache);
                let event = BlockStateUpdated {
                    block_hash,
                    height,
                    state: ChainState::NotOnchain(BlockState::Unknown),
                    discarded: false,
                    validation_result: ValidationResult::Valid,
                };
                if let Err(e) = self.service_senders.block_state_events.send(event) {
                    tracing::trace!(
                        block.hash = ?block_hash,
                        block.height = height,
                        "Failed to broadcast block state update event on concurrent-prune: {}", e
                    );
                }
                return Ok(());
            };
            let tip_move = self.try_move_tip_to(&mut cache, block_hash)?;

            let state = cache
                .get_block_and_status(&block_hash)
                .map(|(_, state)| *state)
                .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

            (tip_move, state)
        }; // RwLockWriteGuard is dropped here, before the await

        match tip_move {
            Some(tip_move) => self.publish_tip_move(tip_move).await?,
            None => {
                // Metadata is persisted for every validated block, also when the tip
                // does not move: then there is no block to clear or confirm.
                use crate::block_migration_service::StreamEmission;
                let stream = if self.block_stream_enabled() {
                    StreamEmission::Observed
                } else {
                    StreamEmission::Disabled
                };
                self.block_migration_service
                    .persist_metadata(&[], &[], stream)?;
            }
        }

        // Confirmed-success boundary 2 of 2: mark_block_as_valid succeeded
        // and the second cache check passed — this is a confirmed valid arrival
        // that reached a stable onchain state. Pop the recovery LRU here.
        if let Some(reason) = self.recent_soft_internal_discards.pop(&block_hash) {
            metrics::record_soft_internal_recovered(reason);
            info!(
                block.hash = %block_hash,
                reason,
                "block previously discarded as soft-internal reached Valid via gossip re-delivery"
            );
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

    /// After a discard, move the tip to the max cumulative difficulty block when it is
    /// heavier than the tip and every block from it back to its first `Onchain` ancestor
    /// is fully validated.
    ///
    /// A validated block that does not become the tip (for example, `mark_tip` refused a
    /// cumulative difficulty tie with the max block) waits for a later tip change. When
    /// the discard removes the max block, such a block can become the max block, and no
    /// validation result for it follows. Without this step the tip stays on a lighter
    /// block, the max block cannot be built upon, and block production waits.
    async fn move_tip_after_discard(&mut self) -> eyre::Result<()> {
        let tip_move = {
            let binding = self.cache.clone();
            let mut cache = binding.write().map_err(|_| {
                eyre::eyre!("block tree cache write lock poisoned in move_tip_after_discard")
            })?;

            // The gate that a validated block passes in `on_block_validation_finished`:
            // the max block is heavier than the tip and its fork attaches to the tree.
            if cache.get_earliest_not_onchain_in_longest_chain().is_none() {
                return Ok(());
            }
            // `mark_tip` sets the ancestry of the new tip `Onchain`, so every block of
            // that ancestry must be fully validated.
            let (_, max_hash) = cache.get_max_cumulative_difficulty_block();
            if cache.get_max_chain_validated_head() != Some(max_hash) {
                return Ok(());
            }
            self.try_move_tip_to(&mut cache, max_hash)?
        }; // RwLockWriteGuard is dropped here, before the await

        if let Some(tip_move) = tip_move {
            self.publish_tip_move(tip_move).await?;
        }
        Ok(())
    }

    /// Move the tip to `candidate`, a validated block, when it is heavier than the tip.
    /// The caller holds the block tree write lock across this call, so the tip change,
    /// the reorg classification and the prune see one tree state. Returns `None` when the
    /// tip does not move. `publish_tip_move` sends a returned move to the other services
    /// after the caller releases the lock.
    fn try_move_tip_to(
        &self,
        cache: &mut BlockTree,
        candidate: BlockHash,
    ) -> eyre::Result<Option<TipMove>> {
        let new_tip = cache
            .blocks
            .get(&candidate)
            .map(|entry| Arc::clone(entry.block.header()))
            .ok_or_else(|| eyre::eyre!("tip candidate {candidate} not found in cache"))?;
        let old_tip_block = cache
            .get_block(&cache.tip)
            .ok_or_eyre("tip block must always be present")?;
        let old_tip_hash = old_tip_block.block_hash;
        let old_tip_height = old_tip_block.height;

        // only mark the tip if the new tip has higher cumulative difficulty than the old one
        if old_tip_block.cumulative_diff >= new_tip.cumulative_diff {
            // this also means that the tip can point to a block in a chain that is not
            // the canonical one (aka which the self.max_cumulative_difficulty is pointing at).
            // That is valid because the blocks below self.max_cumulative_difficulty
            // could still be undergoing validation, which is not guaranteed to succeed
            debug!(
                block.hash = %candidate,
                block.height = new_tip.height,
                old_tip.hash = %old_tip_hash,
                old_tip.height = old_tip_height,
                "block_tree.transition: validated as Fork (kept current tip)"
            );
            return Ok(None);
        }
        let changed = cache.mark_tip(&candidate)?;
        debug!(
            block.hash = %candidate,
            block.height = new_tip.height,
            old_tip.hash = %old_tip_hash,
            old_tip.height = old_tip_height,
            tip_changed = changed,
            "block_tree.transition: mark_tip"
        );
        if !changed {
            return Ok(None);
        }

        let block_index_read = self.block_index_guard.read();
        let fcu_markers = ForkChoiceMarkers::from_block_tree(
            cache,
            block_index_read,
            &self.db,
            self.config.consensus.block_migration_depth as usize,
            self.config.consensus.block_tree_depth as usize,
        )?;

        // Compute the reorg split BEFORE pruning so the parent-walk
        // sees both lineages in `cache.blocks`. Independent of
        // `ChainState` flags by construction.
        //
        // The tip change is a reorg only when it abandons blocks of the old
        // canonical chain. Classify from the old tip -> new tip transition, not
        // from the max-difficulty chain: that chain can be a fork still waiting
        // for validation, unrelated to the block that just became the tip.
        let split: ReorgSplit = cache.find_reorg_split(ReorgTips {
            new_tip: candidate,
            old_tip: old_tip_hash,
        })?;
        let is_reorg = !split.old_divergent.is_empty();

        // A reorg whose fork point predates the post-prune cache window
        // cannot complete safely. `cache.prune()` below drops every block
        // below `new_tip.height - (block_tree_depth - 1)`; once the LCA is
        // in that range, `BlockTree::prune`'s recursive child removal
        // cascades the whole old fork (and the new-fork blocks below the
        // window) out of the cache, evicting their epoch and commitment
        // snapshots. Downstream reorg consumers re-read those by hash
        // (`send_epoch_events` -> `get_epoch_snapshot`,
        // `handle_confirmed_commitment_tx_reorg` -> `get_commitment_snapshot`)
        // and would fail mid-reorg, after the event was broadcast and
        // metadata persisted.
        //
        // Abort before prune mutates the cache, so the failure is pre-commit
        // with an accurate diagnostic. Matches master, whose post-prune
        // `prune_chains_at_ancestor` errored whenever the LCA had been
        // evicted from the canonical chain. The trigger — a new fork at
        // least `block_tree_depth` long diverging inside the window — is
        // pathological (attack / severe partition); halting is correct.
        // Returning `Err` drives the controlled-shutdown path.
        //
        // A tip extension that spans the window fails the same way: prune
        // evicts its oldest new blocks before they are confirmed, and the
        // epoch snapshot and migration reads that follow fail by hash.
        let fork_height = split.lca.header().height;
        if let Err(e) = validate_reorg_within_cache_window(
            fork_height,
            new_tip.height,
            self.config.consensus.block_tree_depth,
        ) {
            error!(
                fork_height,
                is_reorg,
                new_tip = %candidate,
                new_tip_height = new_tip.height,
                new_block_count = split.new_divergent.len(),
                block_tree_depth = self.config.consensus.block_tree_depth,
                "tip change fork point is older than the block-tree cache window; aborting before prune evicts divergent blocks (and their epoch/commitment snapshots) that downstream consumers re-read, to trigger controlled shutdown",
            );
            return Err(e);
        }

        // Prune the cache after the LCA is captured.
        //
        // Subtract 1 to ensure we keep exactly `depth` blocks.
        // The cache.prune() implementation does not count `tip` into the depth
        // equation, so it's always tip + `depth` that's kept around
        cache.prune(self.config.consensus.block_tree_depth.saturating_sub(1));

        let (epoch_block, reorg_event, blocks_to_confirm) = if is_reorg {
            // Handle blockchain reorganization
            let fork_block: Arc<IrysBlockHeader> = split.lca.header().clone();
            let fork_hash: BlockHash = fork_block.block_hash;
            let fork_height: u64 = fork_block.height;
            let old_fork_blocks: Vec<Arc<SealedBlock>> = split.old_divergent;
            let new_fork_blocks: Vec<Arc<SealedBlock>> = split.new_divergent;
            let blocks_to_confirm = new_fork_blocks.clone();

            debug!("fork_height: {} fork_hash: {}", fork_height, fork_hash);

            debug!(
                "Reorg at block height {} with {}, old fork is {} blocks long, new one is {} blocks",
                new_tip.height,
                new_tip.block_hash,
                old_fork_blocks.len(),
                new_fork_blocks.len()
            );

            // Migration for the current tick hasn't run yet (it happens after
            // this reorg path, in migrate_block()). The highest migrated block is
            // at old_tip - migration_depth, from the previous tick's migration.
            // old_fork_blocks excludes the fork point (common ancestor), so a
            // migrated block is orphaned only when the fork is strictly deeper
            // than migration_depth.
            if old_fork_blocks.len() as u32 > self.config.consensus.block_migration_depth {
                error!(
                    fork_depth = old_fork_blocks.len(),
                    new_fork_depth = new_fork_blocks.len(),
                    fork_height,
                    current_height = new_tip.height,
                    "NETWORK PARTITION RECOVERY: this node was isolated from the network \
                     and fell behind the canonical chain. Deep reorg rolling back {} \
                     migrated blocks. Investigate peer connectivity — check firewall \
                     rules, network routes, and gossip health between this node and \
                     its peers.",
                    old_fork_blocks.len() as u32 - self.config.consensus.block_migration_depth,
                );
                // Roll back blocks migrated on the minority fork before
                // proceeding with the new canonical chain.
                self.block_migration_service
                    .recover_from_network_partition(fork_height)?;
            }

            metrics::record_reorg();
            metrics::record_reorg_depth(u64::try_from(old_fork_blocks.len()).unwrap_or(0));
            self.lifecycle_timestamps.note_reorg();

            // Create reorg event with all necessary data for downstream processing
            let event = ReorgEvent {
                old_fork: Arc::new(old_fork_blocks),
                new_fork: Arc::new(new_fork_blocks),
                fork_parent: fork_block,
                new_tip: candidate,
                timestamp: SystemTime::now(),
                db: Some(self.db.clone()),
            };

            // Was there a new epoch block found in the reorg
            let new_epoch_block = event
                .new_fork
                .iter()
                .find(|sb| self.is_epoch_block(sb.header()))
                .map(|sb| Arc::clone(sb.header()));

            (new_epoch_block, Some(event), blocks_to_confirm)
        } else {
            // Handle normal chain extension
            debug!(
                "Extending longest chain to height {} with {} parent: {} height: {}",
                new_tip.height, new_tip.block_hash, old_tip_hash, old_tip_height
            );

            // The tip can move by more than one block: a validated block that
            // did not become the tip (for example, `mark_tip` refused a cumulative
            // difficulty tie) joins the canonical chain with its child. The new
            // side of the split holds each block after the old tip up to the new
            // tip, oldest first. Confirm each one and detect an epoch block among
            // them, as on a reorg.
            let blocks_to_confirm: Vec<Arc<SealedBlock>> = split.new_divergent;
            let new_epoch_block = blocks_to_confirm
                .iter()
                .find(|sb| self.is_epoch_block(sb.header()))
                .map(|sb| Arc::clone(sb.header()));

            (new_epoch_block, None, blocks_to_confirm)
        };

        Ok(Some(TipMove {
            new_tip,
            fcu_markers,
            reorg_event,
            epoch_block,
            blocks_to_confirm,
        }))
    }

    /// Send a tip move made by `try_move_tip_to` to the other services, after the block
    /// tree write lock is released: epoch events, metadata, the reorg event, the
    /// fork-choice update, mempool confirmations, block migration and the difficulty
    /// update.
    async fn publish_tip_move(&mut self, tip_move: TipMove) -> eyre::Result<()> {
        let TipMove {
            new_tip,
            fcu_markers,
            reorg_event,
            epoch_block,
            blocks_to_confirm,
        } = tip_move;

        // Send epoch events which require a Read lock
        if let Some(epoch_block) = epoch_block {
            // Send the epoch events
            self.send_epoch_events(&epoch_block)?;
        }

        // Persist metadata atomically (same code path for both normal blocks and reorgs);
        // durable observed/reorged stream frames land in this same RW txn.
        let stream_frames = {
            use crate::block_migration_service::{StreamEmission, StreamReorgAppend};
            let old_fork: &[Arc<SealedBlock>] = reorg_event
                .as_ref()
                .map_or(&[] as &[_], |e| e.old_fork.as_ref());
            let stream_reorg = reorg_event.as_ref().map(|e| StreamReorgAppend {
                fork_parent: Arc::clone(&e.fork_parent),
                old_fork: Arc::clone(&e.old_fork),
                new_fork: Arc::clone(&e.new_fork),
            });
            let stream = if !self.block_stream_enabled() {
                StreamEmission::Disabled
            } else if let Some(reorg) = stream_reorg.as_ref() {
                StreamEmission::Reorged(reorg)
            } else {
                StreamEmission::Observed
            };
            self.block_migration_service
                .persist_metadata(old_fork, &blocks_to_confirm, stream)?
        };

        // Broadcast reorg event if applicable
        let reorg_occurred = reorg_event.is_some();
        if let Some(reorg_event) = reorg_event {
            // Deep reorg may poison VDF seeds; re-anchor before reorg fans out.
            self.maybe_reanchor_vdf_after_reorg(&new_tip, &reorg_event);

            if let Err(e) = self.service_senders.reorg_events.send(reorg_event) {
                error!(
                    "Failed to broadcast reorg event - mempool state may be stale: {:?}",
                    e
                );
            }
        }

        // Per-block hot path: emit at debug to avoid flooding info-level
        // logs (~7k entries/day at 12s blocks). Operators monitoring tip
        // progress should consume the canonical_tip_height metric below
        // instead.
        debug!(
            block.height = new_tip.height,
            block.hash = ?new_tip.block_hash,
            block.timestamp_ms = new_tip.timestamp.as_millis(),
            "New canonical tip",
        );
        metrics::record_canonical_tip_height(new_tip.height);
        self.lifecycle_timestamps.note_canonical_advance();

        // Suspect buffer with no reorg on this tip: re-send re-anchor.
        if !reorg_occurred
            && self
                .service_senders
                .vdf_reanchor_signals
                .is_buffer_suspect()
        {
            self.send_reanchor_request(&new_tip);
        }

        // Emit consensus events
        self.emit_fcu(&fcu_markers).await?;

        // Live stream fan-out only after FCU succeeds, so a follower reacting to `observed`
        // never races the execution-layer head update (the pre-change ordering). The frames
        // are already durable; on an FCU error the follower recovers them via replay.
        self.fanout_durable_stream_frames(stream_frames);

        // Mempool confirmations. The mempool may already be shutting down (its receiver
        // dropped); log and continue — block confirmation itself is still valid.
        // Stream `observed`/`reorged` frames were durable-appended inside `persist_metadata`;
        // migration appends `finalized` the same way.
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

        self.migrate_block(&fcu_markers.migration_block)?;

        // Broadcast difficulty update to miners if tip difficulty changed from parent.
        // On either lock poisoning or missing parent (the latter is a strong invariant
        // — the tip just changed so the parent must be present — but a panic here
        // would take the node down for a non-critical broadcast), skip the diff check
        // and log instead. The downstream miners will pick up the difficulty on the
        // next canonical update.
        let parent_diff_changed = match self.cache.read() {
            Ok(cache) => match cache.get_block(&new_tip.previous_block_hash) {
                Some(parent_block) => parent_block.diff != new_tip.diff,
                None => {
                    error!(
                        parent_hash = %new_tip.previous_block_hash,
                        block.hash = %new_tip.block_hash,
                        "parent block missing from cache while broadcasting difficulty update; skipping"
                    );
                    false
                }
            },
            Err(_) => {
                error!("block tree cache read lock poisoned during difficulty broadcast; skipping");
                false
            }
        };
        if parent_diff_changed {
            self.service_senders
                .send_mining_difficulty(BroadcastDifficultyUpdate(new_tip.clone()));
        }

        Ok(())
    }

    /// Queue a VDF seed-buffer re-anchor after a deep reorg that may have
    /// crossed a divergent reset boundary. See
    /// `design/docs/vdf-reset-seed-confirmation-gate.md`.
    fn maybe_reanchor_vdf_after_reorg(&self, canonical_tip: &IrysBlockHeader, reorg: &ReorgEvent) {
        // Shallow reorgs stay inside the confirmation gate's safe window.
        let migration_depth = usize::try_from(self.config.consensus.block_migration_depth)
            .expect("block_migration_depth must fit in usize");
        if reorg.old_fork.len() <= migration_depth {
            return;
        }

        let reset_frequency = u64::try_from(self.config.consensus.vdf.reset_frequency)
            .expect("VDF reset_frequency must fit in u64");
        if reset_frequency == 0 {
            return;
        }
        let c_step = canonical_tip.vdf_limiter_info.global_step_number;
        let lca_step = reorg.fork_parent.vdf_limiter_info.global_step_number;

        // Max of tip, old-fork tip, and live VDF step — free-run can cross a
        // boundary before either tip header has.
        let old_fork_tip_step = reorg.old_fork.last().map_or(c_step, |block| {
            block.header().vdf_limiter_info.global_step_number
        });
        let live_step = self.vdf_state.current_step();
        let crossing_step = c_step.max(old_fork_tip_step).max(live_step);

        if !reorg_crossed_divergent_boundary(lca_step, crossing_step, reset_frequency) {
            return;
        }

        info!(
            c_step,
            lca_step,
            live_step,
            first_divergent_boundary = first_divergent_boundary(lca_step, reset_frequency),
            "Deep reorg crossed a divergent VDF reset boundary; queuing a seed-buffer re-anchor"
        );
        // Mark suspect before publish so validation never trusts the buffer
        // after a heal could clear first (record_heal_applied clears suspect).
        self.service_senders
            .vdf_reanchor_signals
            .mark_buffer_suspect();
        if !self.send_reanchor_request(canonical_tip) {
            warn!(
                c_step,
                "VDF re-anchor not queued after marking buffer suspect; \
                 tip-advance path will retry while suspect remains set"
            );
        }
    }

    /// Publish a [`ReanchorRequest`] for `canonical_tip` (also used by suspect retries).
    /// Returns `true` if the request was queued.
    fn send_reanchor_request(&self, canonical_tip: &IrysBlockHeader) -> bool {
        let c_step = canonical_tip.vdf_limiter_info.global_step_number;

        // Full capacity window (not reorg depth): reanchor_seeds replaces the
        // whole buffer; tree then DB for ancestry past the in-memory tail.
        let capacity = u64::try_from(calc_capacity(&self.config)).expect("capacity fits in u64");
        let want_start = c_step.saturating_sub(capacity.saturating_sub(1)).max(1);
        let block_tree_guard = BlockTreeReadGuard::new(Arc::clone(&self.cache));
        let resolve = |hash: &H256| resolve_header_tree_or_db(&block_tree_guard, &self.db, hash);
        let Some(window) = build_fork_local_step_window(canonical_tip, want_start, c_step, resolve)
        else {
            warn!(
                c_step,
                want_start,
                "VDF re-anchor skipped: canonical seed window unavailable (ancestry below DB)"
            );
            return false;
        };

        let canonical_seeds: VecDeque<Seed> = window.0.into_iter().map(Seed).collect();
        let request = ReanchorRequest {
            canonical_seeds,
            c_step,
            seed: canonical_tip.vdf_limiter_info.seed,
            next_seed: canonical_tip.vdf_limiter_info.next_seed,
        };
        if self.service_senders.vdf_reanchor.try_send(request) {
            info!(c_step, "Queued a VDF seed-buffer re-anchor request");
            true
        } else {
            // Latest-value channel: fails only when the VDF receiver is gone.
            warn!(
                c_step,
                "VDF re-anchor channel closed (VDF thread not running); heal not queued"
            );
            false
        }
    }

    /// While the buffer is suspect, re-send a re-anchor if tip/live-step inputs
    /// changed (quiet-network backstop; never clears suspect itself).
    fn retry_vdf_reanchor_if_suspect(&mut self) {
        if !self
            .service_senders
            .vdf_reanchor_signals
            .is_buffer_suspect()
        {
            self.vdf_reanchor_watchdog_last_sent = None;
            return;
        }

        // Drop the cache guard before send_reanchor_request takes its own reads.
        let canonical_tip = match self.cache.read() {
            Ok(cache) => cache.get_block(&cache.tip).cloned(),
            Err(_) => {
                error!(
                    "block tree cache read lock poisoned in the VDF re-anchor watchdog; skipping tick"
                );
                return;
            }
        };
        let Some(canonical_tip) = canonical_tip else {
            error!("canonical tip missing from cache in the VDF re-anchor watchdog; skipping tick");
            return;
        };

        let inputs = (canonical_tip.block_hash, self.vdf_state.current_step());
        if self.vdf_reanchor_watchdog_last_sent == Some(inputs) {
            debug!(
                c_step = canonical_tip.vdf_limiter_info.global_step_number,
                live_step = inputs.1,
                "VDF buffer still suspect; watchdog inputs unchanged, awaiting fresher state"
            );
            return;
        }

        warn!(
            c_step = canonical_tip.vdf_limiter_info.global_step_number,
            live_step = inputs.1,
            "VDF buffer suspect with no message-driven retry; watchdog re-firing the re-anchor request"
        );
        metrics::record_vdf_reanchor_watchdog_retry();
        if self.send_reanchor_request(&canonical_tip) {
            self.vdf_reanchor_watchdog_last_sent = Some(inputs);
        }
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

        // SoftInternal is a documented non-fault recovery path — warn-level
        // keeps alerting tied to genuine peer rejections (Invalid arm).
        emit_discard_log(kind, block_hash, &error_display, error_log_msg);
        self.chain_sync_state
            .record_block_validation_error(diagnostic_record);

        // Soft-internal-recovery instrumentation: on a SoftInternal discard,
        // derive the reason tag from the wrapped `ValidationError`, increment
        // the discard counter, and remember (block_hash -> reason) in the
        // bounded LRU so a later Valid result can be counted as a recovery.
        //
        // PHASE-B: if the ratio of `soft_internal_recovered_total` to
        // `soft_internal_discard_total` stays below operational tolerance
        // (TBD by team), implement explicit per-block-hash re-request with
        // exponential backoff here.
        //
        // `ValidationCancelled` reaches the SoftInternal arm (every cancel
        // reason is `is_internal() = true`), but cancellations are NOT
        // gossip-recoverable failures — the block's validation outcome is
        // unknown ("we moved on"), not "failed and needing retry". Recording
        // them in the recovery LRU would inflate `soft_internal_discard_total`
        // with local "we moved on" events and the recovery counter would never
        // match (fresh gossip rarely re-triggers the same cancellation race
        // for the same hash).
        let mut cache = self.cache.write().map_err(|_| {
            eyre::eyre!("block tree cache write lock poisoned in discard_and_broadcast")
        })?;

        let maybe_height = cache.get_block(&block_hash).map(|x| x.height);
        let height = maybe_height.unwrap_or(0);
        let state = cache
            .get_block_and_status(&block_hash)
            .map(|(_, state)| *state)
            .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

        // Track whether we actually performed a cache write. Three call
        // shapes reach this helper:
        //   1. Normal flow (block came through block_tree): cache hit,
        //      `remove_block` succeeds → cache_write_succeeded = true.
        //   2. Cache-miss: recursive sweep / eviction race / duplicate
        //      dispatch already removed it, OR the block was submitted
        //      directly to `validation_service` (e.g.
        //      `send_block_to_block_validation` in chain-tests) and was
        //      never in block_tree at all → cache_write_succeeded = false.
        //   3. Cache-hit but `remove_block` Err: cache invariant
        //      corruption → cache_write_succeeded = false.
        //   4. Cache-hit on an `Onchain` block: `remove_block` refuses it
        //      and this helper returns the error (see the match arm).
        //
        // Side effects split on this flag:
        //   - `BlockStateUpdated` broadcast: ALWAYS emitted. Downstream
        //     test infrastructure and `block_validation_tracker` treat
        //     the event as a validation-finalization signal independent
        //     of cache state, so suppressing it on a direct-submission
        //     path would break tests like
        //     `heavy_block_validation_discards_a_block_if_its_too_old`
        //     that exercise the validation pipeline without going through
        //     block_tree.
        //   - Soft-internal-recovery metric + LRU put: ONLY when
        //     cache_write_succeeded. A spurious LRU entry for a hash we
        //     didn't actually remove would be popped by a later cache-hit
        //     Valid arrival, spuriously ticking
        //     `soft_internal_recovered_total`. The metric must mirror the
        //     LRU since both live in the same gate.
        let cache_write_succeeded = if maybe_height.is_some() {
            match cache.remove_block(&block_hash, irys_domain::RemovalReason::ValidationInvalid) {
                Ok(()) => true,
                // An `Onchain` block is the tip or an ancestor of it. The tip only moves
                // to a validated block, so this node already accepted this block, and a
                // result that is not `Valid` contradicts that. Removing the block would
                // empty the canonical chain; keeping it and continuing would hide a node
                // fault. Return the error: the service stops on the controlled-shutdown
                // path, with the tree unchanged and no broadcast.
                Err(err) if state == ChainState::Onchain => {
                    error!(
                        block.hash = %block_hash,
                        block.height = height,
                        ?err,
                        "validation result that is not Valid for an Onchain block; the block stays in the tree and the service stops"
                    );
                    return Err(
                        err.wrap_err(format!("discard of Onchain block {block_hash} refused"))
                    );
                }
                Err(err) => {
                    tracing::error!(block.hash = %block_hash, ?err, "Failed to remove block from cache");
                    false
                }
            }
        } else {
            tracing::debug!(block.hash = %block_hash, "Block already removed from cache (or never inserted; e.g. direct validation_service submission)");
            false
        };
        drop(cache);

        // Record the soft-internal LRU entry *after* the cache write
        // completes, AND only if it actually succeeded. If the lock is
        // poisoned, the block was never in the cache, or the removal
        // returned Err, no LRU entry is left dangling — otherwise a
        // later Valid for the same hash would spuriously tick
        // `soft_internal_recovered_total`.
        if cache_write_succeeded
            && matches!(kind, DiscardKind::SoftInternal)
            && let ValidationResult::InternalFailure(inner) = &validation_result
            && !matches!(
                inner.err(),
                crate::block_validation::ValidationError::ValidationCancelled { .. }
            )
        {
            let reason = soft_internal_reason_tag(inner.err());
            metrics::record_soft_internal_discard(reason);
            // Note: the LRU + metric are set only for `block_hash`, not for
            // children that `remove_block` sweeps recursively. Children were
            // never independently classified as SoftInternal — they're
            // co-discarded with the parent — so a later Valid arrival for a
            // child counts as fresh delivery rather than recovery. Intentional.
            self.recent_soft_internal_discards.put(block_hash, reason);
        }

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
                .send_partitions_expiration(BroadcastPartitionsExpiration {
                    partition_hashes: H256List(expired_partition_hashes),
                    update_height: epoch_block.height,
                });

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
        let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments()?;
        self.service_senders.storage_modules.send_traced(
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: storage_module_infos.into(),
                update_height: epoch_block.height,
            },
        )?;
        Ok(())
    }
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

/// Reject reorgs whose fork point would be evicted by the cache prune that runs
/// immediately after split computation.
///
/// `BlockTree::prune` keeps blocks at or above
/// `new_tip_height - (block_tree_depth - 1)`. Once the fork point (LCA) drops
/// below that, prune's recursive child removal cascades the divergent blocks —
/// and the epoch/commitment snapshots keyed by their hashes — out of the cache,
/// leaving downstream reorg consumers re-reading evicted blocks. Aborting here
/// keeps that failure pre-commit (before the reorg event is broadcast and
/// metadata persisted).
///
/// Fires only when the new fork is at least `block_tree_depth` blocks long yet
/// diverges inside the cache window — attack / severe-partition territory, the
/// same class master halted on when the LCA fell outside the post-prune
/// canonical chain.
pub fn validate_reorg_within_cache_window(
    fork_height: u64,
    new_tip_height: u64,
    block_tree_depth: u64,
) -> eyre::Result<()> {
    let min_keep_height = new_tip_height.saturating_sub(block_tree_depth.saturating_sub(1));
    if fork_height < min_keep_height {
        return Err(eyre::eyre!(
            "reorg deeper than cache window: fork point at height {fork_height} \
             is below the post-prune minimum keep height {min_keep_height} \
             (new tip height {new_tip_height}, block_tree_depth {block_tree_depth})"
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
    use std::sync::Mutex;

    /// A consensus-rejection error must convert to `ValidationResult::Invalid`.
    #[test]
    fn consensus_preval_error_converts_to_invalid() {
        let result: ValidationResult = PreValidationError::BlockSignatureInvalid.into();
        assert!(matches!(result, ValidationResult::Invalid(_)));
        assert_eq!(result.metric_label(), "invalid");
    }

    /// The seeds stage records its label through `granular_metric_label`. This
    /// drives a real `irys_vdf::verify::is_seed_data_valid` failure and maps its
    /// `eyre` error exactly as `block_validation_task` does, pinning that a
    /// `SeedDataInvalid` rejection surfaces as `"invalid"` (not `"seed_data_invalid"`,
    /// which is `metric_reason`).
    #[test]
    fn seed_data_invalid_dispatches_to_invalid_with_label() {
        use irys_types::{BlockHash, H256, H256List, IrysBlockHeader};
        let reset_frequency = 2;
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.block_hash = BlockHash::from_slice(&[4; 32]);
        parent.vdf_limiter_info.seed = BlockHash::from_slice(&[2; 32]);
        parent.vdf_limiter_info.next_seed = BlockHash::from_slice(&[3; 32]);

        let mut block = IrysBlockHeader::new_mock_header();
        block.vdf_limiter_info.global_step_number = 3;
        block.vdf_limiter_info.steps = H256List(vec![H256::zero(); 2]);
        block.vdf_limiter_info.set_seeds(reset_frequency, &parent);
        // Corrupt the embedded seeds so verification fails.
        block.vdf_limiter_info.seed = BlockHash::from_slice(&[5; 32]);
        block.vdf_limiter_info.next_seed = BlockHash::from_slice(&[6; 32]);

        let err = irys_vdf::verify::is_seed_data_valid(&block, &parent, reset_frequency)
            .expect_err("corrupted seeds must fail verification");
        // Mirror the block_validation_task seeds-stage mapping of the eyre error.
        let result: ValidationResult =
            crate::block_validation::ValidationError::SeedDataInvalid(err.to_string()).into();
        assert!(matches!(result, ValidationResult::Invalid(_)));
        assert_eq!(result.granular_metric_label(), "invalid");
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

    /// Every `ValidationCancelled` sub-reason routes to `InternalFailure`.
    /// `HeightDifference` and `ChannelClosed` were historically `Invalid`
    /// under the older "we moved on" rationale, but neither reflects the
    /// peer's block — both are local-side events. They now route through
    /// `is_internal() = true` → `InternalFailure` alongside `ParentMissing`.
    /// Discard still happens in the block_tree service handler; the block
    /// is just no longer peer-attributed.
    #[test]
    fn validation_cancelled_converts_per_reason() {
        use crate::block_validation::ValidationCancelReason;
        for reason in [
            ValidationCancelReason::HeightDifference,
            ValidationCancelReason::ChannelClosed,
            ValidationCancelReason::ParentMissing,
            ValidationCancelReason::RepeatedCancellation,
            ValidationCancelReason::PoAAborted,
        ] {
            let result: ValidationResult =
                crate::block_validation::ValidationError::ValidationCancelled { reason }.into();
            assert!(
                matches!(result, ValidationResult::InternalFailure(_)),
                "reason {:?} should dispatch to InternalFailure",
                reason
            );
            // Per-stage label collapses cancellations to "cancelled"; the
            // overall `ValidationResult::metric_label` returns the underlying
            // bucket ("internal_error" for `InternalFailure`).
            assert_eq!(result.metric_label(), "internal_error");
        }
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
    #[case::recall_range_steps_unavailable(
        crate::block_validation::ValidationError::RecallRangeStepsUnavailable(
            "steps unavailable".into(),
        ),
        "recall_range_steps_unavailable",
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
        "validation_cancelled",
    )]
    #[case::cancel_height_difference(
        crate::block_validation::ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::HeightDifference,
        },
        "validation_cancelled",
    )]
    #[case::cancel_channel_closed(
        crate::block_validation::ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::ChannelClosed,
        },
        "validation_cancelled",
    )]
    #[case::cancel_repeated_cancellation(
        crate::block_validation::ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::RepeatedCancellation,
        },
        "validation_cancelled",
    )]
    fn soft_internal_reason_tag_for_each_variant(
        #[case] err: crate::block_validation::ValidationError,
        #[case] expected_tag: &'static str,
    ) {
        assert_eq!(soft_internal_reason_tag(&err), expected_tag);
    }

    /// Test harness for the soft-internal-discard LRU + recovery path.
    ///
    /// Drives `BlockTreeServiceInner::discard_and_broadcast` and
    /// `on_block_validation_finished` against the real struct (no inlined
    /// re-implementation of the discard gate). Only the four fields that
    /// `discard_and_broadcast` and the Valid-recovery path actually touch
    /// — `cache`, `chain_sync_state`, `service_senders`, and
    /// `recent_soft_internal_discards` — are exercised; everything else is
    /// a cheap testing default. The held `TempDir` keeps the MDBX env alive
    /// for the duration of the test.
    struct DiscardHarness {
        inner: BlockTreeServiceInner,
        block_state_rx: tokio::sync::broadcast::Receiver<BlockStateUpdated>,
        /// Retained VDF re-anchor receiver so the gate tests can observe a queued
        /// `ReanchorRequest`; unused by the discard/recovery tests.
        vdf_reanchor_rx: irys_vdf::ReanchorReceiver,
        /// Subscribed before any test step runs, so the tip-change tests can observe
        /// every broadcast `ReorgEvent`.
        reorg_rx: tokio::sync::broadcast::Receiver<ReorgEvent>,
        /// Retained Reth service receiver; `answer_fork_choice_updates` takes it so a
        /// tip change can complete its FCU round trip.
        reth_service_rx: Option<UnboundedReceiver<Traced<RethServiceMessage>>>,
        /// Retained mempool receiver, so the tip-change tests can observe every
        /// `BlockConfirmed`.
        mempool_rx: UnboundedReceiver<Traced<MempoolServiceMessage>>,
        /// Retained storage module receiver: epoch block detection sends to it and
        /// fails when it is closed.
        storage_modules_rx: UnboundedReceiver<Traced<StorageModuleServiceMessage>>,
        /// Retained chunk migration receiver: block migration sends to it and fails
        /// when it is closed.
        _chunk_migration_rx:
            UnboundedReceiver<Traced<crate::chunk_migration_service::ChunkMigrationServiceMessage>>,
        /// Hash of the synthetic genesis seeded into the cache by `new()`.
        /// Use as `previous_block_hash` when inserting test blocks via
        /// `insert_block_in_cache`.
        genesis_hash: H256,
        /// MDBX env is mmap-backed: dropping the temp dir before the inner
        /// service would unlink the underlying files. Hold both.
        _tempdir: irys_testing_utils::tempfile::TempDir,
    }

    impl DiscardHarness {
        fn new() -> Self {
            Self::new_with_config(|_| {})
        }

        /// Like [`Self::new`] but lets the caller tweak the `NodeConfig` (e.g. the
        /// VDF reset frequency) before the inner service is built. The live VDF
        /// step starts at 0, so the re-anchor gate's `live_step` term is inert.
        fn new_with_config(configure: impl FnOnce(&mut irys_types::NodeConfig)) -> Self {
            Self::new_with_config_and_live_step(configure, 0)
        }

        /// Like [`Self::new_with_config`], additionally pinning the local VDF's
        /// live step — the gate's third crossing term.
        fn new_with_config_and_live_step(
            configure: impl FnOnce(&mut irys_types::NodeConfig),
            live_step: u64,
        ) -> Self {
            use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
            use irys_domain::BlockIndex;
            use irys_testing_utils::{IrysBlockHeaderTestExt as _, utils::TempDirBuilder};
            use irys_types::{ConsensusConfig, NodeConfig};
            use reth_db::mdbx::DatabaseArguments;

            let tmp = TempDirBuilder::new().build();
            let db_env = open_or_create_db(
                tmp.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().expect("irys_testing db args"),
            )
            .expect("open temp MDBX env");
            let db = DatabaseProvider(Arc::new(db_env));

            let block_index = BlockIndex::new_for_testing(db.clone());
            let block_index_guard = BlockIndexReadGuard::new(block_index);

            let mut genesis = IrysBlockHeader::new_mock_header();
            genesis.height = 0;
            genesis.test_sign();
            let genesis_hash = genesis.block_hash;
            let cache = Arc::new(RwLock::new(BlockTree::new(
                &genesis,
                ConsensusConfig::testing(),
            )));

            let (service_senders, service_rx) = crate::test_helpers::build_test_service_senders();
            // Subscribe BEFORE the discard path runs so the broadcast event is
            // observable. Retain the VDF re-anchor receiver so gate tests can
            // observe a queued request, and the Reth, mempool and storage module
            // receivers for the tip-change tests; the rest of `service_rx` drops here.
            let block_state_rx = service_senders.subscribe_block_state_updates();
            let reorg_rx = service_senders.subscribe_reorgs();
            let vdf_reanchor_rx = service_rx.vdf_reanchor;
            let reth_service_rx = Some(service_rx.reth_service);
            let mempool_rx = service_rx.mempool;
            let storage_modules_rx = service_rx.storage_modules;

            let mut node_config = NodeConfig::testing();
            configure(&mut node_config);
            let miner_address = node_config.miner_address();
            let config = irys_types::Config::new_with_random_peer_id(node_config);

            let chain_sync_state = ChainSyncState::new(false, false);

            // A real migration service: the tip-change tests migrate blocks
            // and roll back the block index through it.
            let (chunk_migration_sender, chunk_migration_rx) =
                tokio::sync::mpsc::unbounded_channel();
            let block_migration_service = BlockMigrationService::new(
                db.clone(),
                block_index_guard.clone(),
                None,
                config.consensus.chunk_size,
                config.consensus.epoch.num_blocks_in_epoch,
                config.consensus.block_tree_depth,
                config.consensus.block_migration_depth as u64,
                cache.clone(),
                chunk_migration_sender,
                service_senders.packing_sender(),
                service_senders.storage_modules.clone(),
                false, // tests do not seed stream de-dup
            );

            // Real (not mocked) VDF state pinned at `live_step`; the gate only
            // reads the lock-free counter.
            let vdf_state =
                VdfStateReadonly::new(Arc::new(RwLock::new(irys_vdf::state::VdfState::new(
                    64,
                    live_step,
                    Arc::new(std::sync::atomic::AtomicBool::new(false)),
                ))));

            let inner = BlockTreeServiceInner {
                db,
                cache,
                miner_address,
                block_index_guard,
                config,
                service_senders,
                block_migration_service,
                chain_sync_state,
                recent_soft_internal_discards: LruCache::new(
                    NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap(),
                ),
                lifecycle_timestamps: Arc::new(BlockTreeLifecycleTimestamps::default()),
                vdf_state,
                vdf_reanchor_watchdog_last_sent: None,
            };

            Self {
                inner,
                block_state_rx,
                vdf_reanchor_rx,
                reorg_rx,
                reth_service_rx,
                mempool_rx,
                storage_modules_rx,
                _chunk_migration_rx: chunk_migration_rx,
                genesis_hash,
                _tempdir: tmp,
            }
        }

        /// Seed a block into the BlockTree cache so cache-presence-gated
        /// production paths (e.g., `on_block_validation_finished`'s
        /// recovery-LRU pop, which is ordered after the cache check) can
        /// observe the hash. The block chains directly off the genesis
        /// the harness was constructed with.
        fn insert_block_in_cache(&mut self, block_hash: H256, height: u64) {
            use irys_domain::{CommitmentSnapshot, EmaSnapshot, EpochSnapshot};
            use irys_testing_utils::IrysBlockHeaderTestExt as _;

            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            header.previous_block_hash = self.genesis_hash;
            // `test_sign()` derives `block_hash` from the signature, so the
            // explicit override must come AFTER signing. The signature is
            // therefore not consistent with the stored hash — the cache
            // doesn't validate signatures, only the recovery-pop path
            // exercised here cares about the lookup key.
            header.test_sign();
            header.block_hash = block_hash;

            let sealed = Arc::new(SealedBlock::new_unchecked(
                Arc::new(header),
                BlockTransactions::default(),
            ));
            let ema = EmaSnapshot::genesis(sealed.header());

            let mut cache = self.inner.cache.write().expect("cache lock for seed");
            cache
                .add_block(
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    Arc::new(EpochSnapshot::default()),
                    ema,
                )
                .expect("seed cache with test block");
            // Advance the block to ValidationScheduled so that
            // `mark_block_as_valid` succeeds in `on_block_validation_finished`.
            // Production flow: add_block → mark_block_as_validation_scheduled →
            // submit to validation service → mark_block_as_valid on success.
            cache
                .mark_block_as_validation_scheduled(&block_hash)
                .expect("mark ValidationScheduled for test block");
        }

        /// Drain pending broadcast events into a Vec. Used to assert the
        /// `BlockStateUpdated` shape after a discard.
        fn collect_broadcasts(&mut self) -> Vec<BlockStateUpdated> {
            let mut out = Vec::new();
            while let Ok(event) = self.block_state_rx.try_recv() {
                out.push(event);
            }
            out
        }

        /// Answer every fork-choice update the service sends, as the Reth service does,
        /// so a tip change runs to completion. Returns the recorded updates, in order.
        fn answer_fork_choice_updates(&mut self) -> Arc<Mutex<Vec<ForkChoiceUpdateMessage>>> {
            let mut reth_service_rx = self
                .reth_service_rx
                .take()
                .expect("Reth service receiver not yet taken");
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let recorder = Arc::clone(&recorded);
            tokio::spawn(async move {
                while let Some(message) = reth_service_rx.recv().await {
                    let (message, _span) = message.into_parts();
                    if let RethServiceMessage::ForkChoice { update, response } = message {
                        recorder.lock().expect("fork-choice recorder").push(update);
                        let _ = response.send(());
                    }
                }
            });
            recorded
        }

        /// The last fork-choice update names the tip as head, and its confirmed and
        /// finalized blocks are ancestors of the tip.
        fn assert_last_fork_choice_follows_tip(
            &self,
            updates: &Arc<Mutex<Vec<ForkChoiceUpdateMessage>>>,
        ) {
            let updates = updates.lock().expect("fork-choice recorder");
            let last = updates.last().expect("at least one fork-choice update");
            let cache = self
                .inner
                .cache
                .read()
                .expect("cache lock for tip ancestry");
            let tip = cache.tip;
            assert_eq!(last.head_hash, tip, "the fork-choice head is the tip");

            let mut tip_ancestry = vec![tip];
            let mut current = tip;
            while let Some(block) = cache.get_block(&current)
                && block.height > 0
            {
                current = block.previous_block_hash;
                tip_ancestry.push(current);
            }
            assert!(
                tip_ancestry.contains(&last.confirmed_hash),
                "the confirmed block is an ancestor of the tip"
            );
            assert!(
                tip_ancestry.contains(&last.finalized_hash),
                "the finalized block is an ancestor of the tip"
            );
        }

        /// Add a child of `parent` with `cumulative_diff` to the cache. With
        /// `schedule_validation` the block waits for its validation result, as a block
        /// handed to the validation service does.
        fn add_child_block(
            &mut self,
            parent: &IrysBlockHeader,
            cumulative_diff: u64,
            schedule_validation: bool,
        ) -> IrysBlockHeader {
            use irys_domain::{CommitmentSnapshot, EmaSnapshot, EpochSnapshot};
            use irys_testing_utils::IrysBlockHeaderTestExt as _;

            let mut header = IrysBlockHeader::new_mock_header();
            header.height = parent.height + 1;
            header.previous_block_hash = parent.block_hash;
            header.cumulative_diff = irys_types::U256::from(cumulative_diff);
            header.test_sign();

            let sealed = Arc::new(SealedBlock::new_unchecked(
                Arc::new(header.clone()),
                BlockTransactions::default(),
            ));
            let ema = EmaSnapshot::genesis(sealed.header());
            let mut cache = self
                .inner
                .cache
                .write()
                .expect("cache lock for child block");
            cache
                .add_block(
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    Arc::new(EpochSnapshot::default()),
                    ema,
                )
                .expect("add child block");
            if schedule_validation {
                cache
                    .mark_block_as_validation_scheduled(&header.block_hash)
                    .expect("mark ValidationScheduled for child block");
            }
            header
        }

        /// Record the genesis block as migrated, so a tip change can run block migration.
        fn index_genesis(&self) {
            self.inner
                .block_index_guard
                .read()
                .push_item(
                    &irys_types::BlockIndexItem {
                        block_hash: self.genesis_hash,
                        num_ledgers: 0,
                        ledgers: vec![],
                    },
                    0,
                )
                .expect("index genesis");
        }

        fn genesis(&self) -> IrysBlockHeader {
            let cache = self.inner.cache.read().expect("cache lock for genesis");
            cache
                .get_block(&self.genesis_hash)
                .expect("genesis in cache")
                .clone()
        }

        fn tip(&self) -> BlockHash {
            self.inner.cache.read().expect("cache lock for tip").tip
        }

        /// Add a chain of blocks on `parent`, one block for each cumulative difficulty,
        /// and give each block a `Valid` result before the next block is added.
        async fn add_validated_chain(
            &mut self,
            parent: &IrysBlockHeader,
            cumulative_diffs: &[u64],
        ) -> Vec<IrysBlockHeader> {
            let mut chain: Vec<IrysBlockHeader> = vec![];
            for &cumulative_diff in cumulative_diffs {
                let block =
                    self.add_child_block(chain.last().unwrap_or(parent), cumulative_diff, true);
                self.inner
                    .on_block_validation_finished(block.block_hash, ValidationResult::Valid)
                    .await
                    .expect("validation result handled");
                chain.push(block);
            }
            chain
        }

        /// Hashes of the blocks sent to the mempool as `BlockConfirmed`, in send order.
        fn take_confirmed_blocks(&mut self) -> Vec<BlockHash> {
            let mut confirmed = vec![];
            while let Ok(message) = self.mempool_rx.try_recv() {
                if let (MempoolServiceMessage::BlockConfirmed(block), _span) = message.into_parts()
                {
                    confirmed.push(block.header().block_hash);
                }
            }
            confirmed
        }

        /// Hashes of the migrated blocks, from height 0 up.
        fn indexed_block_hashes(&self) -> Vec<BlockHash> {
            let block_index = self.inner.block_index_guard.read();
            (0..block_index.num_blocks())
                .map(|height| {
                    block_index
                        .get_item(height)
                        .expect("block index item below num_blocks")
                        .block_hash
                })
                .collect()
        }
    }

    /// `n` throwaway sealed blocks for a synthetic `ReorgEvent.old_fork` — only
    /// its length is read by the re-anchor gate (deep-reorg guard).
    fn sealed_fork(n: usize) -> Vec<Arc<SealedBlock>> {
        (0..n)
            .map(|i| {
                let mut hdr = IrysBlockHeader::new_mock_header();
                hdr.height = i as u64 + 1;
                Arc::new(SealedBlock::new_unchecked(
                    Arc::new(hdr),
                    BlockTransactions::default(),
                ))
            })
            .collect()
    }

    /// A canonical tip header carrying its own VDF steps `[1..=c_step]` (value ==
    /// step number), so the gate's window build resolves entirely from this header
    /// with no ancestry walk. Returns the tip and the expected window values.
    fn canonical_tip(c_step: u64, next_seed: H256) -> (IrysBlockHeader, Vec<H256>) {
        let steps: Vec<H256> = (1..=c_step).map(H256::from_low_u64_be).collect();
        let mut tip = IrysBlockHeader::new_mock_header();
        tip.vdf_limiter_info.global_step_number = c_step;
        tip.vdf_limiter_info.steps = H256List(steps.clone());
        tip.vdf_limiter_info.next_seed = next_seed;
        (tip, steps)
    }

    /// A synthetic `ReorgEvent` with the LCA at `lca_step` and an `old_fork` of
    /// the given depth. Only the fields the gate reads are meaningful.
    fn reorg_from(lca_step: u64, old_fork_len: usize, new_tip: H256) -> ReorgEvent {
        let mut fork_parent = IrysBlockHeader::new_mock_header();
        fork_parent.vdf_limiter_info.global_step_number = lca_step;
        ReorgEvent {
            old_fork: Arc::new(sealed_fork(old_fork_len)),
            new_fork: Arc::new(vec![]),
            fork_parent: Arc::new(fork_parent),
            new_tip,
            timestamp: std::time::UNIX_EPOCH,
            db: None,
        }
    }

    #[test]
    fn deep_reorg_across_divergent_boundary_queues_reanchor() {
        // reset_frequency 4; block_migration_depth defaults to 1 in testing.
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 4;
        });

        let next_seed = H256::repeat_byte(0x99);
        let (tip, steps) = canonical_tip(20, next_seed);
        // first_divergent_boundary(lca=5, rf=4) = 12; c_step 20 >= 12 → crossed.
        let reorg = reorg_from(5, 2, tip.block_hash);

        h.inner.maybe_reanchor_vdf_after_reorg(&tip, &reorg);

        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("a deep cross-boundary reorg must queue a ReanchorRequest");
        assert_eq!(req.c_step, 20);
        assert_eq!(
            req.seed, tip.vdf_limiter_info.seed,
            "the request must carry the tip's `seed` (the boundary-tip fold value)"
        );
        assert_eq!(req.next_seed, next_seed);
        let sent: Vec<H256> = req.canonical_seeds.iter().map(|s| s.0).collect();
        assert_eq!(
            sent, steps,
            "the gate must ship the canonical [floor, c_step] window from the tip's ancestry"
        );
        assert!(
            h.inner
                .service_senders
                .vdf_reanchor_signals
                .is_buffer_suspect(),
            "queuing a re-anchor must mark the buffer suspect until the heal applies"
        );
    }

    #[test]
    fn shallow_reorg_does_not_queue_reanchor() {
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 4;
        });
        let (tip, _) = canonical_tip(20, H256::repeat_byte(0x99));
        // old_fork depth 1 == block_migration_depth(1) → not a deep reorg.
        let reorg = reorg_from(5, 1, tip.block_hash);

        h.inner.maybe_reanchor_vdf_after_reorg(&tip, &reorg);

        assert!(
            h.vdf_reanchor_rx.borrow_and_update().is_none(),
            "a shallow reorg must not queue a re-anchor"
        );
        assert!(
            !h.inner
                .service_senders
                .vdf_reanchor_signals
                .is_buffer_suspect(),
            "a shallow reorg must not mark the buffer suspect"
        );
    }

    #[test]
    fn deep_reorg_below_divergent_boundary_does_not_queue_reanchor() {
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 100;
        });
        // first_divergent_boundary(lca=5, rf=100) = 200; c_step 20 < 200 → not crossed.
        let (tip, _) = canonical_tip(20, H256::repeat_byte(0x99));
        let reorg = reorg_from(5, 5, tip.block_hash);

        h.inner.maybe_reanchor_vdf_after_reorg(&tip, &reorg);

        assert!(
            h.vdf_reanchor_rx.borrow_and_update().is_none(),
            "a reorg below the first divergent boundary must not queue a re-anchor"
        );
    }

    /// Live VDF step alone can cross the divergent boundary before either tip.
    #[test]
    fn deep_reorg_crossed_only_by_live_vdf_queues_reanchor() {
        let mut h = DiscardHarness::new_with_config_and_live_step(
            |c| {
                let cc = c.consensus.get_mut();
                cc.block_migration_depth = 1;
                cc.vdf.reset_frequency = 100;
            },
            // first_divergent_boundary(lca=5, rf=100) = 200: the live VDF at
            // 250 has crossed it; the canonical tip below stays at 20.
            250,
        );
        let (tip, steps) = canonical_tip(20, H256::repeat_byte(0x99));
        let reorg = reorg_from(5, 5, tip.block_hash);

        h.inner.maybe_reanchor_vdf_after_reorg(&tip, &reorg);

        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("a live-VDF-only boundary crossing must queue a ReanchorRequest");
        assert_eq!(
            req.c_step, 20,
            "the request still anchors at the canonical tip; the VDF thread recomputes the tail"
        );
        let sent: Vec<H256> = req.canonical_seeds.iter().map(|s| s.0).collect();
        assert_eq!(sent, steps);
        assert!(
            h.inner
                .service_senders
                .vdf_reanchor_signals
                .is_buffer_suspect(),
            "the live-only crossing must also mark the buffer suspect"
        );
    }

    /// Companion bound for the live term: a live step short of the boundary
    /// adds nothing — with every crossing term below it the gate stays quiet.
    #[test]
    fn deep_reorg_with_live_vdf_below_boundary_does_not_queue_reanchor() {
        let mut h = DiscardHarness::new_with_config_and_live_step(
            |c| {
                let cc = c.consensus.get_mut();
                cc.block_migration_depth = 1;
                cc.vdf.reset_frequency = 100;
            },
            // Below first_divergent_boundary(lca=5, rf=100) = 200.
            150,
        );
        let (tip, _) = canonical_tip(20, H256::repeat_byte(0x99));
        let reorg = reorg_from(5, 5, tip.block_hash);

        h.inner.maybe_reanchor_vdf_after_reorg(&tip, &reorg);

        assert!(
            h.vdf_reanchor_rx.borrow_and_update().is_none(),
            "no crossing term reached the boundary, so no re-anchor may queue"
        );
        assert!(
            !h.inner
                .service_senders
                .vdf_reanchor_signals
                .is_buffer_suspect(),
            "an idle gate must not mark the buffer suspect"
        );
    }

    #[test]
    fn send_reanchor_request_publishes_without_reorg_gating() {
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 4;
        });

        let (older_tip, _) = canonical_tip(20, H256::repeat_byte(0x99));
        let (fresh_tip, fresh_steps) = canonical_tip(21, H256::repeat_byte(0x98));

        h.inner.send_reanchor_request(&older_tip);
        h.inner.send_reanchor_request(&fresh_tip);

        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("the retry path must publish a request with no ReorgEvent");
        assert_eq!(req.c_step, 21, "the freshest request wins");
        let sent: Vec<H256> = req.canonical_seeds.iter().map(|s| s.0).collect();
        assert_eq!(sent, fresh_steps);
    }

    /// Insert `tip` as the canonical cache tip for the suspect-watchdog tests.
    fn make_cache_tip(
        h: &mut DiscardHarness,
        mut tip: IrysBlockHeader,
        cumulative_diff: u64,
    ) -> IrysBlockHeader {
        use irys_domain::{CommitmentSnapshot, EmaSnapshot, EpochSnapshot};
        use irys_testing_utils::IrysBlockHeaderTestExt as _;

        tip.previous_block_hash = h.genesis_hash;
        tip.height = 1;
        tip.cumulative_diff = irys_types::U256::from(cumulative_diff);
        // `test_sign()` derives `block_hash` from the header content; keep the
        // signed hash so the watchdog's `(tip hash, live step)` dedupe key is a
        // real, content-distinct value.
        tip.test_sign();

        let sealed = Arc::new(SealedBlock::new_unchecked(
            Arc::new(tip.clone()),
            BlockTransactions::default(),
        ));
        let ema = EmaSnapshot::genesis(sealed.header());

        let mut cache = h.inner.cache.write().expect("cache lock for tip seed");
        cache
            .add_block(
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                ema,
            )
            .expect("seed cache with tip block");
        // Production flow: add_block → mark_block_as_validation_scheduled →
        // mark_block_as_valid → mark_tip.
        cache
            .mark_block_as_validation_scheduled(&tip.block_hash)
            .expect("mark ValidationScheduled for tip block");
        cache
            .mark_block_as_valid(&tip.block_hash)
            .expect("mark Valid for tip block");
        assert!(
            cache.mark_tip(&tip.block_hash).expect("mark_tip"),
            "the seeded block must become the canonical tip"
        );
        tip
    }

    #[test]
    fn watchdog_retry_does_nothing_while_buffer_not_suspect() {
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 4;
        });

        h.inner.retry_vdf_reanchor_if_suspect();

        assert!(
            h.vdf_reanchor_rx.borrow_and_update().is_none(),
            "a not-suspect tick must not publish a re-anchor request"
        );
        assert!(
            h.inner.vdf_reanchor_watchdog_last_sent.is_none(),
            "a not-suspect tick must leave the dedupe state disarmed"
        );
    }

    #[test]
    fn watchdog_retry_fires_once_per_input_state_while_suspect() {
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 4;
        });

        let (tip1_hdr, tip1_steps) = canonical_tip(20, H256::repeat_byte(0x99));
        make_cache_tip(&mut h, tip1_hdr, 6000);
        h.inner
            .service_senders
            .vdf_reanchor_signals
            .mark_buffer_suspect();

        // Tick 1: fresh inputs → read the cache tip and publish.
        h.inner.retry_vdf_reanchor_if_suspect();
        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("the first suspect tick must publish a re-anchor request");
        assert_eq!(
            req.c_step, 20,
            "the request must anchor at the cache tip's step"
        );
        let sent: Vec<H256> = req.canonical_seeds.iter().map(|s| s.0).collect();
        assert_eq!(
            sent, tip1_steps,
            "the watchdog must ship the cache tip's own canonical window"
        );

        // Tick 2: identical inputs → suppressed (no new value on the channel).
        h.inner.retry_vdf_reanchor_if_suspect();
        assert!(
            !h.vdf_reanchor_rx.has_changed().unwrap(),
            "an identical-inputs tick must not re-publish"
        );

        // A fresher canonical tip changes the dedupe key → the watchdog re-fires
        // (only the tip hash moves; the live step stays 0).
        let (tip2_hdr, _) = canonical_tip(21, H256::repeat_byte(0x98));
        make_cache_tip(&mut h, tip2_hdr, 7000);
        h.inner.retry_vdf_reanchor_if_suspect();
        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("a changed tip must re-fire the watchdog");
        assert_eq!(
            req.c_step, 21,
            "the re-fire must anchor at the new canonical tip's step"
        );
    }

    #[test]
    fn watchdog_retry_does_not_arm_on_failed_publish() {
        let mut h = DiscardHarness::new();

        // Genesis tip has no steps → window build fails → publish returns false.
        h.inner
            .service_senders
            .vdf_reanchor_signals
            .mark_buffer_suspect();

        h.inner.retry_vdf_reanchor_if_suspect();
        h.inner.retry_vdf_reanchor_if_suspect();

        assert!(
            h.vdf_reanchor_rx.borrow_and_update().is_none(),
            "a failed publish must not place a request on the channel"
        );
        assert!(
            h.inner.vdf_reanchor_watchdog_last_sent.is_none(),
            "a failed publish must leave the watchdog armed for the next tick"
        );
    }

    #[test]
    fn watchdog_retry_rearms_after_a_heal_clears_suspect() {
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.block_migration_depth = 1;
            cc.vdf.reset_frequency = 4;
        });

        let (tip_hdr, _) = canonical_tip(20, H256::repeat_byte(0x99));
        make_cache_tip(&mut h, tip_hdr, 6000);

        // Episode 1: suspect → the tick publishes.
        h.inner
            .service_senders
            .vdf_reanchor_signals
            .mark_buffer_suspect();
        h.inner.retry_vdf_reanchor_if_suspect();
        assert!(
            h.vdf_reanchor_rx.borrow_and_update().is_some(),
            "the first suspect episode must publish"
        );

        // A heal applies (the only legitimate clear) → the next tick disarms.
        h.inner
            .service_senders
            .vdf_reanchor_signals
            .test_record_heal_applied();
        h.inner.retry_vdf_reanchor_if_suspect();
        assert!(
            !h.vdf_reanchor_rx.has_changed().unwrap(),
            "a not-suspect tick must not publish"
        );
        assert!(
            h.inner.vdf_reanchor_watchdog_last_sent.is_none(),
            "clearing suspect must disarm the watchdog dedupe"
        );

        // Episode 2: suspect again with identical inputs must still re-fire.
        h.inner
            .service_senders
            .vdf_reanchor_signals
            .mark_buffer_suspect();
        h.inner.retry_vdf_reanchor_if_suspect();
        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("a fresh suspect episode must re-fire even with identical inputs");
        assert_eq!(req.c_step, 20);
    }

    #[test]
    fn send_reanchor_request_window_clamps_to_capacity() {
        // calc_capacity = max(num_chunks_in_partition / num_chunks_in_recall_range,
        // max_allowed_vdf_fork_steps) = max(8, 8) = 8.
        let mut h = DiscardHarness::new_with_config(|c| {
            let cc = c.consensus.get_mut();
            cc.vdf.max_allowed_vdf_fork_steps = 8;
            cc.num_chunks_in_partition = 8;
            cc.num_chunks_in_recall_range = 1;
        });

        // c_step 20 > capacity 8, so the window must clamp, not run [1, 20].
        let (tip, _) = canonical_tip(20, H256::repeat_byte(0x99));
        h.inner.send_reanchor_request(&tip);

        let req = h
            .vdf_reanchor_rx
            .borrow_and_update()
            .clone()
            .expect("send_reanchor_request must publish a request");
        assert_eq!(req.c_step, 20);
        let sent: Vec<H256> = req.canonical_seeds.iter().map(|s| s.0).collect();
        let expected: Vec<H256> = (13..=20).map(H256::from_low_u64_be).collect();
        assert_eq!(
            sent, expected,
            "window must clamp to [c_step-capacity+1, c_step] = [13,20], length == capacity 8, NOT reorg depth and NOT [1,20]"
        );
        assert_eq!(sent.len(), 8);
    }

    /// SoftInternal discard path must record (block_hash, reason) into the
    /// LRU so a subsequent Valid arrival can be counted as a gossip-driven
    /// recovery. Drives `discard_and_broadcast` directly and asserts on the
    /// observable side effects (LRU contents + broadcast event shape) — not
    /// a syntactic mirror of the production gate.
    #[test]
    fn soft_internal_discard_inserts_into_lru() {
        let mut h = DiscardHarness::new();
        let block_hash = H256([0xAB; 32]);
        // Cache must hold the block — post-finding-1, the discard side
        // effects (LRU, metric, broadcast) are gated on the cache write
        // actually happening.
        h.insert_block_in_cache(block_hash, /*height=*/ 1);

        let result: ValidationResult =
            crate::block_validation::ValidationError::ParentBlockMissing { block_hash }.into();

        h.inner
            .discard_and_broadcast(block_hash, result, DiscardKind::SoftInternal)
            .expect("discard_and_broadcast must succeed for soft-internal");

        // Observable side effect 1: LRU records (block_hash, reason_tag) so
        // a subsequent Valid arrival can be counted as a recovery. The
        // production gate at this same call site also fires
        // `metrics::record_soft_internal_discard(reason)`; since both live
        // in the same `if` block, the LRU state transitively confirms the
        // counter increment.
        assert_eq!(
            h.inner.recent_soft_internal_discards.get(&block_hash),
            Some(&"parent_block_missing"),
            "SoftInternal discard must record the reason tag for soft-internal recovery accounting"
        );

        // Observable side effect 2: a discard broadcast carries the input
        // ValidationResult so downstream subscribers can react.
        let events = h.collect_broadcasts();
        assert_eq!(events.len(), 1, "exactly one BlockStateUpdated must fire");
        let event = &events[0];
        assert_eq!(event.block_hash, block_hash);
        assert!(event.discarded);
        assert!(
            matches!(
                event.validation_result,
                ValidationResult::InternalFailure(_)
            ),
            "discard broadcast must carry the InternalFailure verdict"
        );
    }

    /// Cache-miss Valid invariant: a Valid result for a hash that is NOT in
    /// the cache must (a) preserve the recovery LRU entry so a later
    /// legitimate re-delivery can still be counted as a recovery, AND (b)
    /// emit a `BlockStateUpdated` broadcast so downstream waiters
    /// (read_block_from_state, wait_for_parent_validation) don't hang.
    /// Drives the real `on_block_validation_finished`.
    #[tokio::test]
    async fn valid_result_with_cache_miss_preserves_lru() {
        let mut h = DiscardHarness::new();
        let block_hash = H256([0xCD; 32]);

        // Seed the LRU as if a prior SoftInternal discard ran for this hash.
        // Deliberately do NOT seed the cache — this is the spurious-arrival
        // path the reorder is guarding.
        h.inner
            .recent_soft_internal_discards
            .put(block_hash, "execution_payload_cache_evicted");

        h.inner
            .on_block_validation_finished(block_hash, ValidationResult::Valid)
            .await
            .expect("Valid on cache-miss returns early via warn path");

        // (a) LRU entry must be preserved — the cache-miss is spurious, not a
        // confirmed recovery. A later legitimate re-delivery still needs to be
        // counted.
        assert_eq!(
            h.inner
                .recent_soft_internal_discards
                .get(&block_hash)
                .copied(),
            Some("execution_payload_cache_evicted"),
            "cache-miss Valid must preserve the LRU entry — a later legitimate \
             arrival still needs to be counted as a recovery"
        );

        // (b) BlockStateUpdated must still fire so downstream waiters
        // don't hang. height = 0 and discarded = false are the sentinel
        // values emitted on the cache-miss path.
        let events = h.collect_broadcasts();
        assert_eq!(
            events.len(),
            1,
            "cache-miss Valid must emit exactly one BlockStateUpdated"
        );
        let event = &events[0];
        assert_eq!(event.block_hash, block_hash);
        assert!(!event.discarded, "cache-miss Valid must not set discarded");
        assert_eq!(event.height, 0, "cache-miss Valid uses sentinel height = 0");
        assert!(
            matches!(event.validation_result, ValidationResult::Valid),
            "cache-miss Valid must carry ValidationResult::Valid"
        );
    }

    /// Companion to `valid_result_with_cache_miss_preserves_lru`: when the
    /// block IS in the cache, a Valid result must pop the LRU (so a second
    /// Valid for the same hash wouldn't double-count) and call
    /// `metrics::record_soft_internal_recovered` against the original
    /// reason tag. Drives `on_block_validation_finished` against a cache
    /// seeded via the harness helper.
    #[tokio::test]
    async fn valid_result_with_cache_hit_pops_lru_and_records_recovery() {
        let mut h = DiscardHarness::new();
        let block_hash = H256([0xCE; 32]);

        // Seed cache + LRU. Both must be present for the recovery path to
        // fire — the order is cache-check → LRU-pop, so the LRU only
        // releases its entry once the cache confirms the block is back.
        h.insert_block_in_cache(block_hash, /*height=*/ 1);
        h.inner
            .recent_soft_internal_discards
            .put(block_hash, "parent_block_missing");

        h.inner
            .on_block_validation_finished(block_hash, ValidationResult::Valid)
            .await
            .expect("Valid on cache-hit recovery completes without error");

        assert!(
            h.inner
                .recent_soft_internal_discards
                .get(&block_hash)
                .is_none(),
            "cache-hit Valid must pop the LRU entry to prevent double-count"
        );

        // Second Valid for the same hash is a no-op for the LRU (already
        // empty); it must remain empty so duplicate Valid arrivals don't
        // re-arm the recovery counter.
        h.inner
            .on_block_validation_finished(block_hash, ValidationResult::Valid)
            .await
            .expect("idempotent second Valid call must not error");
        assert!(
            h.inner
                .recent_soft_internal_discards
                .get(&block_hash)
                .is_none(),
            "second Valid arrival must remain a no-op for the LRU"
        );
    }

    /// `Invalid` (peer-attributable) and node-fault `InternalFailure` paths
    /// MUST NOT pollute the LRU. The LRU only tracks soft-internal discards
    /// so recovery accounting is not contaminated by consensus rejections or
    /// abort-causing local faults.
    #[test]
    fn non_soft_internal_paths_leave_lru_untouched() {
        // 1. Consensus Invalid: dispatch produces ValidationResult::Invalid
        //    and calls `discard_and_broadcast` with DiscardKind::Invalid.
        //    The production gate short-circuits the LRU put for any non-
        //    SoftInternal kind — we drive the real helper here.
        let mut h = DiscardHarness::new();
        let invalid_hash = H256([0x01; 32]);
        h.insert_block_in_cache(invalid_hash, /*height=*/ 1);
        let invalid_result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        h.inner
            .discard_and_broadcast(invalid_hash, invalid_result, DiscardKind::Invalid)
            .expect("discard_and_broadcast must succeed for Invalid arm");
        assert_eq!(
            h.inner.recent_soft_internal_discards.len(),
            0,
            "Invalid path must not touch the LRU"
        );
        // The broadcast still fires — confirms we exercised the same code
        // path (not an early-return that skipped the LRU by accident).
        let events = h.collect_broadcasts();
        assert!(
            events
                .iter()
                .any(|e| e.block_hash == invalid_hash && e.discarded),
            "Invalid arm must still broadcast the discard event"
        );

        // 2. Node-fault InternalFailure: the handler panics BEFORE
        //    `discard_and_broadcast` runs (see
        //    `on_block_validation_finished` lines 532-544). We assert the
        //    classification gate (`is_node_fault`) catches the variant —
        //    proving the panic-before-LRU-update ordering is preserved
        //    structurally. Driving the panic in a unit test would crash
        //    the runner, so we verify the precondition only.
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
        assert_eq!(
            h.inner.recent_soft_internal_discards.len(),
            0,
            "LRU still untouched after the node-fault precondition check"
        );
    }

    /// A `ValidationCancelled` SoftInternal discard MUST NOT insert into
    /// the recovery LRU and MUST NOT increment the
    /// `soft_internal_discard_total` counter. Cancellations are local "we
    /// moved on" events, not gossip-recoverable failures — recording them
    /// would inflate the discard counter and leave the recovery counter
    /// permanently behind (fresh gossip rarely re-triggers the same cancel
    /// race for the same hash).
    ///
    /// The LRU assertion is the load-bearing one. In production the LRU put
    /// and the `metrics::record_soft_internal_discard` call share the same
    /// `if` block — observing an empty LRU after a SoftInternal +
    /// ValidationCancelled discard transitively confirms the counter did
    /// not increment. (OpenTelemetry counters don't expose a sync read API
    /// for direct counter inspection.)
    #[rstest]
    #[case::height_difference(crate::block_validation::ValidationCancelReason::HeightDifference)]
    #[case::channel_closed(crate::block_validation::ValidationCancelReason::ChannelClosed)]
    #[case::parent_missing(crate::block_validation::ValidationCancelReason::ParentMissing)]
    #[case::repeated_cancellation(
        crate::block_validation::ValidationCancelReason::RepeatedCancellation
    )]
    #[case::poa_aborted(crate::block_validation::ValidationCancelReason::PoAAborted)]
    fn validation_cancelled_softinternal_skips_lru_and_counter(
        #[case] reason: crate::block_validation::ValidationCancelReason,
    ) {
        let mut h = DiscardHarness::new();
        let block_hash = H256([0xEF; 32]);
        h.insert_block_in_cache(block_hash, /*height=*/ 1);

        let result: ValidationResult =
            crate::block_validation::ValidationError::ValidationCancelled { reason }.into();
        // Sanity: every cancel reason routes to InternalFailure and is NOT
        // a node fault, so `discard_and_broadcast` is actually reachable
        // from `on_block_validation_finished` for this variant.
        let ValidationResult::InternalFailure(inner) = &result else {
            panic!("ValidationCancelled must dispatch to InternalFailure");
        };
        assert!(
            !inner.is_node_fault(),
            "ValidationCancelled is a soft InternalFailure, not a node fault"
        );

        h.inner
            .discard_and_broadcast(block_hash, result, DiscardKind::SoftInternal)
            .expect("discard_and_broadcast must succeed for ValidationCancelled");

        assert_eq!(
            h.inner.recent_soft_internal_discards.len(),
            0,
            "ValidationCancelled (reason {:?}) must not touch the recovery LRU \
             (and transitively must not increment soft_internal_discard_total)",
            reason
        );

        // The broadcast must still fire — confirms we drove the same code
        // path (not a hidden early-return that left the LRU untouched for
        // the wrong reason).
        let events = h.collect_broadcasts();
        assert!(
            events
                .iter()
                .any(|e| e.block_hash == block_hash && e.discarded),
            "ValidationCancelled discard must still broadcast a discard event"
        );
    }

    /// When the block is no longer in the cache (or was never inserted —
    /// e.g. a direct submission to `validation_service` via
    /// `send_block_to_block_validation`), the soft-internal-recovery
    /// metric and LRU must NOT be touched (an entry would later be popped
    /// by a spurious cache-hit Valid and tick
    /// `soft_internal_recovered_total` for a discard we didn't perform).
    ///
    /// The `BlockStateUpdated` broadcast, however, MUST still fire —
    /// downstream observers (test harness `read_block_from_state`,
    /// `block_validation_tracker`) treat it as a validation-finalization
    /// signal independent of cache state. Suppressing it on the
    /// cache-miss path would break tests like
    /// `heavy_block_validation_discards_a_block_if_its_too_old` that
    /// drive validation without going through block_tree.
    #[test]
    fn discard_on_cache_miss_skips_metrics_but_still_broadcasts() {
        let mut h = DiscardHarness::new();
        let block_hash = H256([0x77; 32]);
        // Deliberately do NOT seed the cache.

        let result: ValidationResult =
            crate::block_validation::ValidationError::ParentBlockMissing { block_hash }.into();
        h.inner
            .discard_and_broadcast(block_hash, result, DiscardKind::SoftInternal)
            .expect("discard_and_broadcast must succeed even when cache is empty");

        assert!(
            h.inner
                .recent_soft_internal_discards
                .get(&block_hash)
                .is_none(),
            "cache-miss discard must not populate the soft-internal-recovery LRU"
        );
        let events = h.collect_broadcasts();
        assert_eq!(
            events.len(),
            1,
            "cache-miss discard must still emit a finalization broadcast"
        );
        let event = &events[0];
        assert_eq!(event.block_hash, block_hash);
        assert!(event.discarded);
        assert!(
            matches!(
                event.validation_result,
                ValidationResult::InternalFailure(_)
            ),
            "broadcast must carry the InternalFailure verdict even on cache-miss"
        );
    }

    /// A discard of an `Onchain` block (here the tip) returns an error and leaves the
    /// tip and the block in the tree: the block tree refuses to remove it, and the
    /// service must not continue as if the block were gone.
    #[test]
    fn discard_of_onchain_block_returns_error() {
        let mut h = DiscardHarness::new();
        let tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);

        let result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        let err = h
            .inner
            .discard_and_broadcast(tip.block_hash, result, DiscardKind::Invalid)
            .expect_err("a discard of an Onchain block must return an error");
        assert!(
            format!("{err:#}").contains("Onchain"),
            "error must name the Onchain block; got: {err:#}"
        );

        assert_eq!(h.tip(), tip.block_hash, "the tip does not change");
        let cache = h.inner.cache.read().expect("cache lock after discard");
        assert!(cache.get_block(&tip.block_hash).is_some());
        assert_eq!(
            cache.get_latest_canonical_entry().block_hash(),
            tip.block_hash
        );
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

    /// A reorg whose fork point stays within the post-prune cache window
    /// (`new_tip - (block_tree_depth - 1)`) keeps every divergent block — and
    /// the epoch/commitment snapshots keyed by their hashes — in the cache, so
    /// downstream reorg consumers can re-read them. Boundary: a fork point at
    /// exactly `min_keep_height` survives the prune.
    #[rstest]
    #[case::fork_at_window_boundary(51, 100, 50)]
    #[case::fork_just_below_tip(99, 100, 50)]
    #[case::fork_at_tip(100, 100, 50)]
    #[case::small_depth_boundary(98, 100, 3)]
    #[case::window_covers_genesis(0, 5, 50)]
    fn validate_reorg_within_cache_window_passes_inside_window(
        #[case] fork_height: u64,
        #[case] new_tip_height: u64,
        #[case] block_tree_depth: u64,
    ) {
        validate_reorg_within_cache_window(fork_height, new_tip_height, block_tree_depth)
            .expect("reorg whose fork point survives the prune must be permitted");
    }

    /// A fork point one block below the window is evicted by `cache.prune()`,
    /// cascading the divergent blocks (and their snapshots) out of the cache.
    /// The gate must surface a typed error pre-prune so the caller triggers
    /// controlled shutdown instead of crashing mid-reorg. The new-fork length at
    /// the boundary equals `block_tree_depth` exactly.
    #[rstest]
    #[case::fork_one_below_window(50, 100, 50)]
    #[case::deep_fork_small_window(10, 100, 50)]
    #[case::small_depth_just_over(97, 100, 3)]
    #[case::small_depth_well_over(80, 100, 3)]
    fn validate_reorg_within_cache_window_returns_err_below_window(
        #[case] fork_height: u64,
        #[case] new_tip_height: u64,
        #[case] block_tree_depth: u64,
    ) {
        let err = validate_reorg_within_cache_window(fork_height, new_tip_height, block_tree_depth)
            .expect_err("fork point below the cache window must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("reorg deeper than cache window"),
            "error must name the cache-window violation; got: {msg}"
        );
        assert!(
            msg.contains(&format!("fork point at height {fork_height}")),
            "error must include the fork height; got: {msg}"
        );
    }

    /// `PreValidation` reaches `soft_internal_reason_tag` whenever its
    /// inner variant classifies as `SoftInternal` (`ParentNotInCache`).
    /// The tag must come from the inner's `metric_reason()` so each one
    /// gets a distinct, grep-stable label instead of collapsing into a
    /// generic catch-all.
    #[rstest]
    #[case::preval_parent_not_in_cache(
        crate::block_validation::ValidationError::PreValidation(
            crate::block_validation::PreValidationError::ParentNotInCache {
                parent_hash: H256::zero(),
                expected_height: 0,
            },
        ),
        "parent_not_in_cache",
    )]
    fn soft_internal_reason_tag_delegates_for_prevalidation_soft_variants(
        #[case] err: crate::block_validation::ValidationError,
        #[case] expected_tag: &'static str,
    ) {
        assert_eq!(soft_internal_reason_tag(&err), expected_tag);
        // Belt-and-braces: confirm the error actually classifies as
        // SoftInternal — if a future audit flips it, this test wedges the
        // contract instead of silently testing nothing.
        assert_eq!(
            err.classify(),
            crate::block_validation::ErrorClass::SoftInternal,
            "test fixture must be SoftInternal-classified",
        );
    }

    /// Every `SoftInternal`-classified `ValidationError` variant must map
    /// to a label distinct from the legacy `"internal_error_other"`
    /// sentinel (a prior catch-all that absorbed unhandled variants and
    /// caused undercounting). Sanity-checks the exhaustive match.
    #[test]
    fn soft_internal_reason_tags_are_not_legacy_catch_all() {
        let cases: Vec<crate::block_validation::ValidationError> = vec![
            crate::block_validation::ValidationError::ExecutionPayloadCacheEvicted {
                evm_block_hash: irys_types::EvmBlockHash::ZERO,
            },
            crate::block_validation::ValidationError::ParentBlockMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::ParentCommitmentSnapshotMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::ParentEpochSnapshotMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::ParentEmaSnapshotMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::PreValidation(
                crate::block_validation::PreValidationError::ParentNotInCache {
                    parent_hash: H256::zero(),
                    expected_height: 0,
                },
            ),
            crate::block_validation::ValidationError::ValidationCancelled {
                reason: crate::block_validation::ValidationCancelReason::HeightDifference,
            },
        ];

        for err in &cases {
            let tag = soft_internal_reason_tag(err);
            assert_ne!(
                tag, "internal_error_other",
                "variant {err:?} must have a real label, not the legacy catch-all",
            );
            assert!(!tag.is_empty(), "variant {err:?} mapped to an empty tag",);
        }
    }

    /// Distinct `SoftInternal` variants (excluding the
    /// `ValidationCancelled` defensive sentinel, which is filtered upstream
    /// at the discard-site cancel gate and intentionally collapses to a
    /// single tag) must map to distinct labels so the discard metric can
    /// attribute root causes.
    #[test]
    fn soft_internal_reason_tags_are_unique_per_variant() {
        use std::collections::HashSet;
        let cases: Vec<crate::block_validation::ValidationError> = vec![
            crate::block_validation::ValidationError::ExecutionPayloadCacheEvicted {
                evm_block_hash: irys_types::EvmBlockHash::ZERO,
            },
            crate::block_validation::ValidationError::RecallRangeStepsUnavailable(
                "steps unavailable".into(),
            ),
            crate::block_validation::ValidationError::ParentBlockMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::ParentCommitmentSnapshotMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::ParentEpochSnapshotMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::ParentEmaSnapshotMissing {
                block_hash: H256::zero(),
            },
            crate::block_validation::ValidationError::PreValidation(
                crate::block_validation::PreValidationError::ParentNotInCache {
                    parent_hash: H256::zero(),
                    expected_height: 0,
                },
            ),
        ];

        let mut seen: HashSet<&'static str> = HashSet::with_capacity(cases.len());
        for err in &cases {
            let tag = soft_internal_reason_tag(err);
            assert!(
                seen.insert(tag),
                "duplicate soft-internal reason tag {tag:?} from variant {err:?}",
            );
        }
    }

    // --- Recovery-ordering regression tests ---------------------------------
    //
    // The recovery path in `on_block_validation_finished` previously popped
    // `recent_soft_internal_discards` and incremented
    // `record_soft_internal_recovered` BEFORE confirming the block was still
    // in the cache. A spurious `Valid` arrival for a hash no longer in cache
    // (eviction race, duplicate gossip, etc.) therefore both lost the
    // recovery marker that a legitimate later re-delivery would have matched,
    // and overcounted the spurious path as a recovery.
    //
    // The fix moves the cache-presence check first; only on a confirmed cache
    // hit does the LRU entry get popped and the recovery metric incremented.
    // These tests replay the ordering invariant with a small in-test model
    // (same style as the existing LRU tests above) since constructing a real
    // `BlockTreeServiceInner` requires the full service-senders graph.

    /// Helper modelling the post-fix ordering in
    /// `on_block_validation_finished`. Returns whether the recovery metric
    /// would have been incremented (and with which reason).
    ///
    /// `in_cache` simulates the result of the `cache.read().get_block(...)`
    /// lookup. On `false`, the function returns without touching the LRU and
    /// without recording a recovery — matching the production warn-and-return
    /// branch.
    fn simulate_recovery_ordering(
        lru: &mut LruCache<BlockHash, &'static str>,
        block_hash: BlockHash,
        in_cache: bool,
    ) -> Option<&'static str> {
        if !in_cache {
            // Cache miss: do not touch the LRU, do not record a recovery.
            return None;
        }
        // Cache hit: pop the LRU and report the reason so the caller can
        // record the recovery metric.
        lru.pop(&block_hash)
    }

    /// Spurious `Valid` for a hash not in the cache MUST NOT pop the LRU
    /// entry and MUST NOT increment the recovery metric. The LRU marker must
    /// remain so a later, legitimate `Valid` arrival can still be counted as
    /// a gossip-driven recovery.
    #[test]
    fn spurious_valid_with_cache_miss_preserves_lru_and_metric() {
        let mut lru: LruCache<BlockHash, &'static str> =
            LruCache::new(NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap());
        let block_hash = H256([0x6A; 32]);
        lru.put(block_hash, "execution_payload_cache_evicted");

        let recovered = simulate_recovery_ordering(&mut lru, block_hash, /*in_cache=*/ false);

        assert!(
            recovered.is_none(),
            "cache-miss recovery path must not surface a reason (metric must not fire)"
        );
        assert_eq!(
            lru.get(&block_hash),
            Some(&"execution_payload_cache_evicted"),
            "LRU entry must be preserved when the block is no longer in cache — \
             a later legitimate re-delivery still needs it to be counted as a recovery"
        );
    }

    /// Sequence: spurious cache-miss Valid arrives first (LRU must stay
    /// intact), then a legitimate Valid arrives with the block back in cache
    /// (LRU is finally consumed and the recovery is recorded). This
    /// specifically guards against the regression mode where a pre-fix
    /// LRU-before-cache-check order would have dropped the recovery on the
    /// spurious arrival and left the legitimate arrival un-counted.
    #[test]
    fn spurious_then_legitimate_valid_counts_recovery_exactly_once() {
        let mut lru: LruCache<BlockHash, &'static str> =
            LruCache::new(NonZeroUsize::new(SOFT_INTERNAL_DISCARD_LRU_CAPACITY).unwrap());
        let block_hash = H256([0x6C; 32]);
        lru.put(block_hash, "shadow_tx_generation_failed");

        // First arrival: spurious — block not in cache. No recovery.
        let first = simulate_recovery_ordering(&mut lru, block_hash, /*in_cache=*/ false);
        assert!(
            first.is_none(),
            "spurious arrival must not record a recovery"
        );
        assert!(
            lru.get(&block_hash).is_some(),
            "LRU must survive a spurious arrival"
        );

        // Second arrival: legitimate — block back in cache via re-gossip.
        let second = simulate_recovery_ordering(&mut lru, block_hash, /*in_cache=*/ true);
        assert_eq!(
            second,
            Some("shadow_tx_generation_failed"),
            "legitimate arrival must surface the original discard reason"
        );
        assert!(
            lru.get(&block_hash).is_none(),
            "LRU must be drained by the legitimate recovery"
        );

        // Third arrival: any further duplicate Valid for the same hash is a
        // no-op (matches existing valid_result_pops_from_lru_and_returns_reason
        // semantics).
        let third = simulate_recovery_ordering(&mut lru, block_hash, /*in_cache=*/ true);
        assert!(
            third.is_none(),
            "duplicate Valid for already-recovered hash must not double-count"
        );
    }

    fn block_hashes(blocks: &[Arc<SealedBlock>]) -> Vec<BlockHash> {
        blocks
            .iter()
            .map(|block| block.header().block_hash)
            .collect()
    }

    /// The validated block extends the tip while a heavier fork from below the tip waits
    /// for validation. The tip moves without abandoning a block, so no `ReorgEvent`.
    #[tokio::test]
    async fn tip_extension_with_pending_heavier_fork_is_not_a_reorg() {
        let mut h = DiscardHarness::new();
        let fork_choice_updates = h.answer_fork_choice_updates();
        h.index_genesis();
        let genesis = h.genesis();

        // genesis -> old_tip (canonical tip)
        //         -> pending_fork_start -> pending_fork_max (heaviest, not validated)
        // old_tip -> new_tip (validated now)
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let pending_fork_start = h.add_child_block(&genesis, 5_050, true);
        let _pending_fork_max = h.add_child_block(&pending_fork_start, 5_300, false);
        let new_tip = h.add_child_block(&old_tip, 5_200, true);

        h.inner
            .on_block_validation_finished(new_tip.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");

        assert_eq!(
            h.tip(),
            new_tip.block_hash,
            "the tip moves to the new block"
        );
        let event = h.reorg_rx.try_recv();
        assert!(
            event.is_err(),
            "a tip extension abandons no block and must not broadcast a ReorgEvent, got {:?}",
            event.map(|event| block_hashes(&event.old_fork))
        );
        h.assert_last_fork_choice_follows_tip(&fork_choice_updates);
    }

    /// The validated block is on a sibling fork of the tip, lighter than a pending fork
    /// that extends the tip. The tip moves to the sibling fork and abandons the old tip,
    /// so a `ReorgEvent` must report the old tip as orphaned.
    #[tokio::test]
    async fn tip_move_to_sibling_fork_below_pending_max_is_a_reorg() {
        let mut h = DiscardHarness::new();
        let fork_choice_updates = h.answer_fork_choice_updates();
        h.index_genesis();
        let genesis = h.genesis();

        // genesis -> old_tip (canonical tip) -> pending_start -> pending_max (heaviest)
        //         -> new_tip (validated now, heavier than old_tip)
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let pending_start = h.add_child_block(&old_tip, 5_200, true);
        let _pending_max = h.add_child_block(&pending_start, 5_400, false);
        let new_tip = h.add_child_block(&genesis, 5_150, true);

        h.inner
            .on_block_validation_finished(new_tip.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");

        assert_eq!(
            h.tip(),
            new_tip.block_hash,
            "the tip moves to the sibling fork"
        );
        let event = h
            .reorg_rx
            .try_recv()
            .expect("a tip move that abandons the old tip must broadcast a ReorgEvent");
        assert_eq!(block_hashes(&event.old_fork), vec![old_tip.block_hash]);
        assert_eq!(block_hashes(&event.new_fork), vec![new_tip.block_hash]);
        assert_eq!(event.fork_parent.block_hash, genesis.block_hash);
        assert_eq!(event.new_tip, new_tip.block_hash);
        h.assert_last_fork_choice_follows_tip(&fork_choice_updates);
    }

    /// The tip moves two blocks at once: an exact cumulative difficulty tie with a
    /// pending fork kept the tip when `first_new_block` was validated, and its child
    /// is validated now. The tip move abandons no block, so no `ReorgEvent`, but both
    /// new blocks join the canonical chain: each one is confirmed, and the epoch block
    /// among them is detected.
    #[tokio::test]
    async fn tip_extension_by_two_blocks_confirms_each_new_block() {
        let mut h = DiscardHarness::new_with_config(|c| {
            c.consensus.get_mut().epoch.num_blocks_in_epoch = 2;
        });
        let fork_choice_updates = h.answer_fork_choice_updates();
        h.index_genesis();
        let genesis = h.genesis();

        // genesis -> old_tip (canonical tip) -> first_new_block (epoch block) -> new_tip
        //         -> pending_fork_start -> pending_fork_max (heaviest, not validated)
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let pending_fork_start = h.add_child_block(&genesis, 5_200, true);
        let first_new_block = h.add_child_block(&old_tip, 5_200, true);
        // Epoch block detection maps the epoch snapshot's storage modules, which needs
        // a storage submodule config.
        h.inner
            .cache
            .write()
            .expect("cache lock for epoch snapshot")
            .blocks
            .get_mut(&first_new_block.block_hash)
            .expect("first new block in cache")
            .epoch_snapshot = Arc::new(irys_domain::EpochSnapshot {
            storage_submodules_config: Some(irys_config::StorageSubmodulesConfig {
                is_using_hardcoded_paths: false,
                submodule_paths: vec![],
            }),
            ..Default::default()
        });
        h.inner
            .on_block_validation_finished(first_new_block.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");
        assert_eq!(
            h.tip(),
            old_tip.block_hash,
            "a tie with the max cumulative difficulty block keeps the tip"
        );

        let _pending_fork_max = h.add_child_block(&pending_fork_start, 5_400, false);
        let new_tip = h.add_child_block(&first_new_block, 5_300, true);
        h.inner
            .on_block_validation_finished(new_tip.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");

        assert_eq!(
            h.tip(),
            new_tip.block_hash,
            "the tip moves to the new block"
        );
        let event = h.reorg_rx.try_recv();
        assert!(
            event.is_err(),
            "a tip extension abandons no block and must not broadcast a ReorgEvent, got {:?}",
            event.map(|event| block_hashes(&event.old_fork))
        );

        let mut confirmed = vec![];
        while let Ok(message) = h.mempool_rx.try_recv() {
            if let (MempoolServiceMessage::BlockConfirmed(block), _span) = message.into_parts() {
                confirmed.push(block.header().block_hash);
            }
        }
        assert_eq!(
            confirmed,
            vec![first_new_block.block_hash, new_tip.block_hash],
            "each new canonical block is confirmed, oldest first"
        );

        let mut epoch_heights = vec![];
        while let Ok(message) = h.storage_modules_rx.try_recv() {
            let (
                StorageModuleServiceMessage::PartitionAssignmentsUpdated { update_height, .. },
                _span,
            ) = message.into_parts()
            else {
                panic!("block tree sends only partition assignment updates");
            };
            epoch_heights.push(update_height);
        }
        assert_eq!(
            epoch_heights,
            vec![first_new_block.height],
            "the epoch block among the new canonical blocks is detected"
        );
        h.assert_last_fork_choice_follows_tip(&fork_choice_updates);
    }

    /// A tip extension whose fork point is below the post-prune cache window aborts
    /// before prune, as a deep reorg does: prune would evict the oldest new block before
    /// it is confirmed. With `block_tree_depth = 2`, a two-block extension is one block
    /// too long.
    #[tokio::test]
    async fn tip_extension_past_cache_window_aborts_before_prune() {
        let mut h = DiscardHarness::new_with_config(|c| {
            c.consensus.get_mut().block_tree_depth = 2;
        });
        h.answer_fork_choice_updates();
        h.index_genesis();
        let genesis = h.genesis();

        // genesis -> old_tip (canonical tip) -> first_new_block -> new_tip
        //         -> pending_fork_start (ties first_new_block, added first)
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let _pending_fork_start = h.add_child_block(&genesis, 5_200, true);
        let first_new_block = h.add_child_block(&old_tip, 5_200, true);
        h.inner
            .on_block_validation_finished(first_new_block.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");
        assert_eq!(
            h.tip(),
            old_tip.block_hash,
            "a tie with the max cumulative difficulty block keeps the tip"
        );

        let new_tip = h.add_child_block(&first_new_block, 5_300, true);
        let err = h
            .inner
            .on_block_validation_finished(new_tip.block_hash, ValidationResult::Valid)
            .await
            .expect_err("a tip extension past the cache window must abort");
        assert!(
            err.to_string().contains("deeper than cache window"),
            "error must name the cache-window violation; got: {err}"
        );

        let cache = h.inner.cache.read().expect("cache lock after abort");
        assert!(
            cache.get_block(&first_new_block.block_hash).is_some(),
            "the abort happens before prune evicts the oldest new block"
        );
        drop(cache);
        while let Ok(message) = h.mempool_rx.try_recv() {
            let (message, _span) = message.into_parts();
            assert!(
                !matches!(message, MempoolServiceMessage::BlockConfirmed(_)),
                "no block is confirmed when the tip change aborts"
            );
        }
        assert!(
            h.reorg_rx.try_recv().is_err(),
            "no ReorgEvent is sent when the tip change aborts"
        );
    }

    /// Two children of the tip have the same cumulative difficulty. The child added
    /// first is the max block, so the other child keeps the tip when it is validated.
    /// When the max block is invalid, the validated child is the heaviest valid block.
    /// It must become the tip and be confirmed, so that the block producer can build
    /// on the max block.
    #[tokio::test]
    async fn invalid_max_block_after_refused_tie_moves_tip_to_valid_sibling() {
        let mut h = DiscardHarness::new();
        h.answer_fork_choice_updates();
        h.index_genesis();

        // genesis -> old_tip (canonical tip) -> invalid_max_block (added first)
        //                                    -> valid_sibling (same cumulative difficulty)
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let invalid_max_block = h.add_child_block(&old_tip, 5_200, true);
        let valid_sibling = h.add_child_block(&old_tip, 5_200, true);
        h.inner
            .on_block_validation_finished(valid_sibling.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");
        assert_eq!(
            h.tip(),
            old_tip.block_hash,
            "a tie with the max cumulative difficulty block keeps the tip"
        );

        let invalid_result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        h.inner
            .on_block_validation_finished(invalid_max_block.block_hash, invalid_result)
            .await
            .expect("validation result handled");

        let cache = h.inner.cache.read().expect("cache lock after discard");
        assert!(cache.get_block(&invalid_max_block.block_hash).is_none());
        assert_eq!(
            cache.get_max_cumulative_difficulty_block().1,
            valid_sibling.block_hash,
            "the validated sibling is the max block"
        );
        assert_eq!(
            cache.tip, valid_sibling.block_hash,
            "the heaviest valid block becomes the tip"
        );
        assert!(
            cache.can_be_built_upon(&valid_sibling.block_hash),
            "the block producer can build on the max block"
        );
        drop(cache);
        assert_eq!(
            h.take_confirmed_blocks(),
            vec![valid_sibling.block_hash],
            "the new tip is confirmed"
        );
    }

    /// Same start as `invalid_max_block_after_refused_tie_moves_tip_to_valid_sibling`,
    /// with a third tied child added after the max block and before the validated
    /// sibling. When the max block is invalid, the pending tie is the max block, so the
    /// discard leaves the tip, sends no fork-choice update and confirms no block.
    #[tokio::test]
    async fn discard_with_pending_max_block_keeps_tip() {
        let mut h = DiscardHarness::new();
        let fork_choice_updates = h.answer_fork_choice_updates();
        h.index_genesis();

        // genesis -> old_tip (canonical tip) -> invalid_max_block (added first)
        //                                    -> pending_tie (added second, not validated)
        //                                    -> valid_sibling (validated)
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let invalid_max_block = h.add_child_block(&old_tip, 5_200, true);
        let pending_tie = h.add_child_block(&old_tip, 5_200, true);
        let valid_sibling = h.add_child_block(&old_tip, 5_200, true);
        h.inner
            .on_block_validation_finished(valid_sibling.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");

        let invalid_result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        h.inner
            .on_block_validation_finished(invalid_max_block.block_hash, invalid_result)
            .await
            .expect("validation result handled");

        let cache = h.inner.cache.read().expect("cache lock after discard");
        assert_eq!(
            cache.get_max_cumulative_difficulty_block().1,
            pending_tie.block_hash,
            "the pending tie is the max block"
        );
        assert_eq!(cache.tip, old_tip.block_hash, "the tip does not move");
        drop(cache);
        assert!(
            fork_choice_updates
                .lock()
                .expect("fork-choice recorder")
                .is_empty(),
            "no fork-choice update is sent"
        );
        assert!(
            h.take_confirmed_blocks().is_empty(),
            "no block is confirmed"
        );
        assert!(h.reorg_rx.try_recv().is_err(), "no ReorgEvent is sent");
    }

    /// Same start as `invalid_max_block_after_refused_tie_moves_tip_to_valid_sibling`.
    /// A child of the validated sibling that is validated after the discard moves the
    /// tip by two blocks, and both new blocks are confirmed.
    #[tokio::test]
    async fn child_of_valid_sibling_moves_tip_after_invalid_max_block() {
        let mut h = DiscardHarness::new();
        h.answer_fork_choice_updates();
        h.index_genesis();

        // genesis -> old_tip (canonical tip) -> invalid_max_block (added first)
        //                                    -> valid_sibling -> new_tip
        let old_tip = make_cache_tip(&mut h, IrysBlockHeader::new_mock_header(), 5_100);
        let invalid_max_block = h.add_child_block(&old_tip, 5_200, true);
        let valid_sibling = h.add_child_block(&old_tip, 5_200, true);
        h.inner
            .on_block_validation_finished(valid_sibling.block_hash, ValidationResult::Valid)
            .await
            .expect("validation result handled");
        let invalid_result: ValidationResult =
            crate::block_validation::ValidationError::ShadowTransactionInvalid("bad".into()).into();
        h.inner
            .on_block_validation_finished(invalid_max_block.block_hash, invalid_result)
            .await
            .expect("validation result handled");

        let new_tip = h
            .add_validated_chain(&valid_sibling, &[5_300])
            .await
            .remove(0);

        assert_eq!(
            h.tip(),
            new_tip.block_hash,
            "the tip moves to the new block"
        );
        assert!(
            h.inner
                .cache
                .read()
                .expect("cache lock for tip state")
                .can_be_built_upon(&new_tip.block_hash),
            "the block producer can build on the new tip"
        );
        let event = h.reorg_rx.try_recv();
        assert!(
            event.is_err(),
            "a tip extension abandons no block and must not broadcast a ReorgEvent, got {:?}",
            event.map(|event| block_hashes(&event.old_fork))
        );
        assert_eq!(
            h.take_confirmed_blocks(),
            vec![valid_sibling.block_hash, new_tip.block_hash],
            "each new canonical block is confirmed, oldest first"
        );
    }

    /// A reorg through the service with `block_tree_depth = 3`. The post-prune cache
    /// window starts at `new_tip.height - (block_tree_depth - 1)`. A fork point at the
    /// start of the window completes the reorg. A fork point one block below it aborts
    /// before prune, so that no fork block is evicted and nothing is sent downstream.
    #[rstest]
    #[case::fork_point_at_window_start(2, true)]
    #[case::fork_point_one_below_window(3, false)]
    #[tokio::test]
    async fn reorg_fork_point_at_cache_window_boundary(
        #[case] new_fork_len: usize,
        #[case] reorg_completes: bool,
    ) {
        let mut h = DiscardHarness::new_with_config(|c| {
            c.consensus.get_mut().block_tree_depth = 3;
        });
        h.answer_fork_choice_updates();
        h.index_genesis();
        let genesis = h.genesis();

        // genesis -> fork_point (height 1) -> old_tip (canonical tip)
        //                                  -> new fork, new_fork_len blocks
        let old_chain = h.add_validated_chain(&genesis, &[5_100, 5_200]).await;
        let (fork_point, old_tip) = (&old_chain[0], &old_chain[1]);
        h.take_confirmed_blocks();

        // Each new block is lighter than the old tip, except the last one.
        let mut new_fork = h
            .add_validated_chain(fork_point, &vec![5_150; new_fork_len - 1])
            .await;
        new_fork.push(h.add_child_block(new_fork.last().expect("new fork block"), 5_300, true));
        let new_tip = new_fork.last().expect("new tip").clone();
        let new_fork_hashes: Vec<BlockHash> =
            new_fork.iter().map(|block| block.block_hash).collect();
        let result = h
            .inner
            .on_block_validation_finished(new_tip.block_hash, ValidationResult::Valid)
            .await;

        if reorg_completes {
            result.expect("a fork point at the start of the cache window must not abort");
            assert_eq!(h.tip(), new_tip.block_hash, "the tip moves to the new fork");
            let event = h
                .reorg_rx
                .try_recv()
                .expect("a tip move that abandons the old tip must broadcast a ReorgEvent");
            assert_eq!(block_hashes(&event.old_fork), vec![old_tip.block_hash]);
            assert_eq!(block_hashes(&event.new_fork), new_fork_hashes);
            assert_eq!(event.fork_parent.block_hash, fork_point.block_hash);
            assert_eq!(
                h.take_confirmed_blocks(),
                new_fork_hashes,
                "each new canonical block is confirmed, oldest first"
            );
        } else {
            let err = result.expect_err("a fork point below the cache window must abort");
            assert!(
                err.to_string().contains("deeper than cache window"),
                "error must name the cache-window violation; got: {err}"
            );
            let cache = h.inner.cache.read().expect("cache lock after abort");
            for block in new_fork.iter().chain(&old_chain) {
                assert!(
                    cache.get_block(&block.block_hash).is_some(),
                    "the abort happens before prune evicts block {}",
                    block.height
                );
            }
            drop(cache);
            assert!(
                h.reorg_rx.try_recv().is_err(),
                "no ReorgEvent is sent when the tip change aborts"
            );
            assert_eq!(
                h.take_confirmed_blocks(),
                Vec::<BlockHash>::new(),
                "no block is confirmed when the tip change aborts"
            );
        }
    }

    /// A reorg through the service with `block_migration_depth = 2`. Migration keeps
    /// the block index `block_migration_depth` blocks behind the tip, so only a reorg
    /// deeper than `block_migration_depth` abandons a migrated block. That reorg rolls
    /// the block index back to the fork point before the new fork is migrated. In each
    /// case, after the reorg the block index holds only blocks of the new canonical
    /// chain.
    #[rstest]
    #[case::depth_equals_migration_depth(2)]
    #[case::depth_one_past_migration_depth(3)]
    #[tokio::test]
    async fn reorg_around_migration_depth_keeps_block_index_canonical(#[case] reorg_depth: usize) {
        const MIGRATION_DEPTH: usize = 2;
        let mut h = DiscardHarness::new_with_config(|c| {
            let consensus = c.consensus.get_mut();
            consensus.block_migration_depth = MIGRATION_DEPTH as u32;
            consensus.block_tree_depth = 10;
        });
        // The block index roll back reads the storage modules; this node has none.
        h.inner
            .block_migration_service
            .set_storage_modules_guard(StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![]))));
        h.answer_fork_choice_updates();
        h.index_genesis();
        let genesis = h.genesis();

        // genesis -> old_1 -> old_2 -> old_3 -> old_4 (canonical tip)
        let old_chain = h
            .add_validated_chain(&genesis, &[5_100, 5_200, 5_300, 5_400])
            .await;
        h.take_confirmed_blocks();
        let mut canonical: Vec<BlockHash> = std::iter::once(&genesis)
            .chain(&old_chain)
            .map(|block| block.block_hash)
            .collect();
        assert_eq!(
            h.indexed_block_hashes(),
            canonical[..=old_chain.len() - MIGRATION_DEPTH],
            "migration keeps the block index block_migration_depth blocks behind the tip"
        );

        // The new fork starts reorg_depth blocks below the old tip and ends one block
        // above it. Each new block is lighter than the old tip, except the last one.
        let fork_height = old_chain.len() - reorg_depth;
        let fork_point = if fork_height == 0 {
            &genesis
        } else {
            &old_chain[fork_height - 1]
        };
        let mut new_fork = h
            .add_validated_chain(fork_point, &vec![5_150; reorg_depth])
            .await;
        new_fork.push(h.add_child_block(new_fork.last().expect("new fork block"), 5_500, true));
        let new_tip = new_fork.last().expect("new tip").clone();
        h.inner
            .on_block_validation_finished(new_tip.block_hash, ValidationResult::Valid)
            .await
            .expect("a reorg within the cache window completes");

        let event = h
            .reorg_rx
            .try_recv()
            .expect("a tip move that abandons the old tip must broadcast a ReorgEvent");
        assert_eq!(
            block_hashes(&event.old_fork),
            canonical[fork_height + 1..],
            "the old fork is reorg_depth blocks long"
        );
        canonical.truncate(fork_height + 1);
        canonical.extend(new_fork.iter().map(|block| block.block_hash));
        assert_eq!(block_hashes(&event.new_fork), canonical[fork_height + 1..]);

        let new_migration_height = new_tip.height as usize - MIGRATION_DEPTH;
        assert_eq!(
            h.indexed_block_hashes(),
            canonical[..=new_migration_height],
            "the block index holds only blocks of the new canonical chain"
        );
    }

    /// Regression coverage for the warn/error split in
    /// `discard_and_broadcast`. Before the split, every discard arm logged
    /// at `error!`, inflating alert noise on healthy nodes that hit eviction
    /// races (SoftInternal is intentionally non-alarming — passive recovery
    /// via the soft-internal LRU + re-gossip). Without these tests, a future
    /// refactor that re-flattens both arms to a single level would silently
    /// re-trigger that false-positive alert load.
    ///
    /// Tests bind to the level emitted by `emit_discard_log` (the helper
    /// extracted from `discard_and_broadcast` so the split is unit-testable
    /// without standing up the full block-tree service).
    mod log_level_split_tests {
        use super::*;

        /// SoftInternal discards MUST log at `WARN` and MUST NOT log at
        /// `ERROR`. A regression that flattens the split to a single
        /// `error!` will fail the second assertion.
        #[tracing_test::traced_test]
        #[test]
        fn soft_internal_discard_logs_at_warn() {
            emit_discard_log(
                DiscardKind::SoftInternal,
                H256([0x11; 32]),
                "execution payload cache evicted",
                "block validation hit an internal failure (soft race); removing block from cache, fresh gossip can retry",
            );

            assert!(
                logs_contain("WARN"),
                "SoftInternal discard must emit at WARN level"
            );
            assert!(
                !logs_contain("ERROR"),
                "SoftInternal discard must NOT emit at ERROR level — that would re-trigger the false-positive alert load that motivated the warn/error split"
            );
        }

        /// Invalid (consensus-rejected, peer-attributable) discards MUST log
        /// at `ERROR` and MUST NOT log at `WARN`. A regression that flattens
        /// the split to a single `warn!` will fail the second assertion;
        /// downgrading genuine peer rejections to WARN would hide a real
        /// peer-attribution signal from `error!`-rate alerts.
        #[tracing_test::traced_test]
        #[test]
        fn invalid_discard_logs_at_error() {
            emit_discard_log(
                DiscardKind::Invalid,
                H256([0x22; 32]),
                "shadow transaction invalid",
                "block validation failed",
            );

            assert!(
                logs_contain("ERROR"),
                "Invalid discard must emit at ERROR level"
            );
            assert!(
                !logs_contain("WARN"),
                "Invalid discard must NOT emit at WARN level — peer-attributable rejections need to fire ERROR alerts"
            );
        }
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[test]
    fn last_block_at_updates_on_canonical_advance() {
        let ts = Arc::new(BlockTreeLifecycleTimestamps::default());
        assert_eq!(ts.last_block_at_ms(), 0, "starts unset");
        ts.note_canonical_advance();
        let after = ts.last_block_at_ms();
        let now = now_ms();
        assert!(
            after > 0 && (now - after).abs() < 5_000,
            "should be ~now (got {after}, expected ~{now})"
        );
    }

    #[test]
    fn last_reorg_at_updates_on_reorg() {
        let ts = Arc::new(BlockTreeLifecycleTimestamps::default());
        assert_eq!(ts.last_reorg_at_ms(), 0);
        ts.note_reorg();
        assert!(ts.last_reorg_at_ms() > 0);
    }
}
