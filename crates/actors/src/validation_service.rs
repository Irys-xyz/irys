//! Validation service module.
//!
//! The validation service is responsible for validating blocks by:
//! - Validating VDF (Verifiable Delay Function) steps
//! - Validating recall range
//! - Validating PoA (Proof of Access)
//! - Validating that the generated system txs in the reth block
//!   match the expected system txs from the irys block.
//!
//! The service supports concurrent validation tasks for improved performance.

use crate::block_tree_service::BlockTreeReadGuard;
use crate::block_validation::{recall_recall_range_is_valid, system_transactions_are_valid};
use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeServiceMessage, ValidationResult},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
    services::ServiceSenders,
};
use active_validations::ActiveValidations;
use block_validation_task::BlockValidationTask;
use eyre::ensure;
use futures::FutureExt;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{app_state::DatabaseProvider, BlockHash, Config, IrysBlockHeader};
use irys_vdf::rayon;
use irys_vdf::state::{vdf_steps_are_valid, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::future::Future;
use std::{pin::pin, sync::Arc};
use tokio::{
    sync::mpsc::UnboundedReceiver,
    task::JoinHandle,
    time::{interval, Duration},
};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

mod active_validations;
mod block_validation_task;

/// Messages that the validation service supports
#[derive(Debug)]
pub enum ValidationServiceMessage {
    /// Validate a block
    ValidateBlock { block: Arc<IrysBlockHeader> },
}

/// Main validation service structure
#[derive(Debug)]
pub struct ValidationService {
    /// Graceful shutdown handle
    shutdown: GracefulShutdown,
    /// Message receiver
    msg_rx: UnboundedReceiver<ValidationServiceMessage>,
    /// Inner service logic
    inner: Arc<ValidationServiceInner>,
}

/// Inner service structure containing business logic
#[derive(Debug)]
pub(crate) struct ValidationServiceInner {
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
    pub(crate) block_tree_guard: BlockTreeReadGuard,
    /// Rayon thread pool that executes vdf steps   
    pub(crate) pool: rayon::ThreadPool,
}

impl ValidationService {
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
        rx: UnboundedReceiver<ValidationServiceMessage>,
    ) -> JoinHandle<()> {
        let config = config.clone();
        let service_senders = service_senders.clone();

        exec.spawn_critical_with_graceful_shutdown_signal(
            "Validation Service",
            |shutdown| async move {
                let validation_service = Self {
                    shutdown,
                    msg_rx: rx,
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
                    }),
                };

                validation_service
                    .start()
                    .await
                    .expect("validation service encountered an irrecoverable error")
            },
        )
    }

    /// Main service loop
    async fn start(mut self) -> eyre::Result<()> {
        info!("starting validation service");

        let mut active_validations =
            pin!(ActiveValidations::new(self.inner.block_tree_guard.clone()));

        // todo: add a notification system to the block tree service that'd
        // allow us to subscribe to each block status being updated. That could
        // act as a trigger point for re-evaluation. Rather than relying on a timer.
        let mut validation_timer = interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    break;
                }

                // Receive new validation messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            // Transform message to validation future
                            let Some((block_hash, fut)) =
                                self.inner.clone().create_validation_future(msg).await
                            else {
                                // validation future was not created. The task failed during vdf validation
                                continue;
                            };
                            active_validations.push(block_hash, fut.boxed());
                        }
                        None => {
                            // Channel closed
                            warn!("receiver channel closed");
                            break;
                        }
                    }
                }

                // Process active validations every 100ms (only if not empty)
                _ = validation_timer.tick(), if !active_validations.is_empty() => {
                    // Process any completed validations (non-blocking)
                    active_validations.process_completed().await;

                    // If no active validations and channel closed, exit
                    if active_validations.is_empty() && self.msg_rx.is_closed() {
                        break;
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

impl ValidationServiceInner {
    /// Handle incoming messages
    #[instrument(skip_all)]
    async fn create_validation_future(
        self: Arc<Self>,
        msg: ValidationServiceMessage,
    ) -> Option<(BlockHash, impl Future<Output = ()>)> {
        match msg {
            ValidationServiceMessage::ValidateBlock { block } => {
                let block_hash = block.block_hash;
                let block_height = block.height;

                debug!(?block_hash, ?block_height, "validating block");

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

                let block_tree_guard = self.block_tree_guard.clone();
                let task =
                    BlockValidationTask::new(block, block_hash, self.clone(), block_tree_guard);
                Some((block_hash, task.execute()))
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
}
