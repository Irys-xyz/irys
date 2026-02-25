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
use eyre::{bail, ensure};
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, ExecutionPayloadCache,
    chain_sync_state::ChainSyncState,
};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{
    Config, IrysBlockHeader, SealedBlock, SendTraced as _, TokioServiceHandle, Traced,
    app_state::DatabaseProvider,
};
use irys_vdf::rayon;
use irys_vdf::state::{CancelEnum, VdfStateReadonly, vdf_steps_are_valid};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use reth::tasks::shutdown::Shutdown;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use tokio::{
    sync::{Notify, broadcast, mpsc::UnboundedReceiver},
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
    /// VDF task completion notifier
    vdf_notify: Arc<Notify>,
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
        let vdf_notify = Arc::new(Notify::new());

        let handle = runtime_handle.spawn(
            async move {
                let validation_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    reorg_rx,
                    vdf_notify: vdf_notify.clone(),
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
                    .start()
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
    #[tracing::instrument(level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("starting validation service");

        // Use the improved implementation
        let mut coordinator = active_validations::ValidationCoordinator::new(
            self.inner.block_tree_guard.clone(),
            Arc::clone(&self.vdf_notify),
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

                // Process VDF completions
                _ = self.vdf_notify.notified() => {
                    if let Some((hash, result)) = coordinator.process_vdf().await {
                        match result {
                            VdfValidationResult::Valid => {
                                // Valid VDF - task continues to concurrent validation
                            }
                            VdfValidationResult::Invalid(vdf_error) => {
                                error!(
                                    block.hash = %hash,
                                    custom.error = %vdf_error,
                                    "VDF validation failed"
                                );
                                // Send failure to block tree
                                if let Err(e) = self.inner.service_senders.block_tree.send_traced(
                                    crate::block_tree_service::BlockTreeServiceMessage::BlockValidationFinished {
                                        block_hash: hash,
                                        validation_result: ValidationResult::Invalid(ValidationError::VdfValidationFailed(vdf_error.to_string())),
                                    }
                                ) {
                                    error!(
                                        custom.error = ?e,
                                        "Failed to send VDF failure to block tree service"
                                    );
                                }
                            }
                            VdfValidationResult::Cancelled => {
                                // Cancelled tasks are re-queued internally, no action needed
                            }
                        }
                    }
                }

                // Process concurrent task completions (only if there are tasks)
                result = coordinator.concurrent_tasks.join_next_with_id(), if !coordinator.concurrent_tasks.is_empty() => {
                    match result {
                        Some(Ok((id, validation))) => {
                            coordinator.concurrent_task_blocks.remove(&id);

                            if let Err(e) = self.inner.service_senders.block_tree.send_traced(
                                crate::block_tree_service::BlockTreeServiceMessage::BlockValidationFinished {
                                    block_hash: validation.block_hash,
                                    validation_result: validation.validation_result,
                                }
                            ) {
                                error!(
                                    block.hash = %validation.block_hash,
                                    custom.error = ?e,
                                    "Failed to send validation result to block tree service"
                                );
                            }
                        }
                        Some(Err(e)) => {
                            let removed = coordinator.concurrent_task_blocks.remove(&e.id());
                            let message = if e.is_cancelled() {
                                "Concurrent validation task was cancelled"
                            } else {
                                "Concurrent validation task panicked"
                            };
                            error!(
                                block.hash = ?removed.as_ref(),
                                custom.error = %e,
                                message
                            );
                            if let Some(hash) = removed {
                                if let Err(send_err) = self.inner.service_senders.block_tree.send_traced(
                                    crate::block_tree_service::BlockTreeServiceMessage::BlockValidationFinished {
                                        block_hash: hash,
                                        validation_result: ValidationResult::Invalid(
                                            ValidationError::TaskPanicked {
                                                task: "concurrent_validation".to_string(),
                                                details: e.to_string(),
                                            },
                                        ),
                                    }
                                ) {
                                    error!(
                                        block.hash = %hash,
                                        custom.error = ?send_err,
                                        "Failed to send panic result to block tree service"
                                    );
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
                    let vdf_pending = coordinator.vdf_scheduler.pending.len();
                    let concurrent_active = coordinator.concurrent_tasks.len();

                    // TODO: this notify_one() is a quick and dirty fix for an issue this system has with it's usage of notify that we haven't been able to figure out (it causes stalls in the VDF validation)
                    self.vdf_notify.notify_one();

                    // Extract VDF task details if running
                    let (vdf_running, vdf_block_hash, vdf_block_height) =
                        if let Some(current_vdf) = &coordinator.vdf_scheduler.current {
                            (1, Some(current_vdf.hash), Some(current_vdf.sealed_block.header().height))
                        } else {
                            (0, None, None)
                        };

                    info!(
                        vdf.running = vdf_running,
                        vdf.vdf_block_hash = ?vdf_block_hash,
                        vdf.vdf_block_height = ?vdf_block_height,
                        vdf.pending = vdf_pending,
                        vdf.concurrent_active = concurrent_active,
                        "Validation pipeline status"
                    );
                    metrics::record_validation_pipeline(
                        vdf_pending as u64,
                        concurrent_active as u64,
                    );
                }
            }
        }

        info!("shutting down validation service");
        Ok(())
    }
}

impl ValidationServiceInner {
    #[tracing::instrument(level = "trace", skip_all, fields(%step=desired_step_number))]
    async fn wait_for_step_with_cancel(
        &self,
        desired_step_number: u64,
        cancel: Arc<AtomicU8>,
    ) -> eyre::Result<()> {
        let retries_per_second = 20;
        loop {
            if cancel.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8 {
                warn!(
                    vdf.desired_step = desired_step_number,
                    vdf.current_step = self.vdf_state.read().global_step,
                    "VDF validation cancelled while waiting for step"
                );
                bail!("Cancelled");
            }
            let read = self.vdf_state.read().global_step;

            if read >= desired_step_number {
                debug!("VDF Step is available");
                return Ok(());
            }
            debug!("Waiting for step");
            tokio::time::sleep(Duration::from_millis(1000 / retries_per_second)).await;
        }
    }

    /// Perform vdf fast forwarding and validation.
    /// If for some reason the vdf steps are invalid and / or don't match then the function will return an error
    #[tracing::instrument(level = "trace", err, skip_all, fields(block.hash = ?block.block_hash, block.height = ?block.height))]
    pub(crate) async fn ensure_vdf_is_valid(
        self: Arc<Self>,
        block: &IrysBlockHeader,
        cancel: Arc<AtomicU8>,
        skip_vdf_validation: bool,
    ) -> eyre::Result<()> {
        debug!("Verifying VDF info");

        let vdf_info = block.vdf_limiter_info.clone();

        // First, wait for the previous VDF step to be available
        let first_step_number = vdf_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        self.wait_for_step_with_cancel(prev_output_step_number, Arc::clone(&cancel))
            .await?;
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

        // Spawn VDF validation task unless skipping
        // Early guard: validate seeds against parent before heavy VDF work
        let vdf_reset_frequency = self.config.vdf.reset_frequency as u64;
        {
            let binding = self.block_tree_guard.read();
            let previous_block = binding
                .get_block(&block.previous_block_hash)
                .expect("previous block should exist");
            ensure!(
                matches!(
                    is_seed_data_valid(block, previous_block, vdf_reset_frequency),
                    crate::block_tree_service::ValidationResult::Valid
                ),
                "Seed data is invalid"
            );
        }

        // Spawn VDF validation task
        let vdf_ff = self.service_senders.vdf_fast_forward.clone();
        let vdf_state = self.vdf_state.clone();
        if !skip_vdf_validation {
            let vdf_info = vdf_info.clone();
            let this_inner = Arc::clone(&self);
            tokio::task::spawn_blocking(move || {
                vdf_steps_are_valid(
                    &this_inner.pool,
                    &vdf_info,
                    &this_inner.config.vdf,
                    &this_inner.vdf_state,
                    cancel,
                )
            })
            .await??;
        } else {
            debug!(
                block.hash = ?block.block_hash,
                "Skipping vdf_steps_are_valid for block"
            );
        }

        // Fast forward VDF steps
        fast_forward_vdf_steps_from_block(&vdf_info, &vdf_ff)?;
        vdf_state.wait_for_step(vdf_info.global_step_number).await;
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
