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
    block_tree_service::ReorgEvent, block_validation::is_seed_data_valid, services::ServiceSenders,
};
use active_validations::ActiveValidations;
use block_validation_task::BlockValidationTask;
use eyre::{bail, ensure};
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, ExecutionPayloadCache};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{app_state::DatabaseProvider, Config, IrysBlockHeader, TokioServiceHandle};
use irys_vdf::rayon;
use irys_vdf::state::{vdf_steps_are_valid, CancelEnum, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use reth::tasks::shutdown::Shutdown;
use std::{
    pin::pin,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{broadcast, mpsc::UnboundedReceiver},
    time::{interval, Duration},
};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

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
        block: Arc<IrysBlockHeader>,
        skip_vdf_validation: bool,
    },
}

/// Main validation service structure
pub struct ValidationService {
    /// Graceful shutdown handle
    shutdown: Shutdown,
    /// Message receiver
    msg_rx: UnboundedReceiver<ValidationServiceMessage>,
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
    /// Rayon thread pool that executes vdf steps
    pub(crate) pool: rayon::ThreadPool,
    /// Execution payload provider for shadow transaction validation
    pub(crate) execution_payload_provider: ExecutionPayloadCache,
    /// Toggle to enable/disable validation message processing
    pub validation_enabled: Arc<AtomicBool>,
}

impl ValidationService {
    /// Spawn a new validation service
    pub fn spawn_service(
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        vdf_state_readonly: VdfStateReadonly,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_node_adapter: IrysRethNodeAdapter,
        db: DatabaseProvider,
        execution_payload_provider: ExecutionPayloadCache,
        rx: UnboundedReceiver<ValidationServiceMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) -> (TokioServiceHandle, Arc<AtomicBool>) {
        info!("Spawning validation service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let config = config.clone();
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();
        let validation_enabled = Arc::new(AtomicBool::new(true));
        let validation_enabled_clone = validation_enabled.clone();

        let handle = runtime_handle.spawn(
            async move {
                let validation_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    reorg_rx,
                    inner: Arc::new(ValidationServiceInner {
                        pool: rayon::ThreadPoolBuilder::new()
                            .num_threads(config.consensus.vdf.parallel_verification_thread_limit)
                            .build()
                            .expect("to be able to build vdf validation pool"),
                        block_index_guard,
                        vdf_state: vdf_state_readonly,
                        config,
                        service_senders,
                        block_tree_guard,
                        reth_node_adapter,
                        db,
                        execution_payload_provider,
                        validation_enabled: validation_enabled_clone,
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
    #[tracing::instrument(skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("starting validation service");

        let mut active_validations =
            pin!(ActiveValidations::new(self.inner.block_tree_guard.clone()));

        // todo: add a notification system to the block tree service that'd
        // allow us to subscribe to each block status being updated. That could
        // act as a trigger point for re-evaluation. Rather than relying on a timer.
        let mut validation_timer = interval(Duration::from_millis(100));

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
                    // cancel the VDF task if it's `Some`
                     if let Some(task) = &active_validations.vdf_task {
                        task.cancel.store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
                    };
                    break;
                }

                // Receive new validation messages (only when validation is enabled)
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(ValidationServiceMessage::ValidateBlock { block, skip_vdf_validation }) => {
                            let Some(task) = self.inner.clone().create_validation_task(block, &active_validations, skip_vdf_validation) else {
                                // validation task was not created. The task failed during vdf validation
                                continue;
                            };

                            // push this task to the VDF pending queue
                            active_validations.vdf_pending_queue.push(task.block.block_hash, std::cmp::Reverse(task));
                        }
                        None => {
                            // Channel closed
                            warn!("receiver channel closed");
                            break;
                        }
                    }
                }

                // Process active validations every 100ms (only if not empty)
                _ = validation_timer.tick(), if !active_validations.is_empty()   => {

                    // Poll the VDF task & Process any completed validations (non-blocking)
                    let vdf_tasks_completed =  active_validations.process_completed_vdf().await;

                    let tasks_completed = active_validations.process_completed_concurrent().await;

                    if vdf_tasks_completed || tasks_completed {
                        // we may have unblocked one or more blocks from sending the validation message
                        validation_timer.reset();
                    }
                    // If no active validations and channel closed, exit
                    if active_validations.is_empty() && self.msg_rx.is_closed() {
                        break;
                    }
                }

                // Handle reorg events
                result = self.reorg_rx.recv() => {
                    match handle_broadcast_recv(result) {
                        Ok(Some(event)) => self.inner.handle_reorg(event, &mut active_validations).await,
                        // lagged, skipping messages
                        Ok(None) => { },
                        Err(_) => break,
                    }
                }
            }
        }

        // Drain remaining validations
        // This will only process the ones that are instantly ready to be validated.
        // If a task is awaiting on something and is not yet ready, it will be discarded.
        info!(
            "draining {} active validations before shutdown",
            active_validations.concurrent_len()
        );
        active_validations.process_completed_concurrent().await;

        info!("shutting down validation service");
        Ok(())
    }
}

impl ValidationServiceInner {
    /// Handle incoming messages
    #[instrument(skip_all, fields(block_hash, block_height))]
    fn create_validation_task(
        self: Arc<Self>,
        block: Arc<IrysBlockHeader>,
        active_validations: &ActiveValidations,
        skip_vdf_validation: bool,
    ) -> Option<BlockValidationTask> {
        let block_hash = block.block_hash;
        let block_height = block.height;

        tracing::Span::current().record("block_hash", tracing::field::display(&block_hash));
        tracing::Span::current().record("block_height", block_height);

        debug!("validating block");

        // schedule validation task
        let block_tree_guard = self.block_tree_guard.clone();

        let priority: std::cmp::Reverse<active_validations::BlockPriorityMeta> =
            active_validations.calculate_priority(&block);
        let task = BlockValidationTask::new(
            block,
            self,
            block_tree_guard,
            priority.0,
            skip_vdf_validation,
        );
        Some(task)
    }

    #[instrument(skip_all, fields(%step=desired_step_number))]
    async fn wait_for_step_with_cancel(
        &self,
        desired_step_number: u64,
        cancel: Arc<AtomicU8>,
    ) -> eyre::Result<()> {
        let retries_per_second = 20;
        loop {
            if cancel.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8 {
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
    #[tracing::instrument(err, skip_all, fields(block_hash = ?block.block_hash, block_height = ?block.height))]
    async fn ensure_vdf_is_valid(
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
        let vdf_reset_frequency = self.config.consensus.vdf.reset_frequency as u64;
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
                    &this_inner.config.consensus.vdf,
                    &this_inner.vdf_state,
                    cancel,
                )
            })
            .await??;
        } else {
            debug!(
                "Skipping vdf_steps_are_valid for block {:?}",
                block.block_hash
            );
        }

        // Fast forward VDF steps
        fast_forward_vdf_steps_from_block(&vdf_info, &vdf_ff)?;
        vdf_state.wait_for_step(vdf_info.global_step_number).await;
        Ok(())
    }

    /// Handle reorg events
    #[instrument(skip_all)]
    async fn handle_reorg(
        &self,
        event: ReorgEvent,
        active_validations: &mut std::pin::Pin<&mut ActiveValidations>,
    ) {
        info!(
            new_tip = ?event.new_tip,
            new_height = ?event.fork_parent.height,
            "Processing reorg in validation service"
        );

        // Reevaluate all block priorities based on the new canonical chain
        active_validations.reevaluate_priorities();

        info!("Validation service priorities updated after reorg");
    }
}

/// Handle broadcast channel receive results
#[instrument(skip_all, err)]
fn handle_broadcast_recv<T>(
    result: Result<T, broadcast::error::RecvError>,
) -> eyre::Result<Option<T>> {
    match result {
        Ok(event) => Ok(Some(event)),
        Err(broadcast::error::RecvError::Closed) => {
            eyre::bail!("broadcast channel closed")
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            warn!(skipped_messages = ?n, "reorg lagged");
            if n > 5 {
                error!("reorg channel significantly lagged");
            }
            Ok(None)
        }
    }
}
