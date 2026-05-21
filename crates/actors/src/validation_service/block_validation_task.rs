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
    RecallRangeError, SubmitPayloadError, ValidationCancelReason, ValidationError,
    commitment_txs_are_valid, data_txs_are_valid, is_seed_data_valid, poa_is_valid,
    recall_recall_range_is_valid, shadow_transactions_are_valid, submit_payload_to_reth,
};
use crate::metrics;
use crate::validation_service::ValidationServiceInner;
use irys_domain::{BlockState, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, SealedBlock, SystemLedger, UnixTimestampMs};
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, OnceLock};
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

impl ParentValidationResult {
    /// Extract the cancellation reason.
    ///
    /// # Panics
    /// Panics if called on `Ready` — this helper is only valid in
    /// cancel-arm paths where the caller invariant guarantees `Cancelled(_)`.
    fn into_cancel_reason(self) -> ValidationCancelReason {
        match self {
            Self::Cancelled(reason) => reason,
            Self::Ready => panic!(
                "into_cancel_reason called on ParentValidationResult::Ready — \
                 cancel_outcome_to_result is only valid in cancel-arm paths"
            ),
        }
    }
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
        // The `poa_abort_slot` is still threaded through so the cancel arm
        // inside `validate_block` can mark the PoA JoinHandle aborted.
        // Note: `AbortHandle::abort()` on a `spawn_blocking` task does NOT
        // free the thread early — see the `poa_abort_slot.set(...)` site for
        // the full design rationale.
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
        // "internal_error" buckets for observability. Every
        // `ValidationCancelled` sub-reason routes through `InternalFailure`
        // — none are peer-attributable — but the "cancelled" label is
        // preserved on both wrappers so dashboards that historically
        // tracked cancellations continue to work, and any legacy code path
        // that still produces an `Invalid(cancelled)` labels consistently.
        // `TaskPanicked` only routes through `InternalFailure` and labels
        // as "panicked".
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

        // Subscribe BEFORE the first state read so any parent state transition
        // that happens between subscribe and read is buffered on the channel
        // and observed by the next `recv()`. The previous lost-update window
        // was historically closed by a `recent_validation_result` seed-store
        // (since removed); the current safety net is structural: if the parent is
        // absent from the tree at first-read, the `extra_checks` closure routes
        // through `ValidationCancelReason::ParentMissing` → `is_internal() =
        // true` → `InternalFailure` (SoftInternal). We do not need to observe
        // the parent's exact fault outcome to safely park the child — any
        // disappearance is treated as a soft-internal race and re-gossip will
        // retry. Do not reorder: a check-then-subscribe variant would re-open
        // the lost-update window.
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

        // Side channel for recovering safety-critical NodeFault / Invalid
        // observations on the cancel path. Each stage writes to this
        // BEFORE returning so a stage that completed before the cancel
        // arm fired contributes its observation here even if
        // `tokio::join!`'s combined future is still `Pending`. See the
        // type-level doc on `StageFaultCaptures` for the full rationale.
        let stage_captures: Arc<Mutex<StageFaultCaptures>> =
            Arc::new(Mutex::new(StageFaultCaptures::default()));

        // Recall range validation
        let recall_captures = Arc::clone(&stage_captures);
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
                Err(RecallRangeError::StepsUnavailable(err)) => {
                    tracing::warn!(
                        custom.error = ?err,
                        "recall range validation: local VDF steps unavailable (soft internal)"
                    );
                    ValidationError::RecallRangeStepsUnavailable(err.to_string()).into()
                }
                Err(RecallRangeError::Mismatch(err)) => {
                    tracing::error!(
                        custom.error = ?err,
                        "recall range validation failed"
                    );
                    ValidationError::RecallRangeInvalid(err.to_string()).into()
                }
            };
            metrics::record_validation_result("recall_range", result.granular_metric_label());
            // Side-channel write: record NodeFault / Invalid before
            // returning so the cancel arm in `validate_block` can recover
            // this observation even if other stages remain pending and
            // the combined `tokio::join!` future never reports Ready.
            capture_stage_result(&recall_captures, result)
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
            let db = self.service_inner.db.clone();
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
                        &db,
                        parent_block_hash,
                        block_height.saturating_sub(1),
                        &poa_epoch_snapshot,
                        &consensus_config,
                        &miner_address,
                    )
                    .map(|()| ValidationResult::Valid)
                });
                // Publish the abort handle so the cancel arm can mark the
                // JoinHandle aborted. NOTE: `AbortHandle::abort()` on a
                // `spawn_blocking` task does NOT stop the thread — the
                // blocking work runs to completion, but its result (success,
                // Invalid, or panic) is intentionally discarded once the
                // validation is cancelled. Rationale: a cancelled validation
                // has already been classified as throwaway; if its outcome
                // was load-bearing, the block will be re-gossiped and
                // re-validated freshly, where any persistent fault will
                // reproduce. Observing cancelled-stage outcomes would also
                // turn transient panics into false NodeFault crashes.
                let _ = poa_abort_slot.set(handle.abort_handle());
                handle
            }
        };

        let poa_captures = Arc::clone(&stage_captures);
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
                    // Split panic vs cancellation rather than unconditionally
                    // classifying every `JoinError` as `TaskPanicked`. The cancel
                    // arm in `execute_concurrent` calls `handle.abort()` on the
                    // PoA blocking task; if a future refactor ever `.await`s the
                    // aborted handle, the resulting `JoinError::is_cancelled()`
                    // must NOT route through the NodeFault taxonomy and trigger a
                    // supervisor restart. Mirrors the established pattern in the
                    // VDF dispatch loop; see `classify_poa_join_error` for the
                    // panic/cancel split rationale.
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
            // Side-channel write: PoA is the most likely producer of a
            // NodeFault (`TaskPanicked` from a panicked verifier thread,
            // `BlockBoundsLookupError`, etc.), and historically was the
            // exact stage whose Ready signal could be silently dropped
            // by the broken drain-poll. Recording here closes that hazard.
            capture_stage_result(&poa_captures, result)
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
        let shadow_tx_captures = Arc::clone(&stage_captures);
        let shadow_tx_task = async move {
            let started = Instant::now();

            // Both soft-internal early returns below record the shadow_tx
            // stage duration + result before returning, and route through
            // `capture_stage_result` so any future reclassification of these
            // early-exit variants automatically participates in the
            // cancel-arm recovery. Prior shape used `?` which bypassed both.
            // Matches the pattern used by `seeds_validation_task` for its
            // `ParentBlockMissing` exit.

            // Eviction-race: parent's commitment snapshot was present at
            // prevalidation but the in-memory window has since rotated.
            // Surfacing this as the typed `ParentCommitmentSnapshotMissing`
            // (already `is_internal_failure`, not `is_node_fault`) keeps it
            // out of the consensus-rejection bucket — previously this path
            // was bucketed into `ShadowTransactionInvalid` via the eyre
            // `?`, which misclassified an honest peer's block.
            let parent_commitment_snapshot = match self
                .block_tree_guard
                .read()
                .get_commitment_snapshot(&block.previous_block_hash)
            {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    let err = ValidationError::ParentCommitmentSnapshotMissing {
                        block_hash: block.previous_block_hash,
                    };
                    metrics::record_validation_stage_duration_ms(
                        "shadow_tx",
                        started.elapsed().as_secs_f64() * 1000.0,
                    );
                    metrics::record_validation_result("shadow_tx", err.metric_label());
                    // Side-channel write before propagating the original
                    // `ValidationError`. The captured `ValidationResult`
                    // is intentionally discarded — the surrounding
                    // closure returns `Result<_, ValidationError>` and
                    // the error is what propagates upward; the capture
                    // is the side effect.
                    let _ = capture_stage_result(&shadow_tx_captures, err.clone().into());
                    return Err(err);
                }
            };

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
            let execution_data = match self
                .service_inner
                .execution_payload_provider
                .wait_for_payload(&block.evm_block_hash)
                .in_current_span()
                .await
            {
                Ok(payload) => payload,
                Err(err) => {
                    let validation_err = match err {
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
                    };
                    metrics::record_validation_stage_duration_ms(
                        "shadow_tx",
                        started.elapsed().as_secs_f64() * 1000.0,
                    );
                    metrics::record_validation_result("shadow_tx", validation_err.metric_label());
                    // Side-channel write before propagating the original
                    // `ValidationError` — capture return value
                    // intentionally discarded (see the analogous arm
                    // above on `ParentCommitmentSnapshotMissing`).
                    let _ =
                        capture_stage_result(&shadow_tx_captures, validation_err.clone().into());
                    return Err(validation_err);
                }
            };

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
                    // `ShadowTxNodeFault`, etc. don't inflate the
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
            // `ShadowTxNodeFault`,
            // `ParentEpochSnapshotMissing`, etc.) reach `InternalFailure`
            // (and node-fault variants trigger abort+restart), while
            // genuine consensus mismatches (`ShadowTransactionInvalid`)
            // reach `Invalid`. Wrapping every error here as
            // `ShadowTransactionInvalid` would erase that classification
            // and peer-attribute local DB/snapshot failures.
            //
            // Side-channel write: shadow_tx is the second-most-likely
            // NodeFault producer after PoA — `ShadowTxNodeFault` (treasury
            // underflow, etc.) is the canonical hard-fault case. We
            // observe the error via the same `Into<ValidationResult>`
            // dispatcher used downstream, so the classification this
            // side-channel records is identical to what the natural-join
            // path would have routed through `merge_stage_results`.
            // The two soft-internal early returns above
            // (`ParentCommitmentSnapshotMissing`,
            // `ExecutionPayloadCacheEvicted`) also call
            // `capture_stage_result` — the capture is a no-op for
            // soft-internal today but guards against future
            // reclassification.
            if let Err(ref err) = result {
                // Side-channel write — capture return value intentionally
                // discarded; `result` (the `Result<(), ValidationError>`)
                // is the propagation channel.
                let _ = capture_stage_result(&shadow_tx_captures, err.clone().into());
            }
            result.map(|()| execution_data)
        };

        let vdf_reset_frequency = self.service_inner.config.vdf.reset_frequency as u64;
        let seeds_block_hash = self.sealed_block.header().block_hash;
        let seeds_block_height = self.sealed_block.header().height;
        let seeds_captures = Arc::clone(&stage_captures);
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
                        // Side-channel write: `ParentBlockMissing` is soft
                        // internal — the capture is a no-op — but we
                        // route through the helper so any future
                        // reclassification of this early-exit variant
                        // automatically participates in the cancel-arm
                        // recovery.
                        return capture_stage_result(&seeds_captures, result);
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
            capture_stage_result(&seeds_captures, outcome)
        }
        .instrument(tracing::info_span!(
            "seeds_validation",
            block.hash = %seeds_block_hash,
            block.height = %seeds_block_height
        ));

        // Commitment transaction ordering validation
        let sealed_block_for_commitment = self.sealed_block.clone();
        let commitment_captures = Arc::clone(&stage_captures);
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
            capture_stage_result(&commitment_captures, result)
        };

        // Data transaction fee validation. Snapshots were cloned up-front
        // alongside the shadow_tx clones (see comment above shadow_tx_task).
        let sealed_block_for_data = self.sealed_block.clone();
        let data_txs_captures = Arc::clone(&stage_captures);
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
            capture_stage_result(&data_txs_captures, result)
        };

        // Race the six concurrent stages against the height-diff /
        // channel-closed cancellation future. On cancel, the in-flight
        // stages are abandoned (their tuple slots in `tokio::join!` will
        // never be observed) — BUT a safety-critical NodeFault or
        // Invalid produced by any stage that DID complete before the
        // cancel arm fired is recovered out-of-band via the
        // `stage_captures` side channel.
        //
        // # Why not drain the `tokio::join!` future on cancel?
        //
        // The previous implementation here boxed the join future and,
        // on cancel, called `futures::poll!(&mut stages_join)` exactly
        // once to "catch any stage that became Ready alongside the
        // cancel signal". That was structurally broken:
        // `tokio::join!`'s combined future is `Pending` until ALL six
        // sub-futures are Ready — so a NodeFault from a stage that DID
        // complete (say, PoA's `spawn_blocking` returning a panicked
        // `JoinError` while shadow_tx is still awaiting an EVM payload)
        // is invisible to the drain-poll: `poll!` returns `Pending`,
        // the join is abandoned, and the NodeFault is silently dropped.
        // Routing then proceeds through `cancel_outcome_to_result` →
        // soft `InternalFailure`, the supervisor-restart path that the
        // central "node faults must always win" invariant demands is
        // bypassed, and a broken node parks the block in cache instead
        // of panicking.
        //
        // The fix is the side channel: each stage's async body merges
        // its NodeFault / Invalid observation into `stage_captures`
        // BEFORE returning. The cancel arm reads the side channel
        // synchronously and synthesises the correct outcome — which
        // works regardless of which siblings remain Pending, because
        // the observation does not depend on `tokio::join!`'s aggregate
        // readiness.
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
        let cancel_future = self.exit_if_block_is_too_old(|_| ControlFlow::Continue(()));

        // `biased;` makes the natural-join branch win when both arms are
        // simultaneously Ready. This keeps the simultaneous-completion
        // case on the "Joined" path so the merger sees all six stages
        // and runs its full tier ordering. (On a clean cancel-only
        // outcome the side channel handles NodeFault / Invalid recovery,
        // so this bias is only correctness-relevant for the
        // simultaneous-Ready edge case.)
        let stage_outcome = tokio::select! {
            biased;
            joined = stages_join => StageOutcome::Joined(joined),
            cancel = cancel_future => {
                // Cancel won. Mark the PoA JoinHandle aborted as a
                // signalling gesture. The blocking-pool thread continues
                // to completion regardless — `AbortHandle::abort()` on a
                // `spawn_blocking` task does not stop the thread, and the
                // result is intentionally discarded once `stages_join` is
                // dropped. See the `poa_abort_slot.set(...)` site for the
                // full design rationale.
                if let Some(handle) = poa_abort_slot.get() {
                    handle.abort();
                }
                StageOutcome::Cancelled(cancel)
            }
        };

        // Unpack: natural-join path gets the full six-tuple; cancel
        // path is handled out-of-band via the side channel below.
        let (
            recall_result,
            poa_result,
            shadow_tx_result,
            seeds_validation_result,
            commitment_ordering_result,
            data_txs_result,
        ) = match stage_outcome {
            StageOutcome::Joined(joined) => joined,
            StageOutcome::Cancelled(cancel) => {
                // Drain the side channel SYNCHRONOUSLY: anything a
                // stage already wrote before the cancel arm fired is
                // recoverable here. NodeFault wins over Invalid (the
                // capture type enforces this priority); only when
                // neither was observed do we fall through to the
                // cancellation outcome — matching the
                // `merge_stage_results_with_cancel` tier ordering
                // (NodeFault > Invalid > Cancellation > soft
                // InternalFailure) without needing six per-stage
                // results.
                let captured = std::mem::take(
                    &mut *stage_captures
                        .lock()
                        .expect("StageFaultCaptures mutex poisoned"),
                );
                if let Some(result) = captured.into_cancel_result() {
                    return result;
                }
                return cancel_outcome_to_result(cancel.into_cancel_reason());
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
/// inner cancellation future. See the call site in `validate_block`.
enum StageOutcome<J> {
    /// Stages completed before cancellation fired (case 1 in the
    /// `validate_block` cancel race) — `tokio::select!`'s biased
    /// ordering also routes the simultaneous-ready case here.
    Joined(J),
    /// Cancellation fired before the combined `tokio::join!` future
    /// reported Ready. Per-stage NodeFault / Invalid observations made
    /// inside individual stage bodies are recovered out-of-band via the
    /// [`StageFaultCaptures`] side channel — the join future itself is
    /// abandoned (its stages are dropped here when this variant is
    /// produced).
    Cancelled(ParentValidationResult),
}

/// Convert a `tokio::task::JoinError` from the PoA blocking task into a
/// `ValidationError`, distinguishing panic (genuine node fault) from
/// cancellation (deliberate abort from the cancel arm).
///
/// `JoinError` has exactly two producers: `is_panic()` and `is_cancelled()`.
/// Closes a latent landmine where every `JoinError` — including aborts
/// triggered by the cancel arm's `poa_abort_slot.abort()` — was
/// unconditionally classified as `TaskPanicked`. That misclassification
/// routes through the `NodeFault` taxonomy and triggers a supervisor panic
/// at `on_block_validation_finished`. The bug is dormant today because the
/// cancel arm drops the stages-join future after a synchronous
/// `futures::poll!` rather than awaiting the aborted handle, but any future
/// refactor that ever `.await`s a possibly-aborted handle activates it.
///
/// Mirrors the symmetric pattern in the VDF dispatch loop's JoinError arm:
/// `is_panic()` → `TaskPanicked` (NodeFault → abort + restart);
/// otherwise → `ValidationCancelled` (SoftInternal → park-and-retry).
///
/// `JoinError::Cancelled` here means the PoA `spawn_blocking` handle was
/// externally aborted — the outer-stage cancel arm of `validate_block` tore
/// down the PoA stage because a sibling produced a verdict first, or the
/// runtime is shutting down. Routes through the dedicated `PoAAborted`
/// cancel reason rather than `ChannelClosed` so operator logs and the
/// `validation_cancelled` metric can distinguish "broadcast channel closed at
/// shutdown" from "PoA blocking task aborted mid-flight". Both are
/// `SoftInternal` per `ValidationCancelReason::IS_INTERNAL`, so dispatch is
/// identical; this is a diagnostic split, not a routing change.
fn classify_poa_join_error(err: &tokio::task::JoinError) -> ValidationError {
    if err.is_panic() {
        ValidationError::TaskPanicked {
            task: "poa".to_string(),
            details: format!("{:?}", err),
        }
    } else {
        // `is_cancelled()` is the only other producer of `JoinError`.
        // Mirror the per-reason metric pattern used by the height-diff /
        // parent-missing / channel-closed cancel sites so dashboards can
        // attribute cancel volume to the PoA-abort lane specifically.
        metrics::record_validation_cancellation("poa_aborted");
        ValidationError::ValidationCancelled {
            reason: ValidationCancelReason::PoAAborted,
        }
    }
}

/// Out-of-band capture of NodeFault / Invalid outcomes produced inside
/// individual stage async bodies, used to recover safety-critical
/// observations when the outer `tokio::select!` cancel arm wins the race
/// in `validate_block`.
///
/// # Why this exists
///
/// `tokio::join!`'s combined future is `Pending` until ALL sub-futures
/// are Ready. The previous "drain-poll the boxed join with
/// `futures::poll!`" approach to picking up a NodeFault that landed
/// alongside the cancel was structurally broken: if even one sibling
/// stage was still pending (the typical case — there are six stages and
/// the PoA `spawn_blocking` thread completes on a separate scheduler at
/// arbitrary wall-clock time), the drain-poll returns `Pending` and the
/// NodeFault from a stage that DID complete is silently dropped on the
/// floor as the join future is abandoned. The block then routes through
/// the cancellation outcome (→ soft `InternalFailure`) and the
/// supervisor-restart path that NodeFault demands is bypassed.
///
/// This side channel is the structural fix: each stage's async body
/// inspects its own outcome immediately before returning and merges any
/// NodeFault / Invalid into this shared capture. Because the merge
/// happens BEFORE the stage's outer future returns Ready, a stage that
/// completed before the cancel arm fired has already written its
/// observation here — independent of whether `tokio::join!`'s aggregate
/// future was Ready when the cancel arm ran.
/// `InternalFailure` is deliberately NOT captured here — it does not
/// outrank the cancellation outcome (per the merger's tier-3-over-tier-4
/// ordering), and surfacing it from the cancel arm would peer-attribute
/// nothing useful while obscuring the cancellation reason.
#[derive(Default)]
struct StageFaultCaptures {
    /// First NodeFault observed by any stage. Captured via
    /// `InternalFailureError`'s `Clone` impl so the original stage result
    /// is unchanged — this write is purely observational.
    node_fault: Option<crate::block_tree_service::InternalFailureError>,
    /// First consensus rejection observed by any stage. Same observational
    /// semantics as `node_fault`.
    invalid: Option<crate::block_tree_service::ConsensusRejectionError>,
}

impl StageFaultCaptures {
    /// Merge a stage's `ValidationResult` into the capture. NodeFault and
    /// Invalid observations are recorded (first-write-wins per tier); all
    /// other outcomes (Valid, soft `InternalFailure`) are no-ops.
    ///
    /// This is the side-channel write performed inside each stage's
    /// async body immediately before the stage returns. Callers MUST
    /// invoke this before the final return so the observation is
    /// recorded even if the stage's tuple slot in `tokio::join!` is
    /// later abandoned by a cancel-arm win.
    fn merge(&mut self, result: &ValidationResult) {
        match result {
            ValidationResult::InternalFailure(inner) if inner.is_node_fault() => {
                if self.node_fault.is_none() {
                    self.node_fault = Some(inner.clone());
                }
            }
            ValidationResult::Invalid(rejection) => {
                if self.invalid.is_none() {
                    self.invalid = Some(rejection.clone());
                }
            }
            // Valid: nothing to capture.
            // Soft InternalFailure: deliberately not captured (see
            // type-level doc on priority).
            _ => {}
        }
    }

    /// Synthesize the cancel-arm outcome from captured side-channel
    /// observations: NodeFault wins over Invalid; if neither is set, the
    /// caller falls back to the cancellation outcome.
    ///
    /// Mirrors `merge_stage_results_with_cancel`'s tier ordering for the
    /// cancellation path. The merger itself can't be reused here because
    /// it expects six per-stage `ValidationResult`s — by definition the
    /// cancel arm doesn't have those (the join future was Pending) and
    /// the side channel is the only signal available.
    fn into_cancel_result(self) -> Option<ValidationResult> {
        if let Some(node_fault) = self.node_fault {
            return Some(ValidationResult::InternalFailure(node_fault));
        }
        if let Some(invalid) = self.invalid {
            return Some(ValidationResult::Invalid(invalid));
        }
        None
    }
}

/// Convenience helper for inside stage bodies: record a side-channel
/// observation under a single lock acquisition. Keeps the call sites
/// terse and ensures we don't hold the lock across awaits.
///
/// **Returns the captured `ValidationResult` by value, with `#[must_use]`.**
/// In the common case the stage body uses it as a tail expression
/// (`capture_stage_result(&caps, result)`) so the capture and the
/// return are the same line. A few call sites side-channel before
/// propagating a different error (e.g. shadow_tx soft-error arms) —
/// those must explicitly `let _ = capture_stage_result(...)` to drop
/// the return value, which makes the discipline visible at the call
/// site. The lint is a partial defensive measure: it doesn't catch a
/// stage that forgets to call the helper at all, only one that calls
/// it and then accidentally discards the returned value without
/// `let _`.
#[must_use = "capture_stage_result returns the captured ValidationResult so the stage can forward it as a tail expression; if you intentionally discard it (e.g. you're propagating a different error), prefix the call with `let _ =` to make that explicit"]
fn capture_stage_result(
    captures: &Mutex<StageFaultCaptures>,
    result: ValidationResult,
) -> ValidationResult {
    // `expect` over `?` / silent fall-through: a poisoned lock here means
    // a stage body panicked while holding it (which we never do — the
    // critical section is just two `Option` writes). Propagating the
    // panic surfaces a real bug rather than silently dropping the
    // observation.
    captures
        .lock()
        .expect("StageFaultCaptures mutex poisoned")
        .merge(&result);
    result
}

/// Convert a cancellation reason from the inner cancel future into a
/// `ValidationResult`. Both call sites are in `StageOutcome::Cancelled`
/// arms where the caller invariant guarantees only `Cancelled(_)` is
/// reachable. The type now enforces this: pass the reason directly via
/// [`ParentValidationResult::into_cancel_reason`].
fn cancel_outcome_to_result(reason: ValidationCancelReason) -> ValidationResult {
    ValidationError::ValidationCancelled { reason }.into()
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
    // The cancel-aware merger defends the "NodeFault outranks cancel"
    // invariant: when the inner cancellation future fires while stages
    // are still running, the drain-poll in `validate_block` captures any
    // already-ready stage results and hands them here together with the
    // cancel outcome. A NodeFault in any stage MUST still win over the
    // cancel; otherwise the supervisor restart path is silently bypassed
    // at exactly the case where it matters most.

    /// Cancellation outcome fixture for the `HeightDifference` reason (the
    /// classic "outer select drops the validate future" trigger).
    ///
    /// Routes through `InternalFailure` (not `Invalid`) — `HeightDifference`
    /// is a local-side event, not a statement about the peer's block.
    fn cancel_height_diff() -> ValidationResult {
        ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::HeightDifference,
        }
        .into()
    }

    /// Cancellation outcome fixture for the `ChannelClosed` reason
    /// (shutdown). Routes through `InternalFailure` per
    /// `ValidationCancelReason::IS_INTERNAL` (shutdown is local, not a
    /// peer-attributable event) — the OTHER half of the cancel-vs-NodeFault
    /// hazard the merger guards.
    fn cancel_channel_closed() -> ValidationResult {
        ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::ChannelClosed,
        }
        .into()
    }

    /// Regression target: NodeFault stage produced while siblings were
    /// pending, and cancel fires before the join would complete. The
    /// drain-poll in `validate_block` captures the NodeFault and hands it
    /// here together with the cancel; the merger MUST surface the
    /// NodeFault (so the block_tree_service handler panics) rather than
    /// returning the cancellation outcome (which routes to a soft
    /// `InternalFailure`, parking a broken node in cache forever — and in
    /// a prior shape that routed cancel to `Invalid`, peer-attributing a
    /// local fault). Either way, the supervisor-restart path is silently
    /// bypassed if cancel outranks NodeFault, which is exactly what this
    /// test guards against.
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
                "cancel outranked NodeFault, got soft InternalFailure: {:?}",
                inner.err()
            ),
            other => panic!("expected InternalFailure(node-fault), got {:?}", other),
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
    /// The cancel outcome is an `InternalFailure` (every cancel reason is
    /// `is_internal() = true`), but the merger's tier-3 priority is
    /// unchanged — cancel still outranks soft `InternalFailure`.
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
                "expected InternalFailure(ValidationCancelled), got {:?}",
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
    /// Cancel routes through `InternalFailure` (every cancel reason is
    /// `is_internal() = true`). Sub-reason still surfaces unchanged.
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
                "expected InternalFailure(ValidationCancelled), got {:?}",
                other
            ),
        }
    }
}

#[cfg(test)]
mod classify_poa_join_error_tests {
    //! Tests for the panic-vs-cancellation split in `classify_poa_join_error`:
    //! a PoA `JoinError` must NOT unconditionally classify as `TaskPanicked`.
    //! The misclassification bug is dormant in the current call site (the
    //! cancel arm drops the stages-join future after a synchronous
    //! `futures::poll!`), but a future refactor that ever `.await`s the
    //! aborted PoA handle would turn every legitimate height-diff cancel into
    //! a supervisor restart — exactly the never-mislabel invariant this
    //! branch defends.
    //!
    //! See the helper's doc for the mirror to the VDF dispatch loop's
    //! JoinError arm and the SoftInternal routing guarantee.
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
    /// than triggering supervisor restart. This is the load-bearing
    /// invariant for the panic/cancel split.
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
                    ValidationCancelReason::PoAAborted,
                    "cancellation must route through the dedicated PoAAborted reason \
                     (split from ChannelClosed for diagnostic clarity — same SoftInternal \
                     routing, distinct operator log/metric label)"
                );
            }
            other => panic!("expected ValidationCancelled, got {:?}", other),
        }
        assert_eq!(
            err.classify(),
            ErrorClass::SoftInternal,
            "cancellation must classify as SoftInternal (park-and-retry), \
             NOT NodeFault (supervisor restart) — this is the panic/cancel-split invariant"
        );
        assert!(
            !err.is_node_fault(),
            "cancellation must NOT trip the is_node_fault() supervisor-restart gate"
        );
    }
}

#[cfg(test)]
mod cancel_outcome_to_result_tests {
    //! Tests for `cancel_outcome_to_result` and
    //! `ParentValidationResult::into_cancel_reason`.
    use super::*;
    use crate::block_validation::{ValidationCancelReason, ValidationError};

    /// Sanity check: a cancellation reason routes through
    /// `ValidationError::ValidationCancelled` so cancel-aware callers can
    /// classify the outcome via `is_internal_failure`.
    #[test]
    fn cancelled_routes_to_validation_cancelled_error() {
        let result = cancel_outcome_to_result(ValidationCancelReason::HeightDifference);
        match result {
            ValidationResult::InternalFailure(inner) => assert!(
                matches!(inner.err(), ValidationError::ValidationCancelled { .. }),
                "expected ValidationCancelled, got {:?}",
                inner.err()
            ),
            other => panic!(
                "expected InternalFailure(ValidationCancelled), got {:?}",
                other
            ),
        }
    }

    /// `into_cancel_reason` panics when called on `Ready` — the type-level
    /// invariant that cancel-arm paths only hold `Cancelled(_)`.
    #[test]
    #[should_panic(expected = "into_cancel_reason called on ParentValidationResult::Ready")]
    fn into_cancel_reason_panics_on_ready() {
        let _ = ParentValidationResult::Ready.into_cancel_reason();
    }
}

#[cfg(test)]
mod stage_fault_captures_tests {
    //! Unit tests for `StageFaultCaptures` — the side channel that
    //! recovers safety-critical NodeFault / Invalid observations from
    //! individual stage bodies when the outer `tokio::select!` cancel
    //! arm wins in `validate_block`.
    //!
    //! These tests pin the priority contract (NodeFault > Invalid >
    //! cancellation > soft InternalFailure) and the "first-write wins
    //! within a tier" semantics. The priority MUST match
    //! `merge_stage_results_with_cancel`'s tier ordering so the cancel
    //! arm's synthesized outcome is indistinguishable from what the
    //! merger would have produced if it had been given the per-stage
    //! results.
    //!
    //! See the integration tests in `side_channel_cancel_race_tests` below
    //! for proof that the side channel actually closes the
    //! `tokio::join!`-poll-drain hazard when wired into a real
    //! `tokio::select!`.
    use super::*;
    use crate::block_validation::{PreValidationError, ValidationError};
    use irys_types::H256;

    fn invalid() -> ValidationResult {
        ValidationError::ShadowTransactionInvalid("consensus mismatch".to_string()).into()
    }

    /// Hard NodeFault produced by a panicked verifier thread. The
    /// canonical "must surface even if the cancel arm wins" case.
    fn node_fault() -> ValidationResult {
        ValidationError::TaskPanicked {
            task: "poa".to_string(),
            details: "verifier thread panicked".to_string(),
        }
        .into()
    }

    /// Soft (eviction-race) InternalFailure — block parks in cache for
    /// retry, no peer attribution, no node restart. Deliberately NOT
    /// captured by the side channel.
    fn soft_internal() -> ValidationResult {
        ValidationError::ParentBlockMissing {
            block_hash: H256::zero(),
        }
        .into()
    }

    /// Sanity check: the merge() on a default capture with a single
    /// NodeFault input must produce the NodeFault on cancel-arm read.
    /// This is the load-bearing case the side channel exists for.
    #[test]
    fn merge_records_node_fault() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&node_fault());
        let synthesized = captures
            .into_cancel_result()
            .expect("NodeFault should be recovered");
        match synthesized {
            ValidationResult::InternalFailure(inner) => assert!(
                inner.is_node_fault(),
                "expected node-fault InternalFailure, got soft: {:?}",
                inner.err()
            ),
            other => panic!("expected InternalFailure(node-fault), got {:?}", other),
        }
    }

    /// A single Invalid must round-trip through the side channel.
    #[test]
    fn merge_records_invalid() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&invalid());
        match captures.into_cancel_result() {
            Some(ValidationResult::Invalid(_)) => {}
            other => panic!("expected Some(Invalid), got {:?}", other),
        }
    }

    /// Soft InternalFailure is deliberately NOT captured: the merger's
    /// tier ordering has cancellation outranking soft InternalFailure,
    /// so surfacing a soft observation from the cancel arm would
    /// obscure the cancellation reason without changing block-pool
    /// dispatch.
    #[test]
    fn merge_does_not_record_soft_internal_failure() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&soft_internal());
        assert!(
            captures.into_cancel_result().is_none(),
            "soft InternalFailure must not be captured — cancellation outranks it"
        );
    }

    /// `Valid` is a no-op for the side channel — the capture stays
    /// empty so the cancel arm falls through to the cancellation
    /// outcome.
    #[test]
    fn merge_ignores_valid() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&ValidationResult::Valid);
        captures.merge(&ValidationResult::Valid);
        assert!(
            captures.into_cancel_result().is_none(),
            "Valid results must not produce a synthesized cancel outcome"
        );
    }

    /// NodeFault wins over Invalid REGARDLESS of merge order — matches
    /// `merge_stage_results`' tier-1-over-tier-2 contract. The
    /// side-channel-cancel-race invariant in miniature: a NodeFault
    /// observed by any stage must surface on cancel even if a sibling
    /// Invalid was observed first.
    #[test]
    fn node_fault_wins_over_invalid_invalid_first() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&invalid());
        captures.merge(&node_fault());
        match captures.into_cancel_result() {
            Some(ValidationResult::InternalFailure(inner)) => assert!(
                inner.is_node_fault(),
                "NodeFault must outrank Invalid: {:?}",
                inner.err()
            ),
            other => panic!(
                "expected Some(InternalFailure(node-fault)), got {:?}",
                other
            ),
        }
    }

    /// Same invariant with the other ordering — NodeFault first, then
    /// Invalid arrives later. The capture must NOT downgrade.
    #[test]
    fn node_fault_wins_over_invalid_node_fault_first() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&node_fault());
        captures.merge(&invalid());
        match captures.into_cancel_result() {
            Some(ValidationResult::InternalFailure(inner)) => assert!(
                inner.is_node_fault(),
                "first NodeFault must persist when Invalid arrives later: {:?}",
                inner.err()
            ),
            other => panic!(
                "expected Some(InternalFailure(node-fault)), got {:?}",
                other
            ),
        }
    }

    /// First-write-wins within the NodeFault tier — both producers are
    /// halting outcomes, so picking one representative is sufficient
    /// for the supervisor-restart dispatch. We just want to confirm we
    /// don't accidentally pick up two writes and crash / reorder.
    #[test]
    fn first_node_fault_wins_within_tier() {
        let mut captures = StageFaultCaptures::default();
        // First node-fault: TaskPanicked (PoA).
        let first: ValidationResult = ValidationError::TaskPanicked {
            task: "poa".to_string(),
            details: "first".to_string(),
        }
        .into();
        // Second node-fault: a different variant, same tier.
        let second: ValidationResult = ValidationError::PreValidation(
            PreValidationError::BlockBoundsLookupError("second".to_string()),
        )
        .into();
        captures.merge(&first);
        captures.merge(&second);
        match captures.into_cancel_result() {
            Some(ValidationResult::InternalFailure(inner)) => {
                // Either captured value is acceptable structurally, but
                // the first-write-wins semantics should keep TaskPanicked.
                assert!(
                    matches!(inner.err(), ValidationError::TaskPanicked { .. }),
                    "first NodeFault should persist (first-write-wins), got {:?}",
                    inner.err()
                );
            }
            other => panic!(
                "expected Some(InternalFailure(node-fault)), got {:?}",
                other
            ),
        }
    }

    /// Empty capture → None: the cancel arm falls back to the
    /// cancellation outcome unchanged.
    #[test]
    fn empty_capture_returns_none() {
        let captures = StageFaultCaptures::default();
        assert!(
            captures.into_cancel_result().is_none(),
            "empty capture must return None so the caller produces cancel_outcome_to_result"
        );
    }

    /// Captures with only a soft InternalFailure (which the side
    /// channel ignores) plus a Valid: still empty, still falls
    /// through to cancellation. Regression target for the priority
    /// contract.
    #[test]
    fn soft_internal_plus_valid_returns_none() {
        let mut captures = StageFaultCaptures::default();
        captures.merge(&soft_internal());
        captures.merge(&ValidationResult::Valid);
        assert!(
            captures.into_cancel_result().is_none(),
            "soft + Valid must not produce a cancel-arm override"
        );
    }
}

#[cfg(test)]
mod side_channel_cancel_race_tests {
    //! Integration-flavored tests for the side-channel cancel-race fix:
    //! exercise the real `tokio::select!` race between
    //! `tokio::join!`-of-stages and a cancel future, and confirm the side
    //! channel surfaces NodeFault / Invalid observations that
    //! `futures::poll!(&mut stages_join)` would have silently dropped.
    //!
    //! These tests do NOT spin up a full `BlockValidationTask` — they
    //! reproduce ONLY the structural race: six stage-shaped async
    //! blocks that write to a `StageFaultCaptures` side channel
    //! before returning, raced against an immediate-cancel future
    //! under the same `tokio::select! { biased; }` shape used in
    //! `validate_block`. The cancel arm reads the side channel
    //! identically to production.
    //!
    //! This is the only way to exhibit the demote-NodeFault-to-Cancelled
    //! bug in a test: it requires a `tokio::join!` whose combined future
    //! is `Pending` (some sibling is still awaiting) while one stage has
    //! produced a NodeFault. We achieve that by making one stage await
    //! `std::future::pending()` and the rest complete immediately —
    //! `tokio::join!` is then permanently `Pending`, and the cancel arm
    //! is the only escape.
    use super::*;
    use crate::block_validation::ValidationError;
    use irys_types::H256;
    use std::future::pending;

    fn node_fault_result() -> ValidationResult {
        ValidationError::TaskPanicked {
            task: "poa".to_string(),
            details: "verifier panic".to_string(),
        }
        .into()
    }

    fn invalid_result() -> ValidationResult {
        ValidationError::ShadowTransactionInvalid("consensus mismatch".to_string()).into()
    }

    fn soft_result() -> ValidationResult {
        ValidationError::ParentBlockMissing {
            block_hash: H256::zero(),
        }
        .into()
    }

    /// Race a six-stage `tokio::join!` (one stage permanently Pending
    /// to prevent natural completion) against an immediate-Ready cancel
    /// future under the SAME structural shape used in
    /// `validate_block`. Run the resulting outcome through the same
    /// side-channel read the production cancel arm performs.
    ///
    /// `stage_results` lists what each of the six stages produces. A
    /// stage with `None` is "permanently pending" (we install
    /// `std::future::pending()` for it). Stages with `Some(result)`
    /// complete immediately and write `result` to the side channel.
    async fn run_cancel_race(stage_results: [Option<ValidationResult>; 6]) -> ValidationResult {
        let captures: Arc<Mutex<StageFaultCaptures>> =
            Arc::new(Mutex::new(StageFaultCaptures::default()));

        // Build six stage futures. Each `Some(result)` stage merges
        // into the side channel before returning — same pattern as the
        // real stage bodies in `validate_block`. `None` stages await
        // `pending()` forever, ensuring `tokio::join!`'s combined
        // future is always `Pending` at the cancel-arm fire moment.
        let [s0, s1, s2, s3, s4, s5] = stage_results;
        let captures_for = |c: &Arc<Mutex<StageFaultCaptures>>| Arc::clone(c);
        let mk_stage = |result: Option<ValidationResult>,
                        captures: Arc<Mutex<StageFaultCaptures>>| async move {
            match result {
                Some(r) => capture_stage_result(&captures, r),
                None => pending::<ValidationResult>().await,
            }
        };

        let stage0 = mk_stage(s0, captures_for(&captures));
        let stage1 = mk_stage(s1, captures_for(&captures));
        let stage2 = mk_stage(s2, captures_for(&captures));
        let stage3 = mk_stage(s3, captures_for(&captures));
        let stage4 = mk_stage(s4, captures_for(&captures));
        let stage5 = mk_stage(s5, captures_for(&captures));

        let stages_join = async { tokio::join!(stage0, stage1, stage2, stage3, stage4, stage5) };

        // Immediate cancel — production uses `exit_if_block_is_too_old`
        // which returns `Cancelled(HeightDifference)` when the block
        // is too far behind tip. We mimic that synchronously.
        let cancel_future =
            async { ParentValidationResult::Cancelled(ValidationCancelReason::HeightDifference) };

        // Give the join future one chance to make progress before the
        // cancel fires by yielding once. Concretely: the stages
        // listed as `Some(_)` await zero await points before reaching
        // the side-channel write — they're immediately Ready on first
        // poll. We want to poll them once so they execute their
        // capture writes, THEN race the (already-Ready) cancel future.
        // `tokio::task::yield_now()` doesn't directly drive the join,
        // but `biased; stages_join => ...; cancel => ...` polls the
        // join first inside the select, so the immediate-Ready stages
        // are observed and their side-channel writes happen before
        // the cancel arm runs.
        let stage_outcome = tokio::select! {
            biased;
            joined = stages_join => StageOutcome::Joined(joined),
            cancel = cancel_future => StageOutcome::Cancelled(cancel),
        };

        match stage_outcome {
            // No `None` stage was supplied → all six completed
            // immediately under biased ordering. Run them through
            // `merge_stage_results` for symmetry with production.
            StageOutcome::Joined((a, b, c, d, e, f)) => {
                let arr = [&a, &b, &c, &d, &e, &f];
                merge_stage_results(&arr)
            }
            // Cancel won → produce identical synthesis to the
            // production cancel arm.
            StageOutcome::Cancelled(cancel) => {
                let captured = std::mem::take(
                    &mut *captures.lock().expect("StageFaultCaptures mutex poisoned"),
                );
                if let Some(result) = captured.into_cancel_result() {
                    return result;
                }
                cancel_outcome_to_result(cancel.into_cancel_reason())
            }
        }
    }

    /// Load-bearing test for the side channel. Stage 1 (PoA-shaped)
    /// produces a `TaskPanicked` NodeFault and writes it to the side
    /// channel BEFORE the cancel arm fires. Stage 2 is permanently Pending
    /// so `tokio::join!`'s combined future never reports Ready. Without
    /// the side channel (drain-poll-only approach), this case demotes
    /// the NodeFault to a cancellation outcome. With the side channel,
    /// the NodeFault wins.
    #[tokio::test]
    async fn node_fault_with_pending_sibling_surfaces_through_side_channel() {
        let result = run_cancel_race([
            Some(ValidationResult::Valid),
            Some(node_fault_result()),
            None, // permanently pending sibling — exhibits the cancel-race bug
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
        ])
        .await;

        match result {
            ValidationResult::InternalFailure(inner) => assert!(
                inner.is_node_fault(),
                "side channel must surface NodeFault even when join is Pending, got soft: {:?}",
                inner.err()
            ),
            other => panic!(
                "expected InternalFailure(node-fault), got {:?}. The cancel arm dropped the NodeFault — the side channel is not working.",
                other
            ),
        }
    }

    /// Companion to `node_fault_with_pending_sibling_surfaces_through_side_channel`:
    /// the side channel must surface ANY NodeFault variant under cancel,
    /// not only `TaskPanicked` (which the load-bearing test uses).
    /// Other production stages emit different NodeFault variants —
    /// shadow_tx produces `ShadowTxNodeFault` for snapshot underflow,
    /// the reth integration produces `ExecutionLayerTransportFailed`
    /// for engine-RPC transport hiccups, etc. A regression where the
    /// side channel only recognises `TaskPanicked` would silently
    /// demote these to soft cancellation outcomes under load.
    ///
    /// Parameterised over the canonical non-TaskPanicked NodeFault
    /// variants to assert the side-channel's NodeFault detection is
    /// variant-agnostic (i.e. delegates to `is_node_fault()` rather
    /// than matching specific variants).
    #[rstest::rstest]
    #[case::shadow_tx_node_fault(
        ValidationError::ShadowTxNodeFault("treasury underflow".to_string())
    )]
    #[case::execution_layer_transport(
        ValidationError::ExecutionLayerTransportFailed("engine rpc timeout".to_string())
    )]
    #[tokio::test]
    async fn non_task_panicked_node_fault_surfaces_through_side_channel(
        #[case] err: ValidationError,
    ) {
        // Sanity: the variant under test must actually be a NodeFault
        // (otherwise the test would pass for the wrong reason — the
        // side channel writes it but `into_cancel_result` filters by
        // priority tier and would skip a soft variant).
        let stage_result: ValidationResult = err.clone().into();
        let ValidationResult::InternalFailure(ref inner) = stage_result else {
            panic!(
                "test precondition: {err:?} must dispatch to InternalFailure, got {stage_result:?}",
            );
        };
        assert!(
            inner.is_node_fault(),
            "test precondition: {err:?} must classify as NodeFault for this test to be meaningful"
        );

        // Drive the same cancel race as the side-channel regression. Slot 2 is
        // permanently pending so `tokio::join!`'s combined future stays
        // Pending and the cancel arm is the only escape.
        let result = run_cancel_race([
            Some(ValidationResult::Valid),
            Some(stage_result),
            None,
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
        ])
        .await;

        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    inner.is_node_fault(),
                    "side channel must surface {:?} as NodeFault under cancel, got soft: {:?}",
                    err,
                    inner.err()
                );
                // The captured variant must be the SAME one the stage
                // emitted — the side channel's into_cancel_result must
                // preserve identity, not just any-NodeFault.
                assert_eq!(
                    std::mem::discriminant(inner.err()),
                    std::mem::discriminant(&err),
                    "side channel must preserve the original NodeFault variant; \
                     emitted {:?}, surfaced {:?}",
                    err,
                    inner.err()
                );
            }
            other => panic!(
                "non-TaskPanicked NodeFault wiring regression: expected \
                 InternalFailure(node-fault) for {err:?}, got {other:?}. The side \
                 channel's NodeFault detection is variant-specific instead of \
                 delegating to `is_node_fault()`."
            ),
        }
    }

    /// End-to-end wiring test for the panic-classification + side-channel
    /// path. Exercises the real chain: a `spawn_blocking` task panics,
    /// producing `JoinError::is_panic() = true`, which
    /// `classify_poa_join_error` translates into `TaskPanicked` (NodeFault),
    /// which `capture_stage_result` writes to the side channel. A
    /// concurrent cancel fires before the `tokio::join!` can naturally
    /// complete (sibling permanently Pending). The cancel arm must
    /// surface the NodeFault, NOT demote to Cancelled.
    ///
    /// This is the only test that exercises the FULL chain (real panic →
    /// real JoinError::is_panic → classify_poa_join_error → side channel →
    /// cancel-arm readback). The pieces are individually unit-tested in
    /// `classify_poa_join_error_tests`, `StageFaultCaptures` tests, and
    /// `node_fault_with_pending_sibling_surfaces_through_side_channel`,
    /// but only this test catches a regression where the wiring drifts
    /// (e.g. someone removes the `capture_stage_result` call in the PoA
    /// task's panic-arm).
    #[tokio::test]
    async fn poa_spawn_blocking_panic_surfaces_through_side_channel_under_cancel() {
        use tokio::sync::Notify;

        let captures: Arc<Mutex<StageFaultCaptures>> =
            Arc::new(Mutex::new(StageFaultCaptures::default()));
        let poa_captures = Arc::clone(&captures);
        // `panic_captured` gates the cancel arm: it fires AFTER the PoA
        // stage has written its NodeFault to the side channel, so the
        // race is deterministic (not timing-dependent). Without this
        // gate, `spawn_blocking`'s `.await` yields control and the
        // cancel arm can win before the capture lands — exercising a
        // DIFFERENT (and uninteresting) race.
        let panic_captured = Arc::new(Notify::new());
        let panic_captured_signal = Arc::clone(&panic_captured);

        // Stage 0 mirrors the real `poa_task` shape: spawn a blocking
        // task that panics, then convert the resulting `JoinError` via
        // `classify_poa_join_error` and write to the side channel before
        // returning. The stage's own future reaches `Ready` here, but
        // `tokio::join!`'s combined future stays `Pending` because
        // stage 2 is permanently parked.
        let poa_stage = async move {
            let handle = tokio::task::spawn_blocking(|| {
                panic!("simulated poa verifier panic for end-to-end wiring test");
            });
            let result: ValidationResult = match handle.await {
                Ok(()) => ValidationResult::Valid,
                Err(join_err) => classify_poa_join_error(&join_err).into(),
            };
            let captured = capture_stage_result(&poa_captures, result);
            panic_captured_signal.notify_one();
            captured
        };

        let mk_valid = || async { ValidationResult::Valid };
        let stages_join = async {
            tokio::join!(
                poa_stage,
                mk_valid(),
                pending::<ValidationResult>(),
                mk_valid(),
                mk_valid(),
                mk_valid(),
            )
        };

        // Cancel only after the PoA panic has been captured. This is the
        // exact race the side-channel fix defends: panic landed in side channel,
        // sibling permanently Pending blocks natural join completion,
        // cancel arm must surface the NodeFault from the side channel.
        let cancel_future = async move {
            panic_captured.notified().await;
            ParentValidationResult::Cancelled(ValidationCancelReason::HeightDifference)
        };

        let stage_outcome = tokio::select! {
            biased;
            joined = stages_join => StageOutcome::Joined(joined),
            cancel = cancel_future => StageOutcome::Cancelled(cancel),
        };

        let result = match stage_outcome {
            StageOutcome::Joined((a, b, c, d, e, f)) => {
                let arr = [&a, &b, &c, &d, &e, &f];
                merge_stage_results(&arr)
            }
            StageOutcome::Cancelled(cancel) => {
                let captured = std::mem::take(
                    &mut *captures.lock().expect("StageFaultCaptures mutex poisoned"),
                );
                captured
                    .into_cancel_result()
                    .unwrap_or_else(|| cancel_outcome_to_result(cancel.into_cancel_reason()))
            }
        };

        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    inner.is_node_fault(),
                    "real spawn_blocking panic must surface as NodeFault via side channel, got soft: {:?}",
                    inner.err()
                );
                assert!(
                    matches!(inner.err(), ValidationError::TaskPanicked { task, .. } if task == "poa"),
                    "TaskPanicked.task must be \"poa\", got {:?}",
                    inner.err()
                );
            }
            other => panic!(
                "wiring regression: real PoA panic + cancel must yield InternalFailure(node-fault, TaskPanicked), got {:?}. classify_poa_join_error or capture_stage_result is broken.",
                other
            ),
        }
    }

    /// Same shape as the load-bearing side-channel regression, but the early-completing stage
    /// produces an `Invalid` (consensus rejection) instead of a
    /// NodeFault. The side channel must surface this on cancel —
    /// the block IS bad and we observed that fact; reporting it as a
    /// cancellation would be peer-misattribution in the opposite
    /// direction (failing to peer-attribute a genuine consensus
    /// rejection).
    #[tokio::test]
    async fn invalid_with_pending_sibling_surfaces_through_side_channel() {
        let result = run_cancel_race([
            Some(ValidationResult::Valid),
            Some(invalid_result()),
            None,
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
        ])
        .await;

        match result {
            ValidationResult::Invalid(_) => {}
            other => panic!(
                "side channel must surface Invalid over cancellation, got {:?}",
                other
            ),
        }
    }

    /// Both a NodeFault AND an Invalid landed before the cancel arm
    /// fires, with a sibling permanently pending. Priority contract:
    /// NodeFault outranks Invalid — supervisor restart matters more
    /// than peer attribution.
    #[tokio::test]
    async fn node_fault_outranks_invalid_under_cancel() {
        let result = run_cancel_race([
            Some(invalid_result()),
            Some(node_fault_result()),
            None,
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
        ])
        .await;

        match result {
            ValidationResult::InternalFailure(inner) => assert!(
                inner.is_node_fault(),
                "NodeFault must outrank Invalid even under cancel: {:?}",
                inner.err()
            ),
            other => panic!("expected InternalFailure(node-fault), got {:?}", other),
        }
    }

    /// No NodeFault, no Invalid — only soft InternalFailure (which the
    /// side channel ignores) plus Pending siblings. Cancel wins and
    /// produces the cancellation outcome (routed through `InternalFailure`
    /// for the `HeightDifference` reason). Regression target: the side
    /// channel must NOT promote soft observations to cancel-arm overrides.
    #[tokio::test]
    async fn soft_internal_does_not_outrank_cancel() {
        let result = run_cancel_race([
            Some(soft_result()),
            None,
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
        ])
        .await;

        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    matches!(
                        inner.err(),
                        ValidationError::ValidationCancelled {
                            reason:
                                crate::block_validation::ValidationCancelReason::HeightDifference,
                        }
                    ),
                    "expected cancellation outcome, got {:?}",
                    inner.err()
                );
            }
            other => panic!(
                "expected InternalFailure(ValidationCancelled(HeightDifference)), got {:?}",
                other
            ),
        }
    }

    /// All six stages complete naturally (no Pending sibling); one
    /// produces `Invalid` so the natural merger runs through its
    /// failure branch and returns `Invalid`. Biased select picks the
    /// Joined branch — confirming the side channel does NOT corrupt
    /// the natural path (where the merger sees all six results and
    /// decides). Regression target: pure observation, no behavior
    /// change on the natural path.
    ///
    /// Picking a non-Valid stage avoids tripping the all-Valid
    /// `debug_assert!` inside `merge_stage_results` (the merger's
    /// caller invariant is that it's only called when at least one
    /// stage is non-Valid OR a cancel is present).
    #[tokio::test]
    async fn natural_completion_invalid_unchanged() {
        let result = run_cancel_race([
            Some(ValidationResult::Valid),
            Some(invalid_result()),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
            Some(ValidationResult::Valid),
        ])
        .await;

        match result {
            ValidationResult::Invalid(_) => {}
            other => panic!(
                "natural-completion failure path must yield Invalid via the merger, got {:?}",
                other
            ),
        }
    }
}
