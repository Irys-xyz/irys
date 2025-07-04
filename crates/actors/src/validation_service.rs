//! Validation service module.
//!
//! Actor-based service for validating blockchain blocks through multi-stage processing:
//! VDF verification, recall range validation, proof-of-access, and shadow transactions.
//!
//! ## Flow
//! 1. **VDF Validation**: Initial check using thread pool, fast-forward VDF state.
//!     Always done immediately for every block that's provided.
//! 2. **Task Creation**: Create BlockValidationTask, add to priority queue
//! 3. **Parallel Validation**: Three concurrent stages (recall, POA, reth state)
//! 4. **Parent Dependencies**: Wait for parent validation before reporting
//!     results of a child block.

use crate::block_tree_service::BlockTreeReadGuard;
use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeServiceMessage, ReorgEvent, ValidationResult},
    block_validation::PayloadProvider,
    epoch_service::PartitionAssignmentsReadGuard,
    services::ServiceSenders,
};
use active_validations::ActiveValidations;
use block_validation_task::BlockValidationTask;
use eyre::ensure;
use futures::FutureExt as _;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{app_state::DatabaseProvider, Config, IrysBlockHeader};
use irys_vdf::rayon;
use irys_vdf::state::{vdf_steps_are_valid, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::{
    pin::pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{broadcast, mpsc::UnboundedReceiver},
    task::JoinHandle,
    time::{interval, Duration},
};
use tracing::{debug, error, info, instrument, warn};

mod active_validations;
mod block_validation_task;

/// Messages that the validation service supports
#[derive(Debug)]
pub enum ValidationServiceMessage {
    /// Validate a block
    ValidateBlock { block: Arc<IrysBlockHeader> },
}

/// Main validation service structure
pub struct ValidationService<T: PayloadProvider> {
    /// Graceful shutdown handle
    shutdown: GracefulShutdown,
    /// Message receiver
    msg_rx: UnboundedReceiver<ValidationServiceMessage>,
    /// Reorg event receiver
    reorg_rx: broadcast::Receiver<ReorgEvent>,
    /// Inner service logic
    inner: Arc<ValidationServiceInner<T>>,
}

/// Inner service structure containing business logic
pub(crate) struct ValidationServiceInner<T: PayloadProvider> {
    /// Read only view of the block index
    pub(crate) block_index_guard: BlockIndexReadGuard,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    pub(crate) partition_assignments_guard: PartitionAssignmentsReadGuard,
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
    pub(crate) execution_payload_provider: T,
    /// Toggle to enable/disable validation message processing
    pub validation_enabled: Arc<AtomicBool>,
}

impl<T: PayloadProvider> ValidationService<T> {
    /// Spawn a new validation service
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        partition_assignments_guard: PartitionAssignmentsReadGuard,
        vdf_state_readonly: VdfStateReadonly,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_node_adapter: IrysRethNodeAdapter,
        db: DatabaseProvider,
        execution_payload_provider: T,
        rx: UnboundedReceiver<ValidationServiceMessage>,
    ) -> (JoinHandle<()>, Arc<AtomicBool>) {
        let config = config.clone();
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();
        let validation_enabled = Arc::new(AtomicBool::new(true));
        let validation_enabled_clone = validation_enabled.clone();

        let handle = exec.spawn_critical_with_graceful_shutdown_signal(
            "Validation Service",
            |shutdown| async move {
                let validation_service = Self {
                    shutdown,
                    msg_rx: rx,
                    reorg_rx,
                    inner: Arc::new(ValidationServiceInner {
                        pool: rayon::ThreadPoolBuilder::new()
                            .num_threads(config.consensus.vdf.parallel_verification_thread_limit)
                            .build()
                            .expect("to be able to build vdf validation pool"),
                        block_index_guard,
                        partition_assignments_guard,
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
                    .await
                    .expect("validation service encountered an irrecoverable error")
            },
        );

        (handle, validation_enabled)
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
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            tokio::select! {
                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    break;
                }

                // Receive new validation messages (only when validation is enabled)
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            // Transform message to validation future
                            let Some(task) = self.inner.clone().create_validation_future(msg).await else {
                                // validation future was not created. The task failed during vdf validation
                                continue;
                            };
                            active_validations.push(task.block_hash, task.execute().boxed());
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
                    // Process any completed validations (non-blocking)
                    let tasks_completed = active_validations.process_completed().await;
                    if tasks_completed {
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
            active_validations.len()
        );
        active_validations.process_completed().await;

        info!("shutting down validation service");
        Ok(())
    }
}

impl<T: PayloadProvider> ValidationServiceInner<T> {
    /// Handle incoming messages
    #[instrument(skip_all, fields(block_hash, block_height))]
    async fn create_validation_future(
        self: Arc<Self>,
        msg: ValidationServiceMessage,
    ) -> Option<BlockValidationTask<T>> {
        match msg {
            ValidationServiceMessage::ValidateBlock { block } => {
                let block_hash = block.block_hash;
                let block_height = block.height;

                tracing::Span::current().record("block_hash", tracing::field::display(&block_hash));
                tracing::Span::current().record("block_height", block_height);

                debug!("validating block");

                // if vdf is invalid, notify the block tree immediately
                if let Err(_err) = self.clone().ensure_vdf_is_valid(&block).await {
                    // Notify the block tree service
                    if let Err(e) = self.service_senders.block_tree.send(
                        BlockTreeServiceMessage::BlockValidationFinished {
                            block_hash,
                            validation_result: ValidationResult::Invalid,
                        },
                    ) {
                        error!(?e, "Failed to send validation result to block tree service");
                    }
                    return None;
                }

                // schedule validation task
                let block_tree_guard = self.block_tree_guard.clone();
                let task =
                    BlockValidationTask::new(block, block_hash, self.clone(), block_tree_guard);
                Some(task)
            }
        }
    }

    /// Perform vdf fast forwarding and validation.
    /// If for some reason the vdf steps are invalid and / or don't match then the function will return an error
    #[tracing::instrument(err, skip_all, fields(block_hash = ?block.block_hash, block_height = ?block.height))]
    async fn ensure_vdf_is_valid(self: Arc<Self>, block: &IrysBlockHeader) -> eyre::Result<()> {
        let vdf_info = block.vdf_limiter_info.clone();

        // First, wait for the previous VDF step to be available
        let first_step_number = vdf_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);

        self.vdf_state.wait_for_step(prev_output_step_number).await;
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

        // Spawn VDF validation task
        let vdf_ff = self.service_senders.vdf_fast_forward.clone();
        let vdf_state = self.vdf_state.clone();
        {
            let vdf_info = vdf_info.clone();
            tokio::task::spawn_blocking(move || {
                vdf_steps_are_valid(
                    &self.pool,
                    &vdf_info,
                    &self.config.consensus.vdf,
                    &self.vdf_state,
                )
            })
            .await??;
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
