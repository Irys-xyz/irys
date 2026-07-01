//! Validation service module.
//!
//! Actor-based service for validating blockchain blocks through multi-stage processing:
//! VDF verification, recall range validation, proof-of-access, and shadow transactions.
//!
//! ## Flow
//! 1. **VDF Validation**: Initial check using thread pool, fast-forward VDF state.
//!     Done using a priority-queue backed preemptible task slot
//! 2. **Task Creation**: Create BlockValidationTask, add to priority queue
//! 3. **Concurrent Validation**: Three concurrent stages (recall, POA, reth state)
//! 4. **Parent Dependencies**: Wait for parent validation before reporting
//!     results of a child block.
use crate::{
    block_tree_service::{ReorgEvent, ValidationResult},
    block_validation::ValidationError,
    mempool_guard::MempoolReadGuard,
    metrics,
    services::ServiceSenders,
};
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, ExecutionPayloadCache,
    chain_sync_state::ChainSyncState,
};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{
    BlockHash, Config, IrysBlockHeader, SealedBlock, SendTraced as _, TokioServiceHandle, Traced,
    app_state::DatabaseProvider,
};
use irys_vdf::state::{VdfStateReadonly, vdf_step_batch_is_valid};
use irys_vdf::verify::is_seed_data_valid;
use reth::tasks::shutdown::Shutdown;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio::{
    sync::{broadcast, mpsc::UnboundedReceiver},
    time::Duration,
};
use tracing::{Instrument as _, debug, error, info, warn};

mod active_validations;
mod block_validation_task;

#[derive(Debug)]
pub enum VdfValidationResult {
    Valid,
    Invalid(eyre::Report),
    Cancelled,
    /// Parent block was evicted from the block tree between VDF task queuing
    /// and Stage B (seed-data validation against parent). Same eviction race
    /// every other site in this branch classifies as `SoftInternal`; we
    /// surface it here instead of panicking inside `spawn_blocking`. The
    /// dispatch loop maps this to `ValidationError::ParentBlockMissing`,
    /// which routes through `is_internal_failure() = true` → the block parks
    /// for retry, no peer attribution.
    ParentMissing {
        parent_hash: BlockHash,
    },
}

/// Sentinel error returned from `ensure_vdf_is_valid` when Stage B observes
/// the parent absent from `block_tree`. Downcast in
/// `PreemptibleVdfTask::execute` and converted to
/// `VdfValidationResult::ParentMissing`. The prior shape `.expect`ed the
/// parent, panicking and triggering supervisor restart on what is a
/// recoverable eviction race; this sentinel surfaces it as SoftInternal.
#[derive(Debug, thiserror::Error)]
#[error("VDF Stage B: parent block {parent_hash} not found in block tree")]
pub(crate) struct VdfStageBParentMissing {
    pub parent_hash: BlockHash,
}

/// Sentinel error returned from `ensure_vdf_is_valid` when a `spawn_blocking`
/// task in Stage B or Stage C/D resolves as a `JoinError` (thread panic or
/// external `abort()`). Downcast in `PreemptibleVdfTask::execute` to split the
/// two cases: panics route to a local-fault `panic!` (never-mislabel rule);
/// external aborts route to `VdfValidationResult::Cancelled` (same requeue
/// lane as watchdog-triggered aborts). Without this sentinel the `JoinError`
/// falls through to the `None` arm and becomes `VdfValidationResult::Invalid`,
/// mislabelling a local infrastructure failure as a peer-attributable consensus
/// rejection.
#[derive(Debug, thiserror::Error)]
#[error(
    "VDF blocking task failed (stage={stage:?}, is_panic={is_panic}, is_cancelled={is_cancelled})"
)]
pub(crate) struct VdfBlockingTaskFailed {
    pub is_panic: bool,
    pub is_cancelled: bool,
    pub stage: VdfTaskStage,
}

/// Look up the parent for Stage B (seed-data validation) outside the blocking
/// closure. Returns a cloned `IrysBlockHeader` on hit, or the typed
/// `VdfStageBParentMissing` sentinel on miss. Factored out so the
/// lookup-then-route path can be unit-tested against a real `BlockTree`
/// without spinning up a full `ValidationServiceInner`.
pub(crate) fn lookup_stage_b_parent(
    block_tree_guard: &BlockTreeReadGuard,
    block: &IrysBlockHeader,
) -> Result<IrysBlockHeader, VdfStageBParentMissing> {
    let binding = block_tree_guard.read();
    binding
        .get_block(&block.previous_block_hash)
        .cloned()
        .ok_or(VdfStageBParentMissing {
            parent_hash: block.previous_block_hash,
        })
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VdfTaskStage {
    Starting = 0,
    WaitPrevStep = 1,
    ValidateSeeds = 2,
    ValidateBatch = 3,
    FastForwardBatch = 4,
    WaitFinalCatchUp = 5,
    Completed = 6,
}

impl VdfTaskStage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::WaitPrevStep => "wait_prev_step",
            Self::ValidateSeeds => "validate_seeds",
            Self::ValidateBatch => "validate_batch",
            Self::FastForwardBatch => "fast_forward_batch",
            Self::WaitFinalCatchUp => "wait_final_catch_up",
            Self::Completed => "completed",
        }
    }

    /// Stage label for the `irys.validation.stage_duration_ms` histogram.
    /// Prefixed with `vdf_` so VDF stages don't collide with the existing
    /// concurrent-validation labels (`seeds`, `recall_range`, `poa`,
    /// `shadow_tx`, `concurrent_overall`) recorded elsewhere.
    pub(crate) const fn metric_label(self) -> &'static str {
        match self {
            Self::Starting => "vdf_starting",
            Self::WaitPrevStep => "vdf_wait_prev_step",
            Self::ValidateSeeds => "vdf_validate_seeds",
            Self::ValidateBatch => "vdf_validate_batch",
            Self::FastForwardBatch => "vdf_fast_forward_batch",
            Self::WaitFinalCatchUp => "vdf_wait_final_catch_up",
            Self::Completed => "vdf_completed",
        }
    }
}

impl From<u8> for VdfTaskStage {
    fn from(raw: u8) -> Self {
        match raw {
            // weird pattern for single source of discriminant truth.
            // Unknown discriminants panic loudly: a new variant must add an arm here.
            x if x == Self::Starting as u8 => Self::Starting,
            x if x == Self::WaitPrevStep as u8 => Self::WaitPrevStep,
            x if x == Self::ValidateSeeds as u8 => Self::ValidateSeeds,
            x if x == Self::ValidateBatch as u8 => Self::ValidateBatch,
            x if x == Self::FastForwardBatch as u8 => Self::FastForwardBatch,
            x if x == Self::WaitFinalCatchUp as u8 => Self::WaitFinalCatchUp,
            x if x == Self::Completed as u8 => Self::Completed,
            other => panic!(
                "VdfTaskStage::from(u8) unknown discriminant {other} — \
                 a new VdfTaskStage variant needs an arm here"
            ),
        }
    }
}

impl From<VdfTaskStage> for u8 {
    fn from(stage: VdfTaskStage) -> Self {
        stage as Self
    }
}

fn vdf_task_progress_timeout(config: &Config) -> Duration {
    Duration::from_secs(config.vdf.progress_timeout_secs.max(1))
}

/// Mark the VDF task as having moved to a new stage and reset its progress
/// clock. Call this **only at real stage transitions** — never inside a loop
/// or as a "heartbeat" during work, because the pipeline watchdog
/// (`abort_stalled_current`) treats `progress_signal.elapsed() < timeout` as
/// proof that the task is making forward progress. Periodic calls inside a
/// stuck stage would silently defeat the watchdog.
///
/// Records the duration of the stage being LEFT to the
/// `irys.validation.stage_duration_ms` histogram with a `stage` label
/// (`vdf_<stage_name>`, prefixed so VDF stages don't collide with the
/// existing concurrent-validation labels like `seeds`/`poa`/`shadow_tx`).
/// The very first call records the spawn-to-first-progress latency under
/// the `vdf_starting` label.
///
/// Write order matters: refresh the `Instant` **before** publishing the new
/// stage. The watchdog reads stage first, then the `Instant`, so this ordering
/// guarantees that when the watchdog observes a watched (computational) stage
/// it pairs it with an `Instant` no older than the transition itself —
/// never with the previous stage's stale `Instant`.
pub(crate) fn record_vdf_task_progress(
    stage_signal: &Arc<AtomicU8>,
    progress_signal: &Arc<Mutex<Instant>>,
    stage: VdfTaskStage,
) {
    let previous_stage = VdfTaskStage::from(stage_signal.load(Ordering::Relaxed));
    let now = Instant::now();
    {
        let mut guard = progress_signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let elapsed_ms = now.saturating_duration_since(*guard).as_secs_f64() * 1000.0;
        metrics::record_validation_stage_duration_ms(previous_stage.metric_label(), elapsed_ms);
        *guard = now;
    }
    stage_signal.store(stage as u8, Ordering::Relaxed);
}

/// Messages that the validation service supports
#[derive(Debug)]
pub enum ValidationServiceMessage {
    /// Validate a block
    ValidateBlock {
        block: Arc<SealedBlock>,
        skip_vdf_validation: bool,
    },
}

/// Main validation service structure
pub struct ValidationService {
    /// Graceful shutdown handle
    shutdown: Shutdown,
    /// Message receiver
    msg_rx: UnboundedReceiver<Traced<ValidationServiceMessage>>,
    /// Reorg event receiver
    reorg_rx: broadcast::Receiver<ReorgEvent>,
    /// Inner service logic
    inner: Arc<ValidationServiceInner>,
}

/// Inner service structure containing business logic
pub(crate) struct ValidationServiceInner {
    /// Read only view of the block index
    pub(crate) block_index_guard: BlockIndexReadGuard,
    /// VDF steps read guard
    pub(crate) vdf_state: VdfStateReadonly,
    /// Reference to global config for node
    pub(crate) config: Config,
    /// Service channels
    pub(crate) service_senders: ServiceSenders,
    /// Reth node adapter for RPC calls
    pub(crate) reth_node_adapter: IrysRethNodeAdapter,
    /// Database provider for transaction lookups
    pub(crate) db: DatabaseProvider,
    /// Block tree read guard to get access to the canonical chain
    pub(crate) block_tree_guard: BlockTreeReadGuard,
    /// Read only view of the mempool state
    pub(crate) mempool_guard: MempoolReadGuard,
    /// Rayon thread pool that executes vdf steps
    pub(crate) pool: rayon::ThreadPool,
    /// Execution payload provider for shadow transaction validation
    pub(crate) execution_payload_provider: ExecutionPayloadCache,
    /// Toggle to enable/disable validation message processing
    pub validation_enabled: Arc<AtomicBool>,
    /// Chain sync state for recording diagnostic info
    pub(crate) chain_sync_state: ChainSyncState,
}

impl ValidationService {
    /// Spawn a new validation service
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_validation")]
    pub fn spawn_service(
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        mempool_guard: MempoolReadGuard,
        vdf_state_readonly: VdfStateReadonly,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_node_adapter: IrysRethNodeAdapter,
        db: DatabaseProvider,
        execution_payload_provider: ExecutionPayloadCache,
        rx: UnboundedReceiver<Traced<ValidationServiceMessage>>,
        runtime_handle: tokio::runtime::Handle,
        chain_sync_state: ChainSyncState,
    ) -> (TokioServiceHandle, Arc<AtomicBool>) {
        info!("Spawning validation service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let config = config.clone();
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();
        let validation_enabled = Arc::new(AtomicBool::new(true));
        let validation_enabled_clone = validation_enabled.clone();

        let rt_handle = runtime_handle.clone();
        let handle = runtime_handle.spawn(
            async move {
                let validation_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    reorg_rx,
                    inner: Arc::new(ValidationServiceInner {
                        pool: irys_vdf::build_verification_pool(&config.vdf),
                        block_index_guard,
                        vdf_state: vdf_state_readonly,
                        config,
                        service_senders,
                        block_tree_guard,
                        mempool_guard,
                        reth_node_adapter,
                        db,
                        execution_payload_provider,
                        validation_enabled: validation_enabled_clone,
                        chain_sync_state,
                    }),
                };

                validation_service
                    .start(rt_handle)
                    .in_current_span()
                    .await
                    .expect("validation service encountered an irrecoverable error")
            }
            .in_current_span(),
        );

        let service_handle = TokioServiceHandle {
            name: "validation_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        };

        (service_handle, validation_enabled)
    }

    /// Main service loop
    #[tracing::instrument(name = "validation_service_start", level = "trace", skip_all)]
    async fn start(mut self, runtime_handle: tokio::runtime::Handle) -> eyre::Result<()> {
        info!("starting validation service");

        let mut coordinator = active_validations::ValidationCoordinator::new(
            self.inner.block_tree_guard.clone(),
            runtime_handle,
        );

        // Create a timer for periodic pipeline logging
        let mut pipeline_log_interval = tokio::time::interval(Duration::from_secs(5));
        pipeline_log_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if !self.inner.validation_enabled.load(Ordering::Relaxed) {
                info!("Validation is disabled");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            tokio::select! {
                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for validation service");
                    break;
                }

                // Receive new validation messages
                traced = self.msg_rx.recv() => {
                    match traced {
                        Some(traced) => {
                            let (ValidationServiceMessage::ValidateBlock {
                                block,
                                skip_vdf_validation,
                            }, parent_span) = traced.into_parts();

                            let task = block_validation_task::BlockValidationTask::new(
                                block.clone(),
                                Arc::clone(&self.inner),
                                self.inner.block_tree_guard.clone(),
                                skip_vdf_validation,
                                parent_span,
                            );

                            coordinator.submit_task(task);
                        }
                        None => {
                            warn!("receiver channel closed");
                            break;
                        }
                    }
                }

                // Handle reorg events
                result = self.reorg_rx.recv() => {
                    match handle_broadcast_recv(result) {
                        Ok(Some(_event)) => {
                            coordinator.reevaluate_priorities();
                        }
                        Ok(None) => { },
                        Err(_) => break,
                    }
                }

                // Process VDF task completion
                (current, result) = coordinator.vdf_scheduler.poll_vdf() => {
                    match result {
                        Ok((hash, vdf_result, task)) => match vdf_result {
                            VdfValidationResult::Valid => {
                                metrics::record_validation_result("vdf", "valid");
                                coordinator.spawn_concurrent(task);
                            }
                            VdfValidationResult::Invalid(vdf_error) => {
                                metrics::record_validation_result("vdf", "invalid");
                                metrics::record_validation_full_duration_ms(
                                    task.enqueued_at.elapsed().as_secs_f64() * 1000.0,
                                );
                                error!(
                                    block.hash = %hash,
                                    custom.error = %vdf_error,
                                    "VDF validation failed"
                                );
                                vdf_terminal_finalize_via(
                                    &mut coordinator,
                                    &self.inner.service_senders.block_tree,
                                    hash,
                                    ValidationError::VdfValidationFailed(vdf_error.to_string()),
                                );
                            }
                            VdfValidationResult::Cancelled => {
                                metrics::record_validation_result("vdf", "cancelled");
                                // Preemption set the cancel signal, so the task
                                // exited cooperatively. Resubmit — submit_task
                                // recalculates priority (which may have changed
                                // due to reorgs) before re-entering the queue.
                                coordinator.submit_task(task);
                            }
                            VdfValidationResult::ParentMissing { parent_hash } => {
                                // Stage B observed the parent missing from
                                // block_tree (eviction race). Surface as
                                // ParentBlockMissing → SoftInternal so the
                                // block parks for retry and no peer attribution
                                // occurs. See `lookup_stage_b_parent` and the
                                // `seeds_validation_task` mirror path.
                                metrics::record_validation_result("vdf", "parent_missing");
                                metrics::record_validation_full_duration_ms(
                                    task.enqueued_at.elapsed().as_secs_f64() * 1000.0,
                                );
                                warn!(
                                    block.hash = %hash,
                                    block.parent_hash = %parent_hash,
                                    "VDF Stage B: parent missing from block tree (eviction race); routing as SoftInternal"
                                );
                                vdf_terminal_finalize_via(
                                    &mut coordinator,
                                    &self.inner.service_senders.block_tree,
                                    hash,
                                    ValidationError::ParentBlockMissing {
                                        block_hash: parent_hash,
                                    },
                                );
                            }
                        }
                        Err(join_error) => if join_error.is_panic() {
                            metrics::record_validation_result("vdf", "panicked");
                            if let Some(task) = &current.requeue_task {
                                metrics::record_validation_full_duration_ms(
                                    task.enqueued_at.elapsed().as_secs_f64() * 1000.0,
                                );
                            }
                            let panic = join_error.into_panic();
                            error!(
                                block.hash = %current.hash,
                                "VDF validation task panicked; aborting"
                            );
                            // Governing rule: NEVER mislabel a block as Valid or Invalid.
                            // Both misclassifications near-guarantee a fork.
                            //
                            // Validation panics inside `ensure_vdf_is_valid` are unreachable
                            // by construction (see comments at the reachable `expect` sites,
                            // e.g. the `get_step` `.expect` after `wait_for_step`). If we
                            // ever do panic here, we don't know whether the block is valid —
                            // master used to convert this to `Invalid(TaskPanicked)`, but
                            // surfacing a programmer-error as a data-level Invalid would
                            // drop a block every honest peer still accepts, forking us off
                            // the network. Crashing instead lets the supervisor restart us
                            // clean. See design/docs/vdf-validation-stall-detection.md.
                            std::panic::resume_unwind(panic);
                        } else {
                            // JoinError::Cancelled — the spawned future was aborted
                            // without going through the cooperative cancel signal.
                            // This shouldn't happen during normal operation: shutdown
                            // runs after the loop exits, and preemption uses the
                            // AtomicU8 signal, and watchdog-triggered aborts panic
                            // the service before reaching this handler. Requeue
                            // defensively to avoid silently dropping a block.
                            warn!(
                                block.hash = %current.hash,
                                custom.error = %join_error,
                                "VDF task unexpectedly cancelled, requeuing"
                            );
                            if let Some(task) = current.requeue_task {
                                coordinator.submit_task(task);
                            }
                        }
                    }
                    // Start next pending VDF task
                    coordinator.vdf_scheduler.start_next();
                }

                // Process concurrent task completions (only if there are tasks)
                result = coordinator.concurrent_tasks.join_next_with_id(), if !coordinator.concurrent_tasks.is_empty() => {
                    match result {
                        Some(Ok((id, validation))) => {
                            coordinator.concurrent_task_blocks.remove(&id);
                            // The block produced a verdict — clear any
                            // accumulated cancel-retry counter so a future
                            // resubmission of the same hash starts fresh
                            // (no stale state from a prior burst of cancels
                            // that didn't reach the cap before the run
                            // succeeded).
                            coordinator.clear_concurrent_cancel_retries(&validation.block_hash);
                            metrics::record_validation_full_duration_ms(
                                validation.enqueued_at.elapsed().as_secs_f64() * 1000.0,
                            );
                            self.send_validation_result(
                                validation.block_hash,
                                validation.validation_result,
                            );
                        }
                        Some(Err(e)) => {
                            let removed = coordinator.concurrent_task_blocks.remove(&e.id());
                            if e.is_cancelled() {
                                // JoinError::Cancelled here is unexpected: the
                                // only intentional cancel path is
                                // `ValidationCoordinator::shutdown()` via
                                // `concurrent_tasks.abort_all()`, which runs
                                // after the main loop has already broken on
                                // the shutdown signal — i.e. it cannot reach
                                // this arm. So a cancel landing here is a
                                // runtime hiccup (sibling worker panic,
                                // runtime tear-down race, etc.). The first
                                // few cancellations for a given block are
                                // transparently requeued (mirrors the VDF
                                // cancel arm's "defensive resubmit"); past
                                // `MAX_CONCURRENT_CANCEL_RETRIES` we stop
                                // looping and route the block out via
                                // `ValidationCancelReason::RepeatedCancellation`
                                // so a poisoned local condition (sibling-
                                // worker panic loop, runtime distress, etc.)
                                // can't tight-loop between the VDF queue and
                                // the JoinSet and starve other blocks. The
                                // give-up path parks the block as
                                // SoftInternal; fresh gossip is the recovery
                                // lane.
                                //
                                // Metric increments are routed by decision
                                // (requeued vs repeated) so operator dashboards
                                // can distinguish self-healing from terminal
                                // cap-hit park events.
                                if let Some((hash, enqueued_at, sealed_block, skip_vdf_validation)) = removed {
                                    let decision = coordinator.record_concurrent_cancel(hash);
                                    match decision {
                                        active_validations::ConcurrentCancelDecision::Requeue { attempt } => {
                                            metrics::record_validation_concurrent_cancel_requeued();
                                            warn!(
                                                block.hash = %hash,
                                                custom.error = %e,
                                                custom.metric = "validation_concurrent_cancel_requeued",
                                                retry.attempt = attempt,
                                                retry.max_attempts = active_validations::MAX_CONCURRENT_CANCEL_RETRIES,
                                                "Concurrent validation task unexpectedly cancelled; requeuing - sustained occurrence suggests Tokio runtime distress or external abort source"
                                            );
                                            // Do NOT fire `record_validation_full_duration_ms`
                                            // here — this validation is still in
                                            // flight. Firing now would double-count
                                            // when the requeued task finally
                                            // completes. Preserve `enqueued_at` so
                                            // the eventual completion records the
                                            // true gossip-to-completion latency
                                            // including the cancelled attempt.
                                            //
                                            // Resubmit through `submit_task` (which
                                            // enters the VDF queue first). Re-running
                                            // VDF for an already-VDF-validated block
                                            // is wasted work but cheap, and the cancel
                                            // could have fired at any stage — VDF
                                            // re-entry is the only path that covers
                                            // every possibility safely.
                                            //
                                            // Original `parent_span` (gossip trace
                                            // context) was consumed when the task was
                                            // first constructed; on requeue, attach
                                            // to the current span so the trace tree
                                            // continues from this handler rather than
                                            // dangling.
                                            let task = block_validation_task::BlockValidationTask::new_with_enqueued_at(
                                                sealed_block,
                                                Arc::clone(&self.inner),
                                                self.inner.block_tree_guard.clone(),
                                                skip_vdf_validation,
                                                tracing::Span::current(),
                                                enqueued_at,
                                            );
                                            coordinator.submit_task(task);
                                            // No `record_validation_finished` here:
                                            // we're keeping the validation in flight.
                                        }
                                        active_validations::ConcurrentCancelDecision::GiveUp { attempt } => {
                                            // Cap hit — stop looping on this
                                            // block. Route through
                                            // `RepeatedCancellation` (SoftInternal:
                                            // validity unknown, the cancel said
                                            // nothing about the peer's block, the
                                            // block parks for fresh gossip
                                            // recovery alongside other
                                            // SoftInternal discards).
                                            metrics::record_validation_concurrent_cancel_repeated();
                                            error!(
                                                block.hash = %hash,
                                                custom.error = %e,
                                                custom.metric = "validation_concurrent_cancel_repeated",
                                                retry.attempt = attempt,
                                                retry.max_attempts = active_validations::MAX_CONCURRENT_CANCEL_RETRIES,
                                                "Concurrent validation task cancelled past retry cap; routing as RepeatedCancellation SoftInternal — fresh gossip is the recovery lane"
                                            );
                                            // Fire the full-duration metric
                                            // now (validation is leaving the
                                            // active set — the cancelled
                                            // attempts collectively count
                                            // toward this final outcome).
                                            metrics::record_validation_full_duration_ms(
                                                enqueued_at.elapsed().as_secs_f64() * 1000.0,
                                            );
                                            // `_sealed_block` and
                                            // `_skip_vdf_validation` are not
                                            // needed on the give-up path —
                                            // we're not constructing another
                                            // BlockValidationTask. Dropping
                                            // them releases the Arc refcount.
                                            let _ = (sealed_block, skip_vdf_validation);
                                            // Mirror the panic-arm shape: route
                                            // through `.into()` so the From
                                            // dispatcher classifies as
                                            // InternalFailure SoftInternal.
                                            if !self.send_validation_result(
                                                hash,
                                                ValidationError::ValidationCancelled {
                                                    reason: crate::block_validation::ValidationCancelReason::RepeatedCancellation,
                                                }
                                                .into(),
                                            ) {
                                                // Block tree won't handle diagnostics since send failed,
                                                // so record directly as a fallback.
                                                self.inner.chain_sync_state.record_validation_finished(&hash);
                                                self.inner.chain_sync_state.record_block_validation_error(
                                                    format!("block={} error=concurrent task cancelled past retry cap: {}", hash, e),
                                                );
                                            }
                                        }
                                    }
                                } else {
                                    // No tuple in `concurrent_task_blocks` for
                                    // this id — the prior behaviour just
                                    // silently logged the cancel. Preserve
                                    // that since we have no hash to route on.
                                    warn!(
                                        custom.error = %e,
                                        custom.metric = "validation_concurrent_cancel_requeued",
                                        "Concurrent validation task unexpectedly cancelled but bookkeeping entry was already removed; nothing to requeue"
                                    );
                                }
                            } else {
                                error!(
                                    block.hash = ?removed.as_ref().map(|(h, _, _, _)| h),
                                    custom.error = %e,
                                    "Concurrent validation task panicked"
                                );
                                if let Some((hash, enqueued_at, _sealed_block, _skip_vdf)) = removed {
                                    // Panic terminates the attempt — clear
                                    // any accumulated cancel-retry counter
                                    // so a future resubmission starts fresh.
                                    coordinator.clear_concurrent_cancel_retries(&hash);
                                    metrics::record_validation_full_duration_ms(
                                        enqueued_at.elapsed().as_secs_f64() * 1000.0,
                                    );
                                    // Route through `.into()` so the From dispatcher
                                    // classifies TaskPanicked as InternalFailure
                                    // (validity unknown, do not peer-attribute).
                                    // Constructing `Invalid(TaskPanicked)` directly
                                    // here would defeat the seal.
                                    if !self.send_validation_result(
                                        hash,
                                        ValidationError::TaskPanicked {
                                            task: "concurrent_validation".to_string(),
                                            details: e.to_string(),
                                        }
                                        .into(),
                                    ) {
                                        // Block tree won't handle diagnostics since send failed,
                                        // so record directly as a fallback.
                                        self.inner.chain_sync_state.record_validation_finished(&hash);
                                        self.inner.chain_sync_state.record_block_validation_error(
                                            format!("block={} error=concurrent task panicked: {}", hash, e),
                                        );
                                    }
                                }
                            }
                        }
                        None => {
                            // This shouldn't happen when we check is_empty()
                            debug!("JoinSet returned None despite not being empty");
                        }
                    }
                }

                // Periodic pipeline state logging
                _ = pipeline_log_interval.tick() => {
                    if let Some((stalled_for, stage)) = coordinator
                        .vdf_scheduler
                        .abort_stalled_current(vdf_task_progress_timeout(&self.inner.config))
                    {
                        // Watchdog firing means a VDF task made no progress for
                        // `progress_timeout_secs` (default 15s). VDF difficulty is calibrated
                        // to ~1s/step/thread and a batch is clamped to 2×thread_count steps
                        // parallelised over thread_count rayon workers — so the legitimate
                        // worst case per batch is ~2s, leaving ~7× headroom. A trip here
                        // means a deadlock, a poisoned lock, or a runaway loop, not slow
                        // hardware.
                        //
                        // We panic instead of marking the block Invalid because of the
                        // never-mislabel rule: a stalled validation pipeline does not tell
                        // us whether the block is valid or invalid. Both misclassifications
                        // fork us off the network. Crashing is the only consensus-safe
                        // response — `setup_panic_hook` (in chain/src/main.rs) catches the
                        // panic, raises SIGINT for graceful shutdown, and arms a 45s
                        // force-abort watchdog so the supervisor restarts the node clean.
                        // The operator gets a loud crash signal; the underlying bug isn't
                        // re-triggered by the next block as a silent data-level failure.
                        // See design/docs/vdf-validation-stall-detection.md.
                        // SAFETY: abort_stalled_current(...) does not .await, and there is no other
                        // .await between that call and this re-read, so the watchdog cannot have been
                        // cleared by a concurrent task. Do not insert .await between these two points
                        // without revisiting this expect.
                        let current_vdf = coordinator
                            .vdf_scheduler
                            .current
                            .as_ref()
                            .expect("watchdog aborted a task; current must be Some");
                        let completed_ago = current_vdf
                            .completed_at
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .map(|t| t.elapsed());
                        let handle_finished = current_vdf.handle.is_finished();
                        metrics::record_validation_task_force_aborted(stage.metric_label());
                        panic!(
                            "Validation watchdog force-aborted stalled VDF task (block={}, height={}, stage={}, stalled_for={:?}, completed_ago={:?}, handle_finished={})",
                            current_vdf.hash,
                            current_vdf.sealed_block.header().height,
                            stage.as_str(),
                            stalled_for,
                            completed_ago,
                            handle_finished,
                        );
                    }

                    let vdf_pending = coordinator.vdf_scheduler.pending.len();
                    let concurrent_active = coordinator.concurrent_tasks.len();
                    let (by_priority, oldest_age_ms) = coordinator.pipeline_snapshot();

                    // Extract VDF task details if running
                    let (vdf_running, vdf_block_hash, vdf_block_height, vdf_stage, vdf_elapsed_ms, vdf_idle_ms) =
                        if let Some(current_vdf) = &coordinator.vdf_scheduler.current {
                            (
                                1,
                                Some(current_vdf.hash),
                                Some(current_vdf.sealed_block.header().height),
                                Some(
                                    VdfTaskStage::from(
                                        current_vdf.stage_signal.load(Ordering::Relaxed),
                                    )
                                    .as_str(),
                                ),
                                Some(current_vdf.started_at.elapsed().as_millis() as u64),
                                Some(
                                    current_vdf
                                        .last_progress_at
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .elapsed()
                                        .as_millis() as u64,
                                ),
                            )
                        } else {
                            (0, None, None, None, None, None)
                        };

                    info!(
                        vdf.running = vdf_running,
                        vdf.vdf_block_hash = ?vdf_block_hash,
                        vdf.vdf_block_height = ?vdf_block_height,
                        vdf.stage = ?vdf_stage,
                        vdf.elapsed_ms = ?vdf_elapsed_ms,
                        vdf.idle_ms = ?vdf_idle_ms,
                        vdf.pending = vdf_pending,
                        vdf.concurrent_active = concurrent_active,
                        vdf.queue_oldest_age_ms = oldest_age_ms,
                        "Validation pipeline status"
                    );
                    metrics::record_validation_queue_snapshot(
                        &by_priority,
                        concurrent_active as u64,
                        oldest_age_ms,
                    );
                }
            }
        }

        info!("shutting down validation service");
        coordinator.shutdown();
        Ok(())
    }

    /// Report a block's validation result to the block tree service.
    /// Returns `true` on success. On send failure, logs an error and returns `false`.
    ///
    /// PANICS on node-fault delivery failure: see [`send_validation_result_via`].
    fn send_validation_result(
        &self,
        block_hash: BlockHash,
        validation_result: ValidationResult,
    ) -> bool {
        send_validation_result_via(
            &self.inner.service_senders.block_tree,
            block_hash,
            validation_result,
        )
    }
}

/// Free-function form of `ValidationService::send_validation_result` so the
/// delivery-failure behaviour can be unit-tested without standing up a full
/// `ValidationService`.
///
/// PANICS on node-fault delivery failure: if the channel send fails AND the
/// unsent payload is an `InternalFailure` classified as a node fault, this
/// function panics immediately rather than returning `false`. The block-tree
/// handler (`on_block_validation_finished`) is the normal site that converts
/// node faults into a panic+SIGINT for graceful shutdown; if delivery to it
/// fails we must not silently swallow the fault and keep running. The panic
/// is caught by `setup_panic_hook` in `crates/chain/src/main.rs`, which
/// raises SIGINT for graceful shutdown. Same never-mislabel rationale as
/// the block-tree node-fault site — see `ValidationError::is_node_fault`.
fn send_validation_result_via(
    block_tree_sender: &UnboundedSender<Traced<crate::block_tree_service::BlockTreeServiceMessage>>,
    block_hash: BlockHash,
    validation_result: ValidationResult,
) -> bool {
    if let Err(e) = block_tree_sender.send_traced(
        crate::block_tree_service::BlockTreeServiceMessage::BlockValidationFinished {
            block_hash,
            validation_result,
        },
    ) {
        error!(
            block.hash = %block_hash,
            custom.error = ?e,
            "Failed to send validation result to block tree service"
        );
        // Recover the unsent payload from the send error so we can inspect
        // its classification. If it's a node fault, the block-tree handler
        // would have panicked on receipt — replicate that here so a dropped
        // channel can't mask a node fault into silent continuation.
        let (unsent_msg, _span) = e.0.into_parts();
        if let crate::block_tree_service::BlockTreeServiceMessage::BlockValidationFinished {
            validation_result: ValidationResult::InternalFailure(inner),
            ..
        } = &unsent_msg
            && inner.is_node_fault()
        {
            panic!(
                "validation result delivery failed for node-fault block (block={}, error={}); aborting node — see ValidationError::is_node_fault for rationale",
                block_hash, inner
            );
        }
        return false;
    }
    true
}

/// VDF terminal-arm housekeeping: clear the per-hash concurrent-cancel
/// retry counter (so a future fresh submission of the same hash starts
/// from zero) and dispatch the validation result. Shared by the VDF
/// `Invalid` and `ParentMissing` arms — both are terminal verdicts.
///
/// Mirrors the counter-clear at the concurrent-stage Ok arm and the
/// panic arm in the validation select loop. Without the counter clear, a
/// prior attempt's cancel counter would silently shorten the retry
/// budget for a future resubmission. See `concurrent_cancel_retries` and
/// `record_concurrent_cancel`.
///
/// Returns the same `bool` as the underlying `send_validation_result_via`
/// (true on successful dispatch, false otherwise). Panics on node-fault
/// delivery failure — see [`send_validation_result_via`].
pub(in crate::validation_service) fn vdf_terminal_finalize_via(
    coordinator: &mut active_validations::ValidationCoordinator,
    block_tree_sender: &UnboundedSender<Traced<crate::block_tree_service::BlockTreeServiceMessage>>,
    block_hash: BlockHash,
    error: ValidationError,
) -> bool {
    coordinator.clear_concurrent_cancel_retries(&block_hash);
    send_validation_result_via(block_tree_sender, block_hash, error.into())
}

impl ValidationServiceInner {
    /// Clamp the configured VDF validation batch size to `[thread_count, 2 * thread_count]`
    /// (and at least 1). Below the floor we leave worker threads idle; above the
    /// ceiling we stretch the gap between watchdog progress checks.
    fn clamped_validation_batch_size(&self) -> usize {
        let thread_count = self.config.vdf.parallel_verification_thread_limit;
        let configured_batch_size = self.config.vdf.validation_batch_size;
        let min_batch_size = thread_count;
        let max_batch_size = thread_count.saturating_mul(2);
        if configured_batch_size < min_batch_size {
            warn!(
                configured_batch_size,
                thread_count,
                clamped_to = min_batch_size,
                "vdf.validation_batch_size is lower than vdf.parallel_verification_thread_limit; clamping batch size up to thread count to avoid idle worker threads",
            );
        } else if configured_batch_size > max_batch_size {
            warn!(
                configured_batch_size,
                thread_count,
                clamped_to = max_batch_size,
                "vdf.validation_batch_size exceeds 2x vdf.parallel_verification_thread_limit; clamping batch size down to 2x thread count to keep watchdog progress checks tight",
            );
        }
        configured_batch_size
            .clamp(min_batch_size, max_batch_size)
            .max(1)
    }

    /// Perform vdf fast forwarding and validation.
    /// If for some reason the vdf steps are invalid and / or don't match then the function will return an error
    #[tracing::instrument(err, skip_all, fields(block.hash = ?block.block_hash, block.height = ?block.height))]
    pub(crate) async fn ensure_vdf_is_valid(
        self: Arc<Self>,
        block: &IrysBlockHeader,
        cancel: Arc<AtomicU8>,
        stage_signal: Arc<AtomicU8>,
        progress_signal: Arc<Mutex<Instant>>,
        completed_at_signal: Arc<Mutex<Option<Instant>>>,
        skip_vdf_validation: bool,
    ) -> eyre::Result<()> {
        let vdf_info = Arc::new(block.vdf_limiter_info.clone());
        let vdf_config = self.config.vdf.clone();
        let first_step_number = vdf_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        let progress_timeout = Duration::from_secs(vdf_config.progress_timeout_secs);
        let validation_batch_size = self.clamped_validation_batch_size();

        info!(
            vdf.first_step_number = first_step_number,
            vdf.global_step_number = vdf_info.global_step_number,
            vdf.prev_output_step_number = prev_output_step_number,
            vdf.local_step = self.vdf_state.current_step(),
            "ensure_vdf_is_valid: entered"
        );

        // Stage A: wait for the previous VDF step to be available locally
        record_vdf_task_progress(&stage_signal, &progress_signal, VdfTaskStage::WaitPrevStep);
        debug!(
            stage = "wait_prev_step",
            "ensure_vdf_is_valid: waiting for previous step"
        );
        let wait_started = Instant::now();
        let wait_result = self
            .vdf_state
            .wait_for_step(
                prev_output_step_number,
                Arc::clone(&cancel),
                progress_timeout,
            )
            .await;
        metrics::record_vdf_step_wait_duration_ms(wait_started.elapsed().as_secs_f64() * 1000.0);
        wait_result?;

        // Previous-step continuity is NOT re-checked against the local seed
        // buffer here. The real invariant — that this block's `prev_output`
        // equals its parent's `output` — is enforced block-rootedly by
        // `prev_output_is_valid` (crates/vdf/src/verify.rs) inside the mandatory
        // `prevalidate_block` pass, which runs before this task. Comparing
        // `prev_output` against the *buffer* instead would additionally reject a
        // canonical block whose steps diverge from a poisoned local buffer after
        // a deep reorg across a VDF reset boundary — a false rejection layered on
        // an invariant already proven. `wait_for_step` above is kept as the
        // liveness/sync gate (and its never-mislabel stall detection).

        // Stage B: validate seeds against parent (early guard before heavy VDF work)
        let vdf_reset_frequency = vdf_config.reset_frequency as u64;
        record_vdf_task_progress(&stage_signal, &progress_signal, VdfTaskStage::ValidateSeeds);
        debug!(
            stage = "validate_seeds",
            "ensure_vdf_is_valid: validating seed data against parent"
        );
        {
            // Lift the parent lookup OUT of the spawn_blocking closure.
            // If the parent has been evicted between VDF task queuing and now
            // (depth-prune / reorg eviction race), surface a typed sentinel
            // error rather than panicking inside the blocking task. The
            // sentinel is downcast in `PreemptibleVdfTask::execute` and
            // mapped to `VdfValidationResult::ParentMissing`, which the
            // dispatch loop routes via `ValidationError::ParentBlockMissing`
            // → `SoftInternal` (same shape as `seeds_validation_task` in the
            // concurrent stage).
            let previous_block = lookup_stage_b_parent(&self.block_tree_guard, block)?;
            let block_header = block.clone();
            match tokio::task::spawn_blocking(move || {
                is_seed_data_valid(&block_header, &previous_block, vdf_reset_frequency)?;
                Ok::<(), eyre::Report>(())
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(join_err) => {
                    return Err(VdfBlockingTaskFailed {
                        is_panic: join_err.is_panic(),
                        is_cancelled: join_err.is_cancelled(),
                        stage: VdfTaskStage::ValidateSeeds,
                    }
                    .into());
                }
            }
        }

        // Stage C/D: validate VDF steps in bounded batches and fast-forward
        // each validated prefix immediately.
        let vdf_ff = self.service_senders.vdf_fast_forward.clone();
        let total_batches = vdf_info.steps.len().div_ceil(validation_batch_size);
        for (batch_index, batch_steps) in vdf_info.steps.0.chunks(validation_batch_size).enumerate()
        {
            let batch_start_index = batch_index * validation_batch_size;
            let batch_start_step_number = first_step_number + batch_start_index as u64;
            let batch_end_step_number = batch_start_step_number + batch_steps.len() as u64 - 1;

            record_vdf_task_progress(&stage_signal, &progress_signal, VdfTaskStage::ValidateBatch);
            if skip_vdf_validation {
                debug!(
                    stage = VdfTaskStage::ValidateBatch.as_str(),
                    vdf.batch_index = batch_index + 1,
                    vdf.total_batches = total_batches,
                    vdf.batch_start_step = batch_start_step_number,
                    vdf.batch_end_step = batch_end_step_number,
                    "ensure_vdf_is_valid: skipping VDF batch validation"
                );
            } else {
                info!(
                    stage = VdfTaskStage::ValidateBatch.as_str(),
                    vdf.batch_index = batch_index + 1,
                    vdf.total_batches = total_batches,
                    vdf.batch_start_step = batch_start_step_number,
                    vdf.batch_end_step = batch_end_step_number,
                    "ensure_vdf_is_valid: validating VDF batch"
                );

                let batch_range = batch_start_index..batch_start_index + batch_steps.len();
                let is_final_batch = batch_end_step_number == vdf_info.global_step_number;
                let vdf_info_for_batch = Arc::clone(&vdf_info);
                let vdf_state_for_batch = self.vdf_state.clone();
                let vdf_config_for_batch = vdf_config.clone();
                let this_inner = Arc::clone(&self);
                let cancel_for_blocking = Arc::clone(&cancel);
                match tokio::task::spawn_blocking(move || {
                    vdf_step_batch_is_valid(
                        &this_inner.pool,
                        &vdf_info_for_batch,
                        &vdf_config_for_batch,
                        &vdf_state_for_batch,
                        batch_range,
                        is_final_batch,
                        cancel_for_blocking,
                    )
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(e),
                    Err(join_err) => {
                        return Err(VdfBlockingTaskFailed {
                            is_panic: join_err.is_panic(),
                            is_cancelled: join_err.is_cancelled(),
                            stage: VdfTaskStage::ValidateBatch,
                        }
                        .into());
                    }
                }
            }

            record_vdf_task_progress(
                &stage_signal,
                &progress_signal,
                VdfTaskStage::FastForwardBatch,
            );
            debug!(
                stage = VdfTaskStage::FastForwardBatch.as_str(),
                vdf.batch_index = batch_index + 1,
                vdf.total_batches = total_batches,
                vdf.batch_end_step = batch_end_step_number,
                "ensure_vdf_is_valid: enqueueing validated VDF batch for fast-forward"
            );
            vdf_ff
                .send_validated_batch(batch_start_step_number, batch_steps, progress_timeout)
                .await?;
        }

        record_vdf_task_progress(
            &stage_signal,
            &progress_signal,
            VdfTaskStage::WaitFinalCatchUp,
        );
        debug!(
            stage = VdfTaskStage::WaitFinalCatchUp.as_str(),
            vdf.global_step_number = vdf_info.global_step_number,
            "ensure_vdf_is_valid: waiting for fast-forward to reach block end"
        );
        let final_wait_started = Instant::now();
        let final_wait_result = self
            .vdf_state
            .wait_for_step(
                vdf_info.global_step_number,
                Arc::clone(&cancel),
                progress_timeout,
            )
            .await;
        metrics::record_vdf_step_wait_duration_ms(
            final_wait_started.elapsed().as_secs_f64() * 1000.0,
        );
        final_wait_result?;

        // Write completed_at before the stage atomic so a watchdog reading
        // stage == Completed is guaranteed to find completed_at = Some(_).
        // Mirrors the last_progress_at ordering invariant documented above
        // record_vdf_task_progress: progress_signal is written before the
        // stage atomic, and completed_at is written in that same window.
        *completed_at_signal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        record_vdf_task_progress(&stage_signal, &progress_signal, VdfTaskStage::Completed);
        info!(
            vdf.global_step_number = vdf_info.global_step_number,
            "ensure_vdf_is_valid: completed successfully"
        );
        Ok(())
    }
}

/// Handle broadcast channel receive results
#[tracing::instrument(level = "trace", skip_all, err)]
fn handle_broadcast_recv<T>(
    result: Result<T, broadcast::error::RecvError>,
) -> eyre::Result<Option<T>> {
    match result {
        Ok(event) => Ok(Some(event)),
        Err(broadcast::error::RecvError::Closed) => {
            eyre::bail!("broadcast channel closed")
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            warn!(custom.skipped_messages = ?n, "reorg lagged");
            if n > 5 {
                error!("reorg channel significantly lagged");
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{H256, H256List, NodeConfig, U256, VDFLimiterInfo};
    use irys_vdf::state::{CancelEnum, test_helpers::mocked_vdf_service};
    use irys_vdf::{VdfStep, step_number_to_salt_number, vdf_sha};

    fn build_vdf_info(num_steps: usize) -> (Config, VDFLimiterInfo) {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        node_config
            .consensus
            .get_mut()
            .vdf
            .num_checkpoints_in_vdf_step = 4;
        node_config.consensus.get_mut().vdf.reset_frequency = 10_000;
        let config = Config::new_with_random_peer_id(node_config);

        let prev_output = H256::from_low_u64_be(42);
        let mut seed = prev_output;
        let mut steps = Vec::with_capacity(num_steps);
        let mut last_step_checkpoints =
            vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        for step_number in 1..=num_steps as u64 {
            let salt = U256::from(step_number_to_salt_number(&config.vdf, step_number));
            let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
            vdf_sha(
                salt,
                &mut seed,
                config.vdf.num_checkpoints_in_vdf_step,
                config.vdf.num_iterations_per_checkpoint(),
                &mut checkpoints,
            );
            steps.push(seed);
            last_step_checkpoints = checkpoints;
        }

        (
            config,
            VDFLimiterInfo {
                output: *steps.last().expect("at least one step"),
                global_step_number: num_steps as u64,
                seed: H256::from_low_u64_be(7),
                next_seed: H256::from_low_u64_be(8),
                prev_output,
                last_step_checkpoints: H256List(last_step_checkpoints),
                steps: H256List(steps),
                vdf_difficulty: Some(1),
                next_vdf_difficulty: Some(1),
            },
        )
    }

    #[test_log::test(tokio::test)]
    async fn invalid_batch_does_not_fast_forward_any_steps() {
        let (config, mut vdf_info) = build_vdf_info(2);
        vdf_info.steps.0[1] = H256::from_low_u64_be(999);
        vdf_info.output = vdf_info.steps[1];

        let vdf_state = VdfStateReadonly::new(mocked_vdf_service(&config));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("thread pool");
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<Traced<VdfStep>>(4);
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

        let result = vdf_step_batch_is_valid(
            &pool,
            &vdf_info,
            &config.vdf,
            &vdf_state,
            0..vdf_info.steps.len(),
            true,
            cancel,
        );

        assert!(result.is_err(), "invalid batch must fail validation");
        assert!(
            rx.try_recv().is_err(),
            "no fast-forward steps should be emitted for an invalid batch"
        );
    }

    /// On send failure, a node-fault `InternalFailure` payload must trigger a
    /// local panic so the supervisor restarts the node — the block-tree
    /// handler would have panicked on receipt and we must not silently swallow
    /// the fault when the channel is dead. See `ValidationError::is_node_fault`.
    #[test]
    #[should_panic(expected = "validation result delivery failed for node-fault block")]
    fn send_validation_result_panics_on_node_fault_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
            Traced<crate::block_tree_service::BlockTreeServiceMessage>,
        >();
        // Drop the receiver so the send fails.
        drop(rx);

        let block_hash = H256::from_low_u64_be(1);
        // TaskPanicked routes through `From<ValidationError>` to
        // `ValidationResult::InternalFailure` and is classified as a node fault
        // (verifier thread crashed).
        let node_fault: ValidationResult = ValidationError::TaskPanicked {
            task: "concurrent_validation".to_string(),
            details: "boom".to_string(),
        }
        .into();
        assert!(
            matches!(&node_fault, ValidationResult::InternalFailure(inner) if inner.is_node_fault()),
            "precondition: TaskPanicked must classify as node-fault InternalFailure"
        );

        let _ = super::send_validation_result_via(&tx, block_hash, node_fault);
    }

    /// A non-node-fault `InternalFailure` (e.g. shadow-tx generation hit a
    /// mempool race) must NOT panic on delivery failure — these are soft
    /// retry-plausible failures, and the recovery path is leaving the block
    /// in cache for re-evaluation rather than crashing the node.
    #[test]
    fn send_validation_result_returns_false_for_soft_internal_failure_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
            Traced<crate::block_tree_service::BlockTreeServiceMessage>,
        >();
        drop(rx);

        let block_hash = H256::from_low_u64_be(2);
        // ValidationCancelled(HeightDifference) is `is_internal_failure() = true` but
        // `is_node_fault() = false` — a soft local failure.
        let soft_internal: ValidationResult = ValidationError::ValidationCancelled {
            reason: crate::block_validation::ValidationCancelReason::HeightDifference,
        }
        .into();
        assert!(
            matches!(&soft_internal, ValidationResult::InternalFailure(inner) if !inner.is_node_fault()),
            "precondition: ValidationCancelled(HeightDifference) must classify as non-node-fault InternalFailure"
        );

        let result = super::send_validation_result_via(&tx, block_hash, soft_internal);
        assert!(
            !result,
            "send failure must surface as `false`, not panic, for soft internal failures"
        );
    }

    /// A consensus-rejection (`Invalid`) payload must NOT panic on delivery
    /// failure — the block is genuinely bad, the dropped channel is a
    /// liveness/shutdown concern handled separately by the caller.
    #[test]
    fn send_validation_result_returns_false_for_invalid_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
            Traced<crate::block_tree_service::BlockTreeServiceMessage>,
        >();
        drop(rx);

        let block_hash = H256::from_low_u64_be(3);
        // ShadowTransactionInvalid is `is_internal_failure() = false` →
        // routes to `ValidationResult::Invalid`.
        let invalid: ValidationResult =
            ValidationError::ShadowTransactionInvalid("bad tx".to_string()).into();
        assert!(
            matches!(&invalid, ValidationResult::Invalid(_)),
            "precondition: ShadowTransactionInvalid must classify as Invalid"
        );

        let result = super::send_validation_result_via(&tx, block_hash, invalid);
        assert!(
            !result,
            "send failure must surface as `false`, not panic, for consensus rejections"
        );
    }

    #[test_log::test(tokio::test)]
    async fn valid_batch_fast_forwards_only_validated_prefix() {
        let (config, vdf_info) = build_vdf_info(3);
        let vdf_state = VdfStateReadonly::new(mocked_vdf_service(&config));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("thread pool");
        let (tx, mut rx) = irys_vdf::fast_forward_channel();
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

        vdf_step_batch_is_valid(
            &pool,
            &vdf_info,
            &config.vdf,
            &vdf_state,
            0..1,
            false,
            cancel,
        )
        .expect("valid prefix should be accepted");

        tx.send_validated_batch(1, &vdf_info.steps.0[..1], Duration::from_secs(1))
            .await
            .expect("validated prefix should fast-forward");
        let (ff_step, _span) = rx
            .recv()
            .await
            .expect("validated prefix should emit one fast-forward step")
            .into_parts();
        assert_eq!(ff_step.global_step_number, 1);
        assert_eq!(ff_step.step, vdf_info.steps[0]);
        assert!(
            rx.try_recv().is_err(),
            "only the validated prefix should be emitted"
        );
    }

    /// Stage B parent-missing regression: when the parent is absent from
    /// `block_tree` at Stage B entry, the lookup helper must return the
    /// typed `VdfStageBParentMissing` sentinel instead of panicking. The
    /// wider pipeline maps this to `VdfValidationResult::ParentMissing` →
    /// `ValidationError::ParentBlockMissing` → `SoftInternal`. Before the
    /// fix, the lookup ran inside `spawn_blocking` with `.expect("previous block
    /// should exist")` and panicked the node on a recoverable eviction race.
    mod stage_b_parent_lookup {
        use super::*;
        use irys_domain::BlockTree;
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{ConsensusConfig, IrysBlockHeader};
        use std::sync::RwLock;

        fn signed_genesis() -> IrysBlockHeader {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = 0;
            header.poa.chunk = Some(Default::default());
            header.test_sign();
            header
        }

        fn empty_block_tree() -> BlockTreeReadGuard {
            let genesis = signed_genesis();
            let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
            BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)))
        }

        #[test]
        fn returns_sentinel_when_parent_absent() {
            let block_tree = empty_block_tree();

            // Child block whose parent hash isn't in the tree (genesis isn't
            // the parent we're pointing at).
            let mut child = IrysBlockHeader::new_mock_header();
            child.height = 5;
            child.previous_block_hash = H256::from_low_u64_be(0xdead);

            let err = lookup_stage_b_parent(&block_tree, &child)
                .expect_err("missing parent must surface the typed sentinel, not panic");

            assert_eq!(
                err.parent_hash, child.previous_block_hash,
                "sentinel must carry the parent hash so the dispatch loop can build the ParentBlockMissing error"
            );
        }

        #[test]
        fn returns_cloned_parent_when_present() {
            // Genesis is present in the tree; build a child whose parent IS
            // genesis. Lookup must hit and return the cloned header.
            let genesis = signed_genesis();
            let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
            let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));

            let mut child = IrysBlockHeader::new_mock_header();
            child.height = 1;
            child.previous_block_hash = genesis.block_hash;

            let parent = lookup_stage_b_parent(&block_tree, &child)
                .expect("parent present in tree must return Ok");
            assert_eq!(parent.block_hash, genesis.block_hash);
            assert_eq!(parent.height, genesis.height);
        }

        /// Direct round-trip: confirm the sentinel error converts cleanly to
        /// `eyre::Report` (the actual return shape of `ensure_vdf_is_valid`)
        /// and downcasts back to the typed sentinel so
        /// `PreemptibleVdfTask::execute` can route it.
        #[test]
        fn sentinel_round_trips_through_eyre_report() {
            let parent_hash = H256::from_low_u64_be(0xbeef);
            let original = VdfStageBParentMissing { parent_hash };

            // Wrap via `.into()` exactly as `ensure_vdf_is_valid` does.
            let report: eyre::Report = original.into();
            let recovered = report
                .downcast_ref::<VdfStageBParentMissing>()
                .expect("typed sentinel must survive eyre boxing");
            assert_eq!(recovered.parent_hash, parent_hash);
        }
    }
}
