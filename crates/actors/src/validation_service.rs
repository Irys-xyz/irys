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
    block_validation::{ValidationError, is_seed_data_valid},
    mempool_guard::MempoolReadGuard,
    metrics,
    services::ServiceSenders,
};
use eyre::ensure;
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, ExecutionPayloadCache,
    chain_sync_state::ChainSyncState,
};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{
    BlockHash, Config, IrysBlockHeader, SealedBlock, SendTraced as _, TokioServiceHandle, Traced,
    app_state::DatabaseProvider,
};
use irys_vdf::rayon;
use irys_vdf::state::{VdfStateReadonly, vdf_step_batch_is_valid};
use irys_vdf::vdf_utils::fast_forward_validated_steps;
use reth::tasks::shutdown::Shutdown;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::time::Instant;
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
            // weird pattern for single source of discriminant truth
            x if x == Self::WaitPrevStep as u8 => Self::WaitPrevStep,
            x if x == Self::ValidateSeeds as u8 => Self::ValidateSeeds,
            x if x == Self::ValidateBatch as u8 => Self::ValidateBatch,
            x if x == Self::FastForwardBatch as u8 => Self::FastForwardBatch,
            x if x == Self::WaitFinalCatchUp as u8 => Self::WaitFinalCatchUp,
            x if x == Self::Completed as u8 => Self::Completed,
            _ => Self::Starting,
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
fn record_vdf_task_progress(
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
                        pool: rayon::ThreadPoolBuilder::new()
                            .num_threads(config.vdf.parallel_verification_thread_limit)
                            .build()
                            .expect("to be able to build vdf validation pool"),
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
                                self.send_validation_result(hash, ValidationResult::Invalid(
                                    ValidationError::VdfValidationFailed(vdf_error.to_string())
                                ));
                            }
                            VdfValidationResult::Cancelled => {
                                metrics::record_validation_result("vdf", "cancelled");
                                // Preemption set the cancel signal, so the task
                                // exited cooperatively. Resubmit — submit_task
                                // recalculates priority (which may have changed
                                // due to reorgs) before re-entering the queue.
                                coordinator.submit_task(task);
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
                            let message = if e.is_cancelled() {
                                "Concurrent validation task was cancelled"
                            } else {
                                "Concurrent validation task panicked"
                            };
                            error!(
                                block.hash = ?removed.as_ref().map(|(h, _)| h),
                                custom.error = %e,
                                message
                            );
                            if let Some((hash, enqueued_at)) = removed {
                                metrics::record_validation_full_duration_ms(
                                    enqueued_at.elapsed().as_secs_f64() * 1000.0,
                                );
                                if !self.send_validation_result(hash, ValidationResult::Invalid(
                                    ValidationError::TaskPanicked {
                                        task: "concurrent_validation".to_string(),
                                        details: e.to_string(),
                                    },
                                )) {
                                    // Block tree won't handle diagnostics since send failed,
                                    // so record directly as a fallback.
                                    self.inner.chain_sync_state.record_validation_finished(&hash);
                                    self.inner.chain_sync_state.record_block_validation_error(
                                        format!("block={} error=concurrent task panicked: {}", hash, e),
                                    );
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
                        let current_vdf = coordinator
                            .vdf_scheduler
                            .current
                            .as_ref()
                            .expect("watchdog aborted a task; current must be Some");
                        metrics::record_validation_task_force_aborted(stage.metric_label());
                        panic!(
                            "Validation watchdog force-aborted stalled VDF task (block={}, height={}, stage={}, stalled_for={:?})",
                            current_vdf.hash,
                            current_vdf.sealed_block.header().height,
                            stage.as_str(),
                            stalled_for,
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
    fn send_validation_result(
        &self,
        block_hash: BlockHash,
        validation_result: ValidationResult,
    ) -> bool {
        if let Err(e) = self.inner.service_senders.block_tree.send_traced(
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
            return false;
        }
        true
    }
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
            vdf.local_step = self.vdf_state.read().global_step,
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

        // Unreachable in practice: `wait_for_step` above only returns Ok once
        // `global_step >= prev_output_step_number`, so the step is in the seed
        // buffer. The buffer can in principle have trimmed past
        // `prev_output_step_number` if a canonical-tip update advanced
        // `minimum_step_to_keep` during the sub-millisecond window after
        // `wait_for_step` returned — but the buffer's capacity is at minimum
        // `max_allowed_vdf_fork_steps` (≥60k for production), so trimming
        // requires ~60k steps of canonical advancement in that window. That
        // doesn't happen.
        //
        // If it ever does fire, the panic is intentional. We must not
        // downgrade this to an `Invalid` result — see the never-mislabel
        // rule documented at the `resume_unwind` site in the select loop
        // and in design/docs/vdf-validation-stall-detection.md.
        let stored_previous_step = self
            .vdf_state
            .get_step(prev_output_step_number)
            .expect("to get the step, since we've just waited for it");

        ensure!(
            stored_previous_step == vdf_info.prev_output,
            "vdf output is not equal to the saved step with the same index {:?}, got {:?}",
            stored_previous_step,
            vdf_info.prev_output,
        );

        // Stage B: validate seeds against parent (early guard before heavy VDF work)
        let vdf_reset_frequency = vdf_config.reset_frequency as u64;
        record_vdf_task_progress(&stage_signal, &progress_signal, VdfTaskStage::ValidateSeeds);
        debug!(
            stage = "validate_seeds",
            "ensure_vdf_is_valid: validating seed data against parent"
        );
        {
            let block_tree_guard = self.block_tree_guard.clone();
            let block_header = block.clone();
            tokio::task::spawn_blocking(move || {
                let binding = block_tree_guard.read();
                let previous_block = binding
                    .get_block(&block_header.previous_block_hash)
                    .expect("previous block should exist");
                ensure!(
                    matches!(
                        is_seed_data_valid(&block_header, previous_block, vdf_reset_frequency),
                        crate::block_tree_service::ValidationResult::Valid
                    ),
                    "Seed data is invalid"
                );
                Ok::<(), eyre::Report>(())
            })
            .await??;
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
                tokio::task::spawn_blocking(move || {
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
                .await??;
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
            fast_forward_validated_steps(
                batch_start_step_number,
                batch_steps,
                &vdf_ff,
                progress_timeout,
            )
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

    #[test_log::test(tokio::test)]
    async fn valid_batch_fast_forwards_only_validated_prefix() {
        let (config, vdf_info) = build_vdf_info(3);
        let vdf_state = VdfStateReadonly::new(mocked_vdf_service(&config));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("thread pool");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Traced<VdfStep>>(4);
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

        fast_forward_validated_steps(1, &vdf_info.steps.0[..1], &tx, Duration::from_secs(1))
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
}
