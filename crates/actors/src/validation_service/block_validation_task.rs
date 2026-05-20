//! Block validation task execution module.
//!
//! Handles individual block validation through a two-stage pipeline:
//!
//! ## Stage 1: VDF validation (execute_vdf)
//! - **VDF**: Verifies VDF steps are valid & fast-forwards the node if they are
//! Uses a single preemptible task slot to prevent thread overutilisation
//!
//! ## Stage 2: Concurrent Validation (execute_concurrent)
//! Six concurrent validation stages:
//! - **Recall Range**: Async data recall and storage proof verification
//! - **POA**: Blocking cryptographic proof-of-access validation
//! - **Shadow Transactions**: Async Reth integration validation
//! - **Seeds**: Validates VDF seed data
//! - **Commitment Ordering**: Validates commitment transaction ordering
//! - **Data Transaction Fees**: Validates data transaction fees using block's EMA
//!
//! ## Stage 3: Parent Dependency Resolution
//! After successful validation, tasks wait for parent block validation using
//! cooperative yielding. Tasks are cancelled if too far behind canonical tip.

use crate::block_tree_service::ValidationResult;
use crate::block_validation::{
    SubmitPayloadError, ValidationCancelReason, ValidationError, commitment_txs_are_valid,
    data_txs_are_valid, is_seed_data_valid, poa_is_valid, recall_recall_range_is_valid,
    shadow_transactions_are_valid, submit_payload_to_reth,
};
use crate::metrics;
use crate::validation_service::ValidationServiceInner;
use irys_domain::{BlockState, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, SealedBlock, SystemLedger, UnixTimestampMs};
use std::ops::ControlFlow;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::task::AbortHandle;
use tracing::{Instrument as _, debug, error, warn};

/// Result of waiting for parent validation to complete
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentValidationResult {
    /// Parent validation is complete, task can proceed
    Ready,
    /// Task should be cancelled. The reason carries through to
    /// `ValidationError::ValidationCancelled` so that `is_internal_failure`
    /// can distinguish "block too old, discard" from "local race, retry".
    Cancelled(ValidationCancelReason),
}

/// Handles the execution of a single block validation task
#[derive(Clone)]
pub(super) struct BlockValidationTask {
    pub sealed_block: Arc<SealedBlock>,
    pub service_inner: Arc<ValidationServiceInner>,
    pub block_tree_guard: BlockTreeReadGuard,
    pub skip_vdf_validation: bool,
    pub parent_span: tracing::Span,
    /// When the task first entered the validation queue. Preserved across
    /// preemption/requeue so queue-age metrics reflect total waiting time.
    pub enqueued_at: Instant,
}

impl PartialEq for BlockValidationTask {
    fn eq(&self, other: &Self) -> bool {
        self.sealed_block.header().block_hash == other.sealed_block.header().block_hash
    }
}

impl Eq for BlockValidationTask {}

impl std::hash::Hash for BlockValidationTask {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(self.sealed_block.header().block_hash.as_bytes());
    }
}

impl PartialOrd for BlockValidationTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BlockValidationTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Ordering is handled by ValidationPriority, this is
        // just to satisfy PriorityQueue requirement for the value to be Ord
        self.sealed_block
            .header()
            .block_hash
            .cmp(&other.sealed_block.header().block_hash)
    }
}

/// Check if the parent is ready for this block to be reported.
///
/// Parent validation must be fully complete before the child is reported,
/// so `Validated(Unknown)` / `Validated(ValidationScheduled)` are deliberately
/// excluded — these states match the `InTreePendingValidation` boundary in
/// `block_status_provider`, meaning the parent is in the tree but not yet validated.
fn is_parent_ready(parent_state: &ChainState) -> bool {
    matches!(
        parent_state,
        ChainState::Onchain
            | ChainState::Validated(BlockState::ValidBlock)
            | ChainState::NotOnchain(BlockState::ValidBlock)
    )
}

impl BlockValidationTask {
    pub(super) fn new(
        sealed_block: Arc<SealedBlock>,
        service_inner: Arc<ValidationServiceInner>,
        block_tree_guard: BlockTreeReadGuard,
        skip_vdf_validation: bool,
        parent_span: tracing::Span,
    ) -> Self {
        Self::new_with_enqueued_at(
            sealed_block,
            service_inner,
            block_tree_guard,
            skip_vdf_validation,
            parent_span,
            Instant::now(),
        )
    }

    /// Construct a task while preserving an existing `enqueued_at`. Used by
    /// the cancel-requeue path in `validation_service` so end-to-end
    /// validation latency metrics reflect the original gossip-to-completion
    /// span, not just the post-requeue retry.
    pub(super) fn new_with_enqueued_at(
        sealed_block: Arc<SealedBlock>,
        service_inner: Arc<ValidationServiceInner>,
        block_tree_guard: BlockTreeReadGuard,
        skip_vdf_validation: bool,
        parent_span: tracing::Span,
        enqueued_at: Instant,
    ) -> Self {
        Self {
            sealed_block,
            service_inner,
            block_tree_guard,
            skip_vdf_validation,
            parent_span,
            enqueued_at,
        }
    }

    /// Create a task suitable only for priority-queue operations in tests.
    ///
    /// The `service_inner` field contains uninitialized data — calling
    /// `execute_concurrent` or any method that reads it is undefined behavior.
    /// Only queue operations (Hash/Eq/Ord on block_hash) and `check_preemption`
    /// (reads only the associated priority, not the task) are safe.
    #[cfg(test)]
    pub(super) fn test_stub(
        sealed_block: Arc<SealedBlock>,
        block_tree_guard: BlockTreeReadGuard,
    ) -> Self {
        // SAFETY: We allocate a real ArcInner<MaybeUninit<ValidationServiceInner>>
        // (with valid refcounts), cast it to Arc<ValidationServiceInner>, and keep
        // one strong reference in ManuallyDrop so the inner data is never dropped
        // or freed.
        //
        // Layout is identical because MaybeUninit<T> has the same size/alignment as T.
        // Dropping the returned Arc only decrements the strong count back to 1, so
        // drop_in_place is never called on the uninitialized data.
        let fake_inner: Arc<ValidationServiceInner> = unsafe {
            let uninit = Arc::new(std::mem::MaybeUninit::<ValidationServiceInner>::uninit());
            let raw = Arc::into_raw(uninit) as *const ValidationServiceInner;
            let arc = Arc::from_raw(raw);
            let leaked_arc = std::mem::ManuallyDrop::new(arc);

            Arc::clone(&leaked_arc)
        };

        Self {
            sealed_block,
            service_inner: fake_inner,
            block_tree_guard,
            skip_vdf_validation: false,
            parent_span: tracing::Span::none(),
            enqueued_at: Instant::now(),
        }
    }

    /// Execute the concurrent validation task
    #[tracing::instrument(parent = &self.parent_span, skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    pub(super) async fn execute_concurrent(self) -> ValidationResult {
        let concurrent_started = Instant::now();
        let block_timestamp_ms = self.sealed_block.header().timestamp.as_millis();
        let into_cancelled_result = |reason: ValidationCancelReason| -> ValidationResult {
            tracing::warn!(
                block.hash = %self.sealed_block.header().block_hash,
                cancel.reason = %reason,
                "Validation cancelled"
            );
            ValidationError::ValidationCancelled { reason }.into()
        };

        // The height-diff / channel-closed cancellation future is now
        // consumed *inside* `validate_block` so that a `NodeFault` produced
        // by any stage in flight can win over the cancel via
        // `merge_stage_results`. Previously the outer `select` here could
        // drop `validate_block` while a stage had already produced a
        // `NodeFault`, silently demoting it to `Cancelled` →
        // `Invalid`/`InternalFailure(soft)` — which broke the central
        // "node faults must always win and panic" invariant of this branch.
        // See the doc on `validate_block` for the inner-select shape.
        //
        // The `poa_abort_slot` is still threaded through so the PoA
        // blocking task's `JoinHandle` is aborted on cancel rather than
        // detached (preserves the panic→`TaskPanicked` path and stops a
        // blocking-pool thread from leaking).
        let poa_abort_slot: Arc<OnceLock<AbortHandle>> = Arc::new(OnceLock::new());
        let validation_result = self.validate_block(Arc::clone(&poa_abort_slot)).await;

        let final_result = if matches!(validation_result, ValidationResult::Valid) {
            // Stages passed and no in-flight cancel fired: now block on
            // parent validation before reporting upward.
            let parent_wait_started = Instant::now();
            let parent_wait_outcome = self.wait_for_parent_validation().await;
            metrics::record_parent_wait_duration_ms(
                parent_wait_started.elapsed().as_secs_f64() * 1000.0,
            );
            match parent_wait_outcome {
                ParentValidationResult::Cancelled(reason) => into_cancelled_result(reason),
                ParentValidationResult::Ready => validation_result,
            }
        } else {
            validation_result
        };

        metrics::record_validation_stage_duration_ms(
            "concurrent_overall",
            concurrent_started.elapsed().as_secs_f64() * 1000.0,
        );
        // Split "cancelled" / "panicked" out of the generic "invalid" /
        // "internal_error" buckets for observability. Post-M2 audit
        // (2026-05-20) every `ValidationCancelled` sub-reason routes
        // through `InternalFailure` — none are peer-attributable — but the
        // "cancelled" label is preserved on both wrappers so dashboards
        // that historically tracked cancellations continue to work, and any
        // legacy code path that still produces an `Invalid(cancelled)`
        // labels consistently. `TaskPanicked` only routes through
        // `InternalFailure` and labels as "panicked".
        //
        // Delegates to `ValidationError::metric_label()` (which is the
        // exhaustive match — adding a new variant there produces a compile
        // error until its label is decided). The closure collapses the
        // granular per-stage labels (`node_fault`, `internal_error`,
        // `invalid`) back to the caller-supplied `default` so the overall
        // metric's label set in production dashboards is unchanged. The
        // `cancelled` / `panicked` labels remain distinct because both
        // have always been overall-metric labels for this closure.
        let label_for = |err: &ValidationError, default: &'static str| -> &'static str {
            match err.metric_label() {
                "cancelled" => "cancelled",
                "panicked" => "panicked",
                // `node_fault`, `internal_error`, `invalid` all collapse to
                // the caller-supplied default so the overall metric keeps its
                // dashboard-compatible label set (`"invalid"` when wrapped in
                // `Invalid`, `"internal_error"` when wrapped in
                // `InternalFailure`).
                _ => default,
            }
        };
        let result_label = match &final_result {
            ValidationResult::Valid => "valid",
            ValidationResult::Invalid(rejection) => label_for(rejection.err(), "invalid"),
            ValidationResult::InternalFailure(inner) => label_for(inner.err(), "internal_error"),
        };
        metrics::record_validation_result("concurrent_overall", result_label);
        if let Ok(now) = UnixTimestampMs::now() {
            let age_ms = now.as_millis().saturating_sub(block_timestamp_ms) as f64;
            metrics::record_block_age_at_validation_ms(age_ms);
        }
        final_result
    }

    /// Wait for parent validation to complete
    /// We do this because just because a block is valid internally, if it's not connected to a valid chain it's still not valid
    #[tracing::instrument(skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    async fn wait_for_parent_validation(&self) -> ParentValidationResult {
        let parent_chain_state_check =
            |parent_hash: BlockHash| match self.get_parent_chain_state(&parent_hash) {
                None => {
                    // Parent absent from tree during the wait stage. Possible
                    // causes are all local: block_pool failed to gate on the
                    // parent, depth-prune evicted it, or the soft-internal
                    // handler removed it. None are peer-attributable, so
                    // `ParentMissing` routes through `is_internal() = true` →
                    // `InternalFailure`.
                    error!(
                        block.parent_hash = %parent_hash,
                        block.hash = %self.sealed_block.header().block_hash,
                        block.height = %self.sealed_block.header().height,
                        "Parent block not found in tree during wait stage"
                    );
                    ControlFlow::Break(ParentValidationResult::Cancelled(
                        ValidationCancelReason::ParentMissing,
                    ))
                }
                Some(parent_state) if is_parent_ready(&parent_state) => {
                    debug!("Parent validation complete");
                    ControlFlow::Break(ParentValidationResult::Ready)
                }
                Some(_) => {
                    // Parent exists but not ready, wait for updates
                    ControlFlow::Continue(())
                }
            };

        self.exit_if_block_is_too_old(parent_chain_state_check)
            .await
    }

    #[tracing::instrument(skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    async fn exit_if_block_is_too_old(
        &self,
        extra_checks: impl Fn(BlockHash) -> ControlFlow<ParentValidationResult, ()>,
    ) -> ParentValidationResult {
        let parent_hash = self.sealed_block.header().previous_block_hash;

        let mut block_state_rx = self
            .service_inner
            .service_senders
            .subscribe_block_state_updates();

        loop {
            // 1. Check cancellation condition first
            if self.should_exit_due_to_height_diff() {
                let block_tree = self.block_tree_guard.read();
                let tip_hash = block_tree.tip;
                if let Some(tip_block) = block_tree.get_block(&tip_hash) {
                    let height_diff = tip_block
                        .height
                        .saturating_sub(self.sealed_block.header().height);
                    warn!(
                        block.hash = %self.sealed_block.header().block_hash,
                        block.height = %self.sealed_block.header().height,
                        block.height_diff= height_diff,
                        config.threshold = self.service_inner.config.consensus.block_tree_depth,
                        "Cancelling validation: block too far behind tip"
                    );
                }
                metrics::record_validation_cancellation("height_diff");
                return ParentValidationResult::Cancelled(ValidationCancelReason::HeightDifference);
            }

            match extra_checks(parent_hash) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(result) => {
                    if matches!(result, ParentValidationResult::Cancelled(_)) {
                        metrics::record_validation_cancellation("parent_missing");
                    }
                    return result;
                }
            }

            // 3. Wait for relevant state changes
            debug!(block.parent_hash = %parent_hash, "Waiting for parent validation");
            match block_state_rx.recv().await {
                Ok(_) => continue,
                // `broadcast::Receiver::recv` returns `Lagged` when the
                // sender outpaced this subscriber — the missed events MUST
                // NOT be conflated with `Closed`. Lagging means we skipped
                // some state updates but the channel is still live; the
                // next `recv()` will reset and continue. If we treated this
                // as `Closed` we'd return `Cancelled` → `Invalid` → remove
                // a valid block from the cache under load (broadcast queue
                // pressure while validation is slow). The poll loop's
                // initial `parent_chain_state_check` re-reads the block-tree
                // state directly, so any missed event will be picked up.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        block.parent_hash = %parent_hash,
                        block.hash = %self.sealed_block.header().block_hash,
                        missed = n,
                        "Block state broadcast lagged; re-polling parent state"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!(
                        block.parent_hash = %parent_hash,
                        block.hash = %self.sealed_block.header().block_hash,
                        block.height = %self.sealed_block.header().height,
                        "Block state channel closed while waiting for parent"
                    );
                    metrics::record_validation_cancellation("channel_closed");
                    return ParentValidationResult::Cancelled(
                        ValidationCancelReason::ChannelClosed,
                    );
                }
            }
        }
    }

    /// Check if the block should exit due to height difference from canonical tip
    fn should_exit_due_to_height_diff(&self) -> bool {
        let block_tree = self.block_tree_guard.read();
        let tip_hash = block_tree.tip;

        if let Some(tip_block) = block_tree.get_block(&tip_hash) {
            let height_diff = tip_block
                .height
                .saturating_sub(self.sealed_block.header().height);
            height_diff > self.service_inner.config.consensus.block_tree_depth
        } else {
            false
        }
    }

    /// Get the chain state of the parent block
    fn get_parent_chain_state(&self, parent_hash: &BlockHash) -> Option<ChainState> {
        let block_tree = self.block_tree_guard.read();
        block_tree
            .get_block_and_status(parent_hash)
            .map(|(_header, state)| *state)
    }

    /// Perform block validation
    #[tracing::instrument(skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    async fn validate_block(&self, poa_abort_slot: Arc<OnceLock<AbortHandle>>) -> ValidationResult {
        let skip_vdf_validation = self.skip_vdf_validation;
        let poa = self.sealed_block.header().poa.clone();
        let miner_address = self.sealed_block.header().miner_address;
        let block = self.sealed_block.header();

        // Recall range validation
        let recall_task = async move {
            let started = Instant::now();
            let outcome = recall_recall_range_is_valid(
                block,
                &self.service_inner.config.consensus,
                &self.service_inner.vdf_state,
            )
            .await;
            metrics::record_validation_stage_duration_ms(
                "recall_range",
                started.elapsed().as_secs_f64() * 1000.0,
            );
            let result: ValidationResult = match outcome {
                Ok(()) => ValidationResult::Valid,
                Err(err) => {
                    tracing::error!(
                        custom.error = ?err,
                        "recall range validation failed"
                    );
                    ValidationError::RecallRangeInvalid(err.to_string()).into()
                }
            };
            metrics::record_validation_result("recall_range", result.granular_metric_label());
            result
        }
        .instrument(tracing::info_span!("recall_range_validation", block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height));

        // Fetch parent epoch + EMA snapshots once in the outer scope under
        // a single `block_tree.read()` and hand them to the concurrent
        // tasks below. Previously each task did its own `get_*_snapshot`
        // read, giving us multiple lock acquisitions for the same parent
        // hash and multiple eviction-race classification sites. Centralising
        // the fetch means one read, one eviction-race error variant, and
        // each task can be tested with a constructed `Arc<EpochSnapshot>` /
        // `Arc<EmaSnapshot>` without needing a populated block-tree.
        //
        // This fetch must run *before* the PoA `spawn_blocking` below. Any
        // early-return path (snapshot missing) after the spawn would drop
        // the blocking `JoinHandle` — the blocking thread would keep
        // running and any panic in it would be swallowed instead of
        // surfacing as `TaskPanicked` → node-fault → abort+restart. By
        // resolving both snapshots before the spawn we guarantee the only
        // way to exit `validate_block` after the PoA task exists is
        // through the `select`/`merge_stage_results` path that observes
        // the join result.
        let (parent_epoch_snapshot, parent_ema_snapshot) = {
            let tree = self.block_tree_guard.read();
            let epoch = match tree.get_epoch_snapshot(&block.previous_block_hash) {
                Some(s) => s,
                None => {
                    tracing::error!(
                        block.parent_hash = %block.previous_block_hash,
                        "Parent epoch snapshot not found"
                    );
                    return ValidationError::ParentEpochSnapshotMissing {
                        block_hash: block.previous_block_hash,
                    }
                    .into();
                }
            };
            let ema = match tree.get_ema_snapshot(&block.previous_block_hash) {
                Some(s) => s,
                None => {
                    tracing::error!(
                        block.parent_hash = %block.previous_block_hash,
                        "Parent EMA snapshot not found"
                    );
                    return ValidationError::ParentEmaSnapshotMissing {
                        block_hash: block.previous_block_hash,
                    }
                    .into();
                }
            };
            (epoch, ema)
        };
        tracing::debug!("Using parent epoch snapshot for PoA validation");

        // POA validation
        let block_hash_for_error_log = self.sealed_block.header().block_hash;
        let block_height_for_error_log = self.sealed_block.header().height;
        let poa_task = {
            let consensus_config = self.service_inner.config.consensus.clone();
            let block_index_guard = self.service_inner.block_index_guard.clone();
            let block_tree_guard = self.block_tree_guard.clone();
            let block_hash = self.sealed_block.header().block_hash;
            let block_height = self.sealed_block.header().height;
            let parent_block_hash = self.sealed_block.header().previous_block_hash;
            // Clone the Arc for the blocking task; the non-PoA tasks below
            // get their own clones from the same single fetch.
            let poa_epoch_snapshot = Arc::clone(&parent_epoch_snapshot);
            {
                let poa_span = tracing::info_span!(
                    "poa_validation",
                    block.hash = %block_hash,
                    block.height = %block_height
                );
                let handle = tokio::task::spawn_blocking(move || {
                    let _guard = poa_span.enter();
                    if skip_vdf_validation {
                        debug!(block.hash = ?block_hash, "Skipping POA validation due to skip_vdf_validation flag");
                        return Ok(ValidationResult::Valid);
                    }
                    poa_is_valid(
                        &poa,
                        &block_index_guard,
                        &block_tree_guard,
                        parent_block_hash,
                        block_height.saturating_sub(1),
                        &poa_epoch_snapshot,
                        &consensus_config,
                        &miner_address,
                    )
                    .map(|()| ValidationResult::Valid)
                });
                // Publish the abort handle so the outer `execute_concurrent`
                // select-cancel arm can `.abort()` this blocking task
                // instead of detaching it (panics would otherwise be
                // swallowed and the blocking-pool thread would keep running
                // until the PoA work finished naturally).
                let _ = poa_abort_slot.set(handle.abort_handle());
                handle
            }
        };

        let poa_task = async move {
            let started = Instant::now();
            let res = poa_task.await;
            metrics::record_validation_stage_duration_ms(
                "poa",
                started.elapsed().as_secs_f64() * 1000.0,
            );

            let result: ValidationResult = match res {
                Ok(Ok(valid)) => valid,
                Ok(Err(e)) => {
                    tracing::error!(
                        block.hash = %block_hash_for_error_log,
                        block.height = %block_height_for_error_log,
                        custom.error = ?e,
                        "PoA validation failed"
                    );
                    e.into()
                }
                Err(err) => {
                    // H1 fix: split panic vs cancellation rather than unconditionally
                    // classifying every `JoinError` as `TaskPanicked`. The cancel arm
                    // in `execute_concurrent` calls `handle.abort()` on the PoA
                    // blocking task; if a future refactor ever `.await`s the aborted
                    // handle, the resulting `JoinError::is_cancelled()` must NOT
                    // route through the NodeFault taxonomy and trigger a supervisor
                    // restart. Mirrors the established pattern at
                    // `validation_service.rs:438-479`. See `classify_poa_join_error`
                    // for the panic/cancel split rationale.
                    if err.is_panic() {
                        tracing::error!(
                            block.hash = %block_hash_for_error_log,
                            block.height = %block_height_for_error_log,
                            custom.error = ?err,
                            "poa task panicked"
                        );
                    } else {
                        // is_cancelled() — the cancel arm fired `handle.abort()`.
                        // Log at debug; this is a deliberate, expected outcome of
                        // the height-diff / channel-closed cancel race.
                        debug!(
                            block.hash = %block_hash_for_error_log,
                            block.height = %block_height_for_error_log,
                            custom.error = ?err,
                            "poa task aborted (deliberate cancel)"
                        );
                    }
                    classify_poa_join_error(&err).into()
                }
            };
            metrics::record_validation_result("poa", result.granular_metric_label());
            result
        };

        // Shadow transaction validation (pure validation, no reth submission)
        let config = &self.service_inner.config;
        let service_senders = &self.service_inner.service_senders;

        // Get block index (convert read guard to Arc<RwLock>)
        let block_index = self.service_inner.block_index_guard.inner();

        // Each `async move` task that needs the snapshots clones the Arc up
        // front. `Arc::clone` is cheap (atomic refcount bump) and lets us
        // hand the same underlying snapshot to multiple concurrent tasks
        // without giving up the single outer-scope fetch.
        let shadow_tx_epoch_snapshot = parent_epoch_snapshot.clone();
        let data_txs_epoch_snapshot = parent_epoch_snapshot.clone();
        let data_txs_ema_snapshot = parent_ema_snapshot.clone();

        let sealed_block_for_shadow = self.sealed_block.clone();
        let shadow_tx_task = async move {
            let started = Instant::now();

            // Eviction-race: parent's commitment snapshot was present at
            // prevalidation but the in-memory window has since rotated.
            // Surfacing this as the typed `ParentCommitmentSnapshotMissing`
            // (already `is_internal_failure`, not `is_node_fault`) keeps it
            // out of the consensus-rejection bucket — previously this path
            // was bucketed into `ShadowTransactionInvalid` via the eyre
            // `?`, which misclassified an honest peer's block.
            let parent_commitment_snapshot = self
                .block_tree_guard
                .read()
                .get_commitment_snapshot(&block.previous_block_hash)
                .map_err(|_| ValidationError::ParentCommitmentSnapshotMissing {
                    block_hash: block.previous_block_hash,
                })?;

            // `wait_for_payload` returns one of two typed soft errors when
            // its in-process oneshot wiring cannot deliver the payload:
            //   - `ReceiverDisrupted` — `payload_senders` LRU evicted our
            //     slot under catch-up sync, or an explicit
            //     `remove_payload_from_cache` for the same hash.
            //   - `WaitTimeout` — the bounded
            //     `sync.execution_payload_wait_timeout_millis` elapsed
            //     before the payload arrived (peer advertised the header
            //     but never served the EVM payload). Caps the previously
            //     LRU-bounded (unbounded under low load) wait.
            // Both are local cache / wait disruption — NOT an
            // execution-layer fault — so we route them to the soft
            // `ExecutionPayloadCacheEvicted` (`is_internal_failure` true,
            // `is_node_fault` false) and let the block re-enter via
            // gossip. Diagnostic distinction lives one layer down in the
            // `ExecutionPayloadWaitError` variant; we log each variant
            // distinctly so operators can grep / count them.
            let execution_data = self
                .service_inner
                .execution_payload_provider
                .wait_for_payload(&block.evm_block_hash)
                .in_current_span()
                .await
                .map_err(|err| match err {
                    irys_domain::ExecutionPayloadWaitError::ReceiverDisrupted {
                        evm_block_hash,
                    } => {
                        tracing::warn!(
                            ?evm_block_hash,
                            "execution payload wait disrupted: receiver dropped (LRU eviction or cache removal)"
                        );
                        ValidationError::ExecutionPayloadCacheEvicted { evm_block_hash }
                    }
                    irys_domain::ExecutionPayloadWaitError::WaitTimeout {
                        evm_block_hash,
                        elapsed_ms,
                    } => {
                        tracing::warn!(
                            ?evm_block_hash,
                            elapsed_ms,
                            "execution payload wait timed out: peer advertised header but never served payload"
                        );
                        ValidationError::ExecutionPayloadCacheEvicted { evm_block_hash }
                    }
                })?;

            let result = shadow_transactions_are_valid(
                config,
                &self.block_tree_guard,
                &self.service_inner.mempool_guard,
                block,
                &self.service_inner.db,
                &execution_data,
                shadow_tx_epoch_snapshot,
                parent_commitment_snapshot,
                block_index,
                sealed_block_for_shadow.transactions(),
            )
            .instrument(tracing::info_span!(
                "shadow_tx_validation",
                block.hash = %self.sealed_block.header().block_hash,
                block.height = %self.sealed_block.header().height
            ))
            .await;
            metrics::record_validation_stage_duration_ms(
                "shadow_tx",
                started.elapsed().as_secs_f64() * 1000.0,
            );
            match result.as_ref() {
                Ok(_) => metrics::record_validation_result("shadow_tx", "valid"),
                Err(err) => {
                    // Use the variant-specific label so `ShadowTxNodeFault`,
                    // `ParentCommitmentSnapshotMissing`,
                    // `ExecutionPayloadCacheEvicted`,
                    // `ShadowTxGenerationFailed`, etc. don't inflate the
                    // peer-attributable "invalid" counter. The overall
                    // metric still maps these back to the dashboard label
                    // set via `label_for`.
                    metrics::record_validation_result("shadow_tx", err.metric_label());
                    tracing::error!(
                        custom.error = ?err,
                        "shadow transaction validation failed"
                    );
                }
            }
            // `shadow_transactions_are_valid` already returns a typed
            // `ValidationError`. Propagate it unchanged so the outer
            // dispatcher (`err.into()`) routes each variant correctly:
            // internal-failure variants (`ParentBlockMissing`,
            // `ShadowTxGenerationFailed`, `ShadowTxNodeFault`,
            // `ParentEpochSnapshotMissing`, etc.) reach `InternalFailure`
            // (and node-fault variants trigger abort+restart), while
            // genuine consensus mismatches (`ShadowTransactionInvalid`)
            // reach `Invalid`. Wrapping every error here as
            // `ShadowTransactionInvalid` would erase that classification
            // and peer-attribute local DB/snapshot failures.
            result.map(|()| execution_data)
        };

        let vdf_reset_frequency = self.service_inner.config.vdf.reset_frequency as u64;
        let seeds_block_hash = self.sealed_block.header().block_hash;
        let seeds_block_height = self.sealed_block.header().height;
        let seeds_validation_task = async move {
            let started = Instant::now();
            let binding = self.block_tree_guard.read();
            let previous_block =
                match binding.get_block(&self.sealed_block.header().previous_block_hash) {
                    Some(block) => block,
                    None => {
                        metrics::record_validation_stage_duration_ms(
                            "seeds",
                            started.elapsed().as_secs_f64() * 1000.0,
                        );
                        let result: ValidationResult = ValidationError::ParentBlockMissing {
                            block_hash: self.sealed_block.header().previous_block_hash,
                        }
                        .into();
                        metrics::record_validation_result("seeds", result.granular_metric_label());
                        tracing::error!(
                            block.parent_hash = %self.sealed_block.header().previous_block_hash,
                            "Previous block not found in block tree"
                        );
                        return result;
                    }
                };
            let outcome = is_seed_data_valid(
                self.sealed_block.header(),
                previous_block,
                vdf_reset_frequency,
            );
            metrics::record_validation_stage_duration_ms(
                "seeds",
                started.elapsed().as_secs_f64() * 1000.0,
            );
            metrics::record_validation_result("seeds", outcome.granular_metric_label());
            outcome
        }
        .instrument(tracing::info_span!(
            "seeds_validation",
            block.hash = %seeds_block_hash,
            block.height = %seeds_block_height
        ));

        // Commitment transaction ordering validation
        let sealed_block_for_commitment = self.sealed_block.clone();
        let commitment_ordering_task = async move {
            let started = Instant::now();
            let outcome = commitment_txs_are_valid(
                config,
                block,
                &self.block_tree_guard,
                sealed_block_for_commitment
                    .transactions()
                    .get_ledger_system_txs(SystemLedger::Commitment),
            )
            .instrument(tracing::info_span!("commitment_ordering_validation"))
            .await;
            metrics::record_validation_stage_duration_ms(
                "commitment_ordering",
                started.elapsed().as_secs_f64() * 1000.0,
            );
            let result: ValidationResult = match outcome {
                Ok(()) => ValidationResult::Valid,
                Err(err) => {
                    tracing::error!(
                        custom.error = ?err,
                        "commitment ordering validation failed"
                    );
                    err.into()
                }
            };
            metrics::record_validation_result(
                "commitment_ordering",
                result.granular_metric_label(),
            );
            result
        };

        // Data transaction fee validation. Snapshots were cloned up-front
        // alongside the shadow_tx clones (see comment above shadow_tx_task).
        let sealed_block_for_data = self.sealed_block.clone();
        let data_txs_validation_task = async move {
            let started = Instant::now();
            let txs = sealed_block_for_data.transactions();
            let outcome = data_txs_are_valid(
                config,
                service_senders,
                block,
                &self.service_inner.db,
                &self.block_tree_guard,
                txs,
                data_txs_epoch_snapshot,
                data_txs_ema_snapshot,
            )
            .instrument(tracing::info_span!(
                "data_txs_validation",
                block.hash = %self.sealed_block.header().block_hash,
                block.height = %self.sealed_block.header().height
            ))
            .await;
            metrics::record_validation_stage_duration_ms(
                "data_txs",
                started.elapsed().as_secs_f64() * 1000.0,
            );
            let result: ValidationResult = match outcome {
                Ok(()) => ValidationResult::Valid,
                Err(e) => {
                    tracing::error!(
                        custom.error = ?e,
                        "data transaction validation failed"
                    );
                    e.into()
                }
            };
            metrics::record_validation_result("data_txs", result.granular_metric_label());
            result
        };

        // Race the six concurrent stages against the height-diff /
        // channel-closed cancellation future. The cancel branch deliberately
        // does NOT drop the in-flight stages — instead it captures the
        // cancellation reason and falls through to a single drain-and-merge
        // step so that any stage that already produced a `NodeFault`
        // (e.g. `TaskPanicked`, `ShadowTxNodeFault`) is observed and wins
        // over the cancellation via `merge_stage_results_with_cancel`'s
        // priority. Before the fix, this race lived in the OUTER
        // `execute_concurrent` select and would drop `validate_block` —
        // silently demoting an already-produced node fault to a cancellation
        // outcome, breaking the "node faults must always win" invariant.
        //
        // The join future is boxed so it is `Unpin` and can be drained via
        // `futures::poll!` after the cancel arm fires.
        let stages_join = async {
            tokio::join!(
                recall_task,
                poa_task,
                shadow_tx_task,
                seeds_validation_task,
                commitment_ordering_task,
                data_txs_validation_task
            )
        };
        let mut stages_join = Box::pin(stages_join);
        let cancel_future = self.exit_if_block_is_too_old(|_| ControlFlow::Continue(()));

        // `biased;` is defensive — if both branches are simultaneously
        // ready we prefer the stages so a NodeFault that arrived
        // alongside the cancel signal still wins. The drain-poll below
        // makes this practically redundant (cancel arm also picks up
        // ready stages), but biasing is cheap insurance.
        let stage_outcome = tokio::select! {
            biased;
            joined = &mut stages_join => StageOutcome::Joined(joined),
            cancel = cancel_future => {
                // Cancel won. Fire the PoA abort so the blocking-pool
                // thread doesn't continue and so any panic surfaces via
                // the `JoinError` path (rather than being detached and
                // silently swallowed).
                if let Some(handle) = poa_abort_slot.get() {
                    handle.abort();
                }
                // Drain the join future ONCE. If any stage was already
                // ready (in particular any NodeFault), the join completes
                // here and we merge it with the cancel signal. If not,
                // the cancel is the only signal and the stages are
                // dropped below (their data is owned in this scope so
                // dropping is safe — the PoA blocking thread has already
                // been aborted above).
                match futures::poll!(&mut stages_join) {
                    std::task::Poll::Ready(joined) => StageOutcome::JoinedAfterCancel(joined, cancel),
                    std::task::Poll::Pending => StageOutcome::Cancelled(cancel),
                }
            }
        };

        // Unpack: either we have a six-tuple of stage results (with or
        // without a concurrent cancel), or we have only a cancel signal.
        let (
            recall_result,
            poa_result,
            shadow_tx_result,
            seeds_validation_result,
            commitment_ordering_result,
            data_txs_result,
            cancel_signal,
        ) = match stage_outcome {
            StageOutcome::Joined(joined) => {
                let (a, b, c, d, e, f) = joined;
                (a, b, c, d, e, f, None)
            }
            StageOutcome::JoinedAfterCancel(joined, cancel) => {
                let (a, b, c, d, e, f) = joined;
                (a, b, c, d, e, f, Some(cancel))
            }
            StageOutcome::Cancelled(cancel) => {
                // No stage was ready. Cancel is the result. Drop the
                // (still-pending) stages by letting `stages_join` fall
                // out of scope. The PoA blocking thread was aborted in
                // the select arm above.
                return cancel_outcome_to_result(cancel);
            }
        };

        // shadow_tx_task returns a typed `ValidationError` on failure;
        // route through `.into()` so eviction races / node-fault paths
        // (`ParentCommitmentSnapshotMissing`, `ExecutionPayloadCacheEvicted`)
        // dispatch to `InternalFailure` and genuine consensus mismatches
        // (`ShadowTransactionInvalid`) dispatch to `Invalid`. The success
        // side carries `execution_data` for the reth-submission stage —
        // keep it separately so shadow_tx still participates in the
        // multi-stage `Invalid`-over-`InternalFailure` merger below.
        // (Previously the failure path early-returned and bypassed the
        // merger, which could leave a known-bad block in cache when shadow
        // hit an internal failure while another stage proved the block
        // bad.)
        let (shadow_tx_result, execution_data) = match shadow_tx_result {
            Ok(data) => (ValidationResult::Valid, Some(data)),
            Err(err) => {
                tracing::error!(custom.error = ?err, "Shadow transaction validation failed, not submitting to reth");
                (err.into(), None)
            }
        };

        // If cancellation fired while stages were running, we MUST go
        // through the merger so any concurrent NodeFault wins over the
        // cancel signal. Otherwise we hand the stages to the existing
        // success / failure logic.
        if let Some(cancel) = cancel_signal {
            let stage_results = [
                &recall_result,
                &poa_result,
                &shadow_tx_result,
                &seeds_validation_result,
                &commitment_ordering_result,
                &data_txs_result,
            ];
            let cancel_result = cancel_outcome_to_result(cancel);
            return merge_stage_results_with_cancel(&stage_results, Some(&cancel_result));
        }

        match (
            &recall_result,
            &poa_result,
            &shadow_tx_result,
            &seeds_validation_result,
            &commitment_ordering_result,
            &data_txs_result,
        ) {
            (
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
            ) => {
                tracing::debug!("All consensus validations successful, submitting to reth");

                // All consensus layer validations passed, now submit to
                // execution layer. `execution_data` is guaranteed Some
                // because shadow_tx_result is Valid above.
                let execution_data = execution_data
                    .expect("shadow_tx validation succeeded; execution_data must be present");
                let reth_started = Instant::now();
                let reth_result = submit_payload_to_reth(
                    self.sealed_block.header(),
                    &self.service_inner.reth_node_adapter,
                    execution_data,
                )
                .instrument(tracing::error_span!(
                    "reth_submission",
                    block.hash = %self.sealed_block.header().block_hash,
                    block.height = %self.sealed_block.header().height
                ))
                .await;
                metrics::record_validation_stage_duration_ms(
                    "reth_submission",
                    reth_started.elapsed().as_secs_f64() * 1000.0,
                );

                let result: ValidationResult = match reth_result {
                    Ok(()) => {
                        tracing::debug!("Reth execution layer validation successful");
                        ValidationResult::Valid
                    }
                    Err(err) => {
                        tracing::error!(custom.error = ?err, "Reth execution layer validation failed");
                        // Dispatch on the typed error: local transport
                        // failures are node faults (broken local EL,
                        // not the peer's block); structural and
                        // payload-rejection failures are consensus
                        // rejections.
                        match err {
                            SubmitPayloadError::LocalTransport(msg) => {
                                ValidationError::ExecutionLayerTransportFailed(msg).into()
                            }
                            SubmitPayloadError::PayloadStructure(msg)
                            | SubmitPayloadError::PayloadRejected(msg) => {
                                ValidationError::ExecutionLayerFailed(msg).into()
                            }
                        }
                    }
                };
                metrics::record_validation_result(
                    "reth_submission",
                    result.granular_metric_label(),
                );
                result
            }
            _ => {
                tracing::debug!("Consensus validation failed, not submitting to reth");
                let stage_results = [
                    &recall_result,
                    &poa_result,
                    &shadow_tx_result,
                    &seeds_validation_result,
                    &commitment_ordering_result,
                    &data_txs_result,
                ];
                merge_stage_results(&stage_results)
            }
        }
    }
}

/// Outcome of racing the six concurrent validation stages against the
/// inner cancellation future. See the call site in `validate_block` for
/// the drain-poll behavior on cancel.
enum StageOutcome<J> {
    /// Stages completed before cancellation fired.
    Joined(J),
    /// Cancellation fired but at least one drain-poll observed the join
    /// future was already ready. Both signals participate in the merge so
    /// a NodeFault can still win over the cancel.
    JoinedAfterCancel(J, ParentValidationResult),
    /// Cancellation fired and no stage had completed at the drain-poll
    /// moment. The cancel signal is the result.
    Cancelled(ParentValidationResult),
}

/// Convert a `tokio::task::JoinError` from the PoA blocking task into a
/// `ValidationError`, distinguishing panic (genuine node fault) from
/// cancellation (deliberate abort from the cancel arm).
///
/// `JoinError` has exactly two producers: `is_panic()` and `is_cancelled()`.
/// The H1 audit (2026-05-20) closed a latent landmine where every `JoinError`
/// — including aborts triggered by the cancel arm's `poa_abort_slot.abort()`
/// — was unconditionally classified as `TaskPanicked`. That misclassification
/// routes through the `NodeFault` taxonomy and triggers a supervisor panic at
/// `block_tree_service.rs:540-544`. The bug is dormant today because the
/// cancel arm drops the stages-join future after a synchronous `futures::poll!`
/// rather than awaiting the aborted handle, but any future refactor that ever
/// `.await`s a possibly-aborted handle activates it.
///
/// Mirrors the established pattern at `validation_service.rs:438-479`:
/// `is_panic()` → `TaskPanicked` (NodeFault → abort + restart);
/// otherwise → `ValidationCancelled` (SoftInternal → park-and-retry).
///
/// The `ChannelClosed` reason is reused for the "stage handle was aborted
/// externally" semantics — it is already the local-side outcome for shutdown
/// and aborts, and per commit `c3fd8963a` all `ValidationCancelReason` variants
/// classify as `SoftInternal`, so routing is correct without introducing a new
/// variant.
fn classify_poa_join_error(err: &tokio::task::JoinError) -> ValidationError {
    if err.is_panic() {
        ValidationError::TaskPanicked {
            task: "poa".to_string(),
            details: format!("{:?}", err),
        }
    } else {
        // `is_cancelled()` is the only other producer of `JoinError`. The
        // cancel arm in `execute_concurrent` aborted the PoA blocking handle
        // — a local, peer-innocent event. `ChannelClosed` carries the same
        // "we deliberately tore down this stage" semantics.
        ValidationError::ValidationCancelled {
            reason: ValidationCancelReason::ChannelClosed,
        }
    }
}

/// Convert a `ParentValidationResult` from the inner cancel future into a
/// `ValidationResult`. The inner cancel future uses a Continue-only
/// `extra_checks` closure, so only the `Cancelled(_)` arm is reachable.
fn cancel_outcome_to_result(outcome: ParentValidationResult) -> ValidationResult {
    match outcome {
        ParentValidationResult::Cancelled(reason) => {
            ValidationError::ValidationCancelled { reason }.into()
        }
        // The Continue-only extra_checks closure used here never breaks
        // with `Ready`; this arm is unreachable in production.
        ParentValidationResult::Ready => unreachable!(
            "exit_if_block_is_too_old must not return Ready when given a Continue-only extra_checks closure"
        ),
    }
}

/// Merge concurrent stage results into a single `ValidationResult` using a
/// three-tier priority ordering:
///
/// 1. **Node-fault `InternalFailure` wins first.** Variants whose
///    `is_node_fault()` is true (e.g. `TaskPanicked`,
///    `ExecutionLayerTransportFailed`, `ShadowTxNodeFault`) signal a broken
///    local node. Surfacing these is SAFETY-CRITICAL: the block-pool's
///    `is_node_fault()` check is what triggers panic + supervisor restart.
///    If we let an `Invalid` from a sibling stage win, the block would be
///    reported as a consensus rejection and the panic+SIGINT invariant
///    would silently break at exactly the scenario where it matters most.
/// 2. **`Invalid` next.** A consensus rejection is the strongest signal among
///    non-fault outcomes — peer attribution / block discard is correct.
/// 3. **Soft `InternalFailure` last.** Eviction / saturation variants
///    (`ParentBlockMissing`, `Parent*SnapshotMissing`,
///    `ExecutionPayloadCacheEvicted`) classify as internal-failure but not
///    node-fault; the block parks in cache for a later retry.
///
/// Order within each tier is the input array order so the priority is
/// deterministic.
fn merge_stage_results(stage_results: &[&ValidationResult]) -> ValidationResult {
    merge_stage_results_with_cancel(stage_results, None)
}

/// Same as [`merge_stage_results`] but also accepts an OPTIONAL
/// cancellation outcome. Used by the inner cancel race in
/// `validate_block`: when cancellation fires while stages are still
/// running, the drain-poll captures any ready stage results and this
/// merger guarantees a `NodeFault` from any stage still beats the
/// cancel signal.
///
/// Priority including the cancel input:
///
/// 1. Node-fault `InternalFailure` (any stage)
/// 2. `Invalid` (any stage)
/// 3. Cancellation outcome (if supplied)
/// 4. Soft `InternalFailure` (any stage)
///
/// Cancellation outranks soft `InternalFailure` because once cancellation
/// has fired the in-progress stages are deliberately abandoned — a soft
/// internal failure produced by, say, a snapshot eviction during the
/// cancel window adds no information. But cancellation must NEVER outrank
/// a real consensus rejection (we observed the block to be bad) or a node
/// fault (we observed our own node to be bad) — those need to surface.
///
/// CALLER INVARIANT: this function is only called when at least one stage
/// is non-Valid OR `cancel` is `Some(_)`. Passing all-Valid stages with no
/// cancellation is a caller bug — the falls-through arm `debug_assert!`s
/// and returns `Valid` in release for safe degradation.
fn merge_stage_results_with_cancel(
    stage_results: &[&ValidationResult],
    cancel: Option<&ValidationResult>,
) -> ValidationResult {
    // Tier 1: node-fault InternalFailure must win over Invalid so the
    // supervisor restarts the node.
    if let Some(node_fault) = stage_results.iter().find_map(|r| match r {
        ValidationResult::InternalFailure(inner) if inner.is_node_fault() => Some(inner.clone()),
        _ => None,
    }) {
        return ValidationResult::InternalFailure(node_fault);
    }

    // Tier 2: consensus rejection.
    if let Some(invalid) = stage_results.iter().find_map(|r| match r {
        ValidationResult::Invalid(rejection) => Some(rejection.clone()),
        _ => None,
    }) {
        return ValidationResult::Invalid(invalid);
    }

    // Tier 3: cancellation outcome (if any). Inserted between consensus
    // rejection and soft internal failure: cancellation says "we gave up
    // before we could decide", which is a stronger statement than an
    // eviction race observed on an already-abandoned stage.
    if let Some(cancel) = cancel {
        return cancel.clone();
    }

    // Tier 4: remaining (soft) InternalFailure — block parks in cache for
    // retry, no peer attribution.
    if let Some(internal) = stage_results.iter().find_map(|r| match r {
        ValidationResult::InternalFailure(inner) => Some(inner.clone()),
        _ => None,
    }) {
        return ValidationResult::InternalFailure(internal);
    }

    // Unreachable by caller invariant: `merge_stage_results_with_cancel` is
    // only invoked from the consensus-failure branch (at least one stage is
    // non-Valid) OR with a `Some(cancel)` outcome. An all-Valid input with
    // no cancellation means the caller routed a clean block through the
    // failure path — fail loud in debug so tests catch the regression, fail
    // clean in release (Valid is the only safe direction for clean input;
    // returning Invalid would wrong-attribute a peer rejection).
    debug_assert!(
        false,
        "merge_stage_results_with_cancel called with all-Valid stages and no cancellation — caller invariant broken"
    );
    ValidationResult::Valid
}

#[cfg(test)]
mod is_parent_ready_tests {
    //! Boundary tests for `is_parent_ready`.
    //!
    //! The predicate was tightened from `Validated(_)` to
    //! `Validated(BlockState::ValidBlock)` only so children whose parent is in
    //! `Validated(Unknown)` or `Validated(ValidationScheduled)` continue to wait.
    //! These tests lock in that boundary so a future refactor cannot
    //! accidentally re-broaden it.
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::onchain(ChainState::Onchain, true)]
    #[case::validated_valid_block(ChainState::Validated(BlockState::ValidBlock), true)]
    #[case::not_onchain_valid_block(ChainState::NotOnchain(BlockState::ValidBlock), true)]
    // Regression targets: prior buggy `Validated(_)` match accepted these.
    #[case::validated_unknown(ChainState::Validated(BlockState::Unknown), false)]
    #[case::validated_validation_scheduled(
        ChainState::Validated(BlockState::ValidationScheduled),
        false
    )]
    #[case::not_onchain_unknown(ChainState::NotOnchain(BlockState::Unknown), false)]
    #[case::not_onchain_validation_scheduled(
        ChainState::NotOnchain(BlockState::ValidationScheduled),
        false
    )]
    fn is_parent_ready_chain_state_dispatch(
        #[case] parent_state: ChainState,
        #[case] expected: bool,
    ) {
        assert_eq!(
            is_parent_ready(&parent_state),
            expected,
            "is_parent_ready({:?}) should be {}",
            parent_state,
            expected
        );
    }
}

#[cfg(test)]
mod merge_stage_results_tests {
    //! Tests for the three-tier `merge_stage_results` priority ordering.
    //!
    //! The merger is SAFETY-CRITICAL: a node-fault `InternalFailure` must win
    //! over a sibling stage's `Invalid` so that block-pool dispatch hits the
    //! `is_node_fault()` panic+SIGINT path. Misclassifying a node fault as
    //! `Invalid` would silently park a broken node and discard the block.
    use super::*;
    use crate::block_validation::ValidationError;
    use irys_types::H256;
    use rstest::rstest;

    /// Consensus rejection — used for `Invalid` cases.
    fn invalid() -> ValidationResult {
        ValidationError::ShadowTransactionInvalid("consensus mismatch".to_string()).into()
    }

    /// Node-fault `InternalFailure` — must trigger supervisor restart.
    fn node_fault_internal() -> ValidationResult {
        ValidationError::TaskPanicked {
            task: "poa".to_string(),
            details: "verifier thread panicked".to_string(),
        }
        .into()
    }

    /// Soft (eviction-race) `InternalFailure` — block parks in cache for
    /// retry, no peer attribution, no node restart.
    fn soft_internal() -> ValidationResult {
        ValidationError::ParentBlockMissing {
            block_hash: H256::zero(),
        }
        .into()
    }

    /// Sanity-check the fixtures — the merger's correctness depends on
    /// `is_node_fault()` discriminating these construction paths.
    #[test]
    fn fixtures_classify_correctly() {
        let ValidationResult::InternalFailure(inner) = node_fault_internal() else {
            panic!("node_fault_internal() must produce InternalFailure");
        };
        assert!(
            inner.is_node_fault(),
            "TaskPanicked must classify as a node fault"
        );

        let ValidationResult::InternalFailure(inner) = soft_internal() else {
            panic!("soft_internal() must produce InternalFailure");
        };
        assert!(
            !inner.is_node_fault(),
            "ParentBlockMissing must NOT classify as a node fault"
        );
    }

    /// A node-fault `InternalFailure` must win over `Invalid` regardless of
    /// position in the input array. This is the regression target for the
    /// pre-fix bug where the first `Invalid` was returned and the node-fault
    /// was discarded.
    #[rstest]
    #[case::node_fault_first(
        vec![node_fault_internal(), invalid(), ValidationResult::Valid, ValidationResult::Valid]
    )]
    #[case::invalid_first(
        vec![invalid(), node_fault_internal(), ValidationResult::Valid, ValidationResult::Valid]
    )]
    #[case::valid_then_invalid_then_node_fault(
        vec![
            ValidationResult::Valid,
            invalid(),
            ValidationResult::Valid,
            node_fault_internal(),
            ValidationResult::Valid,
            ValidationResult::Valid,
        ]
    )]
    #[case::mixed_with_soft_internal(
        vec![
            invalid(),
            soft_internal(),
            node_fault_internal(),
            ValidationResult::Valid,
        ]
    )]
    fn node_fault_internal_failure_wins_over_invalid(#[case] results: Vec<ValidationResult>) {
        let refs: Vec<&ValidationResult> = results.iter().collect();
        match merge_stage_results(&refs) {
            ValidationResult::InternalFailure(inner) => assert!(
                inner.is_node_fault(),
                "expected node-fault InternalFailure, got soft InternalFailure: {:?}",
                inner.err()
            ),
            other => panic!("expected InternalFailure(node-fault), got {:?}", other),
        }
    }

    /// In the absence of any node-fault InternalFailure, a consensus rejection
    /// (`Invalid`) wins over a soft `InternalFailure`. This locks in the
    /// pre-existing behavior (peer-attributable bad block beats local
    /// eviction race).
    #[test]
    fn invalid_wins_over_soft_internal_failure() {
        let results = [
            ValidationResult::Valid,
            soft_internal(),
            invalid(),
            ValidationResult::Valid,
        ];
        let refs: Vec<&ValidationResult> = results.iter().collect();
        match merge_stage_results(&refs) {
            ValidationResult::Invalid(_) => {}
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    /// Two `Invalid` results with no `InternalFailure` of any kind: merger
    /// returns `Invalid` (regression target for the existing behavior).
    #[test]
    fn two_invalids_no_internal_failure_returns_invalid() {
        let results = [
            ValidationResult::Valid,
            invalid(),
            ValidationResult::Valid,
            invalid(),
            ValidationResult::Valid,
            ValidationResult::Valid,
        ];
        let refs: Vec<&ValidationResult> = results.iter().collect();
        match merge_stage_results(&refs) {
            ValidationResult::Invalid(_) => {}
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    /// Only soft `InternalFailure`s and `Valid`s — merger returns the soft
    /// failure so the block parks in cache for retry.
    #[test]
    fn only_soft_internal_failure_returns_internal_failure() {
        let results = [
            ValidationResult::Valid,
            soft_internal(),
            ValidationResult::Valid,
        ];
        let refs: Vec<&ValidationResult> = results.iter().collect();
        match merge_stage_results(&refs) {
            ValidationResult::InternalFailure(inner) => assert!(
                !inner.is_node_fault(),
                "expected soft InternalFailure, got node-fault: {:?}",
                inner.err()
            ),
            other => panic!("expected InternalFailure(soft), got {:?}", other),
        }
    }

    /// All-`Valid` input is never produced by the call site (the merger only
    /// runs in the consensus-failure branch). The merger guards this caller
    /// invariant with a `debug_assert!`, so debug builds (including tests)
    /// panic loudly if the invariant is ever broken by a future refactor.
    #[test]
    #[should_panic(expected = "caller invariant broken")]
    fn all_valid_with_no_cancel_panics_in_debug() {
        let results = [
            ValidationResult::Valid,
            ValidationResult::Valid,
            ValidationResult::Valid,
        ];
        let refs: Vec<&ValidationResult> = results.iter().collect();
        let _ = merge_stage_results(&refs);
    }

    // ---- merge_stage_results_with_cancel ----
    //
    // The cancel-aware merger is what defends the H2 invariant: when the
    // inner cancellation future fires while stages are still running, the
    // drain-poll in `validate_block` captures any already-ready stage
    // results and hands them here together with the cancel outcome.
    // A NodeFault in any stage MUST still win over the cancel; otherwise
    // the supervisor restart path is silently bypassed at exactly the
    // case where it matters most.

    /// Cancellation outcome fixture for the
    /// `HeightDifference` reason (the classic "outer select drops the
    /// validate future" trigger before the H2 fix).
    ///
    /// Post-M2 audit (2026-05-20): routes through `InternalFailure` (not
    /// `Invalid`) — `HeightDifference` is a local-side event, not a
    /// statement about the peer's block.
    fn cancel_height_diff() -> ValidationResult {
        ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::HeightDifference,
        }
        .into()
    }

    /// Cancellation outcome fixture for the `ChannelClosed` reason
    /// (shutdown). Post-M2 audit (2026-05-20): routes through
    /// `InternalFailure` per the updated `ValidationCancelReason::is_internal`
    /// classification (shutdown is local, not a peer-attributable event) —
    /// the OTHER half of the H2 hazard.
    fn cancel_channel_closed() -> ValidationResult {
        ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::ChannelClosed,
        }
        .into()
    }

    /// H2 regression target: NodeFault stage produced while siblings were
    /// pending, and cancel fires before the join would complete. The
    /// drain-poll in `validate_block` captures the NodeFault and hands it
    /// here together with the cancel; the merger MUST surface the
    /// NodeFault (so the block_tree_service handler panics) rather than
    /// returning the cancellation outcome (which post-M2 routes to a soft
    /// `InternalFailure`, parking a broken node in cache forever; pre-M2
    /// it routed to `Invalid`, peer-attributing a local fault). Either way,
    /// the supervisor-restart path is silently bypassed if cancel outranks
    /// NodeFault, which is exactly what this test guards against.
    #[rstest]
    #[case::node_fault_with_height_diff_cancel(cancel_height_diff())]
    #[case::node_fault_with_channel_closed_cancel(cancel_channel_closed())]
    fn node_fault_wins_over_cancellation(#[case] cancel: ValidationResult) {
        let node_fault = node_fault_internal();
        let pending_proxy = ValidationResult::Valid; // stand-in for a stage that never produced
        let results = [
            &pending_proxy,
            &node_fault,
            &pending_proxy,
            &pending_proxy,
            &pending_proxy,
            &pending_proxy,
        ];
        match merge_stage_results_with_cancel(&results, Some(&cancel)) {
            ValidationResult::InternalFailure(inner) => assert!(
                inner.is_node_fault(),
                "H2 regression: cancel outranked NodeFault, got soft InternalFailure: {:?}",
                inner.err()
            ),
            other => panic!(
                "H2 regression: expected InternalFailure(node-fault), got {:?}",
                other
            ),
        }
    }

    /// Cancellation does NOT outrank a real consensus rejection observed
    /// before cancel arrived: an `Invalid` observed in a sibling stage
    /// means the block IS bad, and we should report it as such rather
    /// than as a cancellation.
    #[test]
    fn invalid_wins_over_cancellation() {
        let invalid_r = invalid();
        let valid_r = ValidationResult::Valid;
        let cancel = cancel_height_diff();
        let results = [&valid_r, &invalid_r, &valid_r, &valid_r, &valid_r, &valid_r];
        match merge_stage_results_with_cancel(&results, Some(&cancel)) {
            ValidationResult::Invalid(rejection) => assert!(
                matches!(
                    rejection.err(),
                    ValidationError::ShadowTransactionInvalid(_)
                ),
                "expected ShadowTransactionInvalid, got {:?}",
                rejection.err()
            ),
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    /// Cancellation outranks soft `InternalFailure`: once cancellation
    /// has fired, the in-progress stages are deliberately abandoned —
    /// a snapshot-eviction observed during the cancel window adds no
    /// information and the caller already knows we gave up.
    ///
    /// Post-M2 audit (2026-05-20): the cancel outcome is now an
    /// `InternalFailure` (was `Invalid`), but the merger's tier-3 priority
    /// is unchanged — cancel still outranks soft `InternalFailure`.
    #[test]
    fn cancellation_wins_over_soft_internal_failure() {
        let soft = soft_internal();
        let valid_r = ValidationResult::Valid;
        let cancel = cancel_height_diff();
        let results = [&valid_r, &soft, &valid_r, &valid_r, &valid_r, &valid_r];
        match merge_stage_results_with_cancel(&results, Some(&cancel)) {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    !inner.is_node_fault(),
                    "cancel outcome must be a soft InternalFailure, not a node fault: {:?}",
                    inner.err()
                );
                assert!(
                    matches!(
                        inner.err(),
                        ValidationError::ValidationCancelled {
                            reason:
                                crate::block_validation::ValidationCancelReason::HeightDifference,
                        }
                    ),
                    "expected cancellation outcome to survive, got {:?}",
                    inner.err()
                );
            }
            other => panic!(
                "expected InternalFailure(ValidationCancelled) post-M2, got {:?}",
                other
            ),
        }
    }

    /// With no cancellation, behavior matches the legacy `merge_stage_results`
    /// (regression coverage for the wrapper).
    #[test]
    fn no_cancel_matches_legacy_merger() {
        let invalid_r = invalid();
        let soft = soft_internal();
        let valid_r = ValidationResult::Valid;
        let results = [&valid_r, &soft, &invalid_r, &valid_r, &valid_r, &valid_r];
        let with_cancel = merge_stage_results_with_cancel(&results, None);
        let legacy = merge_stage_results(&results);
        match (with_cancel, legacy) {
            (ValidationResult::Invalid(_), ValidationResult::Invalid(_)) => {}
            (a, b) => panic!(
                "wrapper diverged from legacy: with_cancel={:?}, legacy={:?}",
                a, b
            ),
        }
    }

    /// All-Valid stages with a cancel input: cancel is the result.
    /// Mirrors the "drain-poll observed a join that was Ready but every
    /// stage happened to be Valid" path — extremely rare but the merger
    /// must still produce the cancel outcome rather than the defensive
    /// `Other(..)` fallback (which would obliterate the cancellation
    /// reason).
    ///
    /// Post-M2 audit (2026-05-20): cancel routes through `InternalFailure`
    /// (was `Invalid`). Sub-reason still surfaces unchanged.
    #[test]
    fn all_valid_with_cancel_returns_cancellation() {
        let valid_r = ValidationResult::Valid;
        let cancel = cancel_height_diff();
        let results = [&valid_r, &valid_r, &valid_r, &valid_r, &valid_r, &valid_r];
        match merge_stage_results_with_cancel(&results, Some(&cancel)) {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    matches!(inner.err(), ValidationError::ValidationCancelled { .. }),
                    "expected cancellation outcome, got {:?}",
                    inner.err()
                );
            }
            other => panic!(
                "expected InternalFailure(ValidationCancelled) post-M2, got {:?}",
                other
            ),
        }
    }
}

#[cfg(test)]
mod classify_poa_join_error_tests {
    //! Tests for the H1 fix: PoA `JoinError` must split panic vs cancellation
    //! rather than unconditionally classifying every `JoinError` as
    //! `TaskPanicked`. The bug is dormant in the current call site (the cancel
    //! arm drops the stages-join future after a synchronous `futures::poll!`),
    //! but a future refactor that ever `.await`s the aborted PoA handle would
    //! turn every legitimate height-diff cancel into a supervisor restart —
    //! exactly the never-mislabel invariant this branch defends.
    //!
    //! See the helper's doc for the mirror to `validation_service.rs:438-479`
    //! and the SoftInternal routing guarantee from commit `c3fd8963a`.
    use super::*;
    use crate::block_validation::{ErrorClass, ValidationCancelReason, ValidationError};

    /// Panic path: a panicking blocking task produces `JoinError::is_panic()`.
    /// Must classify as `TaskPanicked` and route through `NodeFault` so the
    /// supervisor restart path still fires for genuine verifier-thread crashes.
    #[tokio::test]
    async fn panic_classifies_as_task_panicked_node_fault() {
        let handle = tokio::task::spawn_blocking(|| {
            panic!("simulated poa verifier panic");
        });
        let join_err = handle
            .await
            .expect_err("panicking task must produce JoinError");
        assert!(
            join_err.is_panic(),
            "fixture must produce is_panic() = true"
        );

        let err = classify_poa_join_error(&join_err);
        match &err {
            ValidationError::TaskPanicked { task, .. } => {
                assert_eq!(task, "poa", "TaskPanicked.task must be \"poa\"");
            }
            other => panic!("expected TaskPanicked, got {:?}", other),
        }
        assert_eq!(
            err.classify(),
            ErrorClass::NodeFault,
            "panic must classify as NodeFault so supervisor restarts"
        );
        assert!(
            err.is_node_fault(),
            "TaskPanicked must trip the is_node_fault() supervisor-restart gate"
        );
    }

    /// Cancellation path: aborting a handle then awaiting it produces
    /// `JoinError::is_cancelled()`. Must classify as `ValidationCancelled`
    /// and route through `SoftInternal` so the block parks for retry rather
    /// than triggering supervisor restart. This is the H1 regression target.
    #[tokio::test]
    async fn cancellation_classifies_as_validation_cancelled_soft_internal() {
        // Spawn a long-running task, abort it, then await — yields
        // JoinError::is_cancelled() = true.
        let handle = tokio::task::spawn(async {
            // Park until aborted.
            std::future::pending::<()>().await;
        });
        handle.abort();
        let join_err = handle
            .await
            .expect_err("aborted task must produce JoinError");
        assert!(
            join_err.is_cancelled(),
            "fixture must produce is_cancelled() = true"
        );
        assert!(
            !join_err.is_panic(),
            "fixture must not produce is_panic() = true"
        );

        let err = classify_poa_join_error(&join_err);
        match &err {
            ValidationError::ValidationCancelled { reason } => {
                assert_eq!(
                    *reason,
                    ValidationCancelReason::ChannelClosed,
                    "cancellation must reuse ChannelClosed per the H1 fix"
                );
            }
            other => panic!("expected ValidationCancelled, got {:?}", other),
        }
        assert_eq!(
            err.classify(),
            ErrorClass::SoftInternal,
            "cancellation must classify as SoftInternal (park-and-retry), \
             NOT NodeFault (supervisor restart) — this is the H1 invariant"
        );
        assert!(
            !err.is_node_fault(),
            "cancellation must NOT trip the is_node_fault() supervisor-restart gate"
        );
    }
}
