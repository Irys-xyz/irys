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

use crate::block_validation::{recall_recall_range_is_valid, system_transactions_are_valid};
use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeServiceMessage, ValidationResult},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
    services::ServiceSenders,
};
use eyre::ensure;
use futures::future::Either;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{app_state::DatabaseProvider, Config, IrysBlockHeader};
use irys_vdf::rayon;
use irys_vdf::state::{vdf_steps_are_valid, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::{pin::pin, sync::Arc};
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

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
    inner: ValidationServiceInner,
}

/// Inner service structure containing business logic
#[derive(Debug)]
struct ValidationServiceInner {
    /// Read only view of the block index
    block_index_guard: BlockIndexReadGuard,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    partition_assignments_guard: PartitionAssignmentsReadGuard,
    /// VDF steps read guard
    vdf_state: VdfStateReadonly,
    /// Reference to global config for node
    config: Config,
    /// Service channels
    service_senders: ServiceSenders,
    /// Reth node adapter for RPC calls
    reth_node_adapter: IrysRethNodeAdapter,
    /// Database provider for transaction lookups
    db: DatabaseProvider,
    /// Rayon thread pool that executes vdf steps
    pool: rayon::ThreadPool,
}

impl ValidationService {
    /// Spawn a new validation service
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_index_guard: BlockIndexReadGuard,
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
                    inner: ValidationServiceInner {
                        pool: rayon::ThreadPoolBuilder::new()
                            .num_threads(config.consensus.vdf.parallel_verification_thread_limit)
                            .build()
                            .expect("to be able to build vdf validation pool"),
                        block_index_guard,
                        partition_assignments_guard,
                        vdf_state: vdf_state_readonly,
                        config,
                        service_senders,
                        reth_node_adapter,
                        db,
                    },
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

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            // Handle shutdown or message reception
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg).await;
                }
                Either::Left((None, _)) => {
                    warn!("receiver channel closed");
                    break None;
                }
                Either::Right((shutdown, _)) => {
                    warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        // Process remaining messages before shutdown
        debug!(
            amount_of_messages = ?self.msg_rx.len(),
            "processing last in-bound messages before shutdown"
        );
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await;
        }

        // Explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        info!("shutting down validation service");
        Ok(())
    }
}

impl ValidationServiceInner {
    /// Handle incoming messages
    #[instrument(skip_all)]
    async fn handle_message(&mut self, msg: ValidationServiceMessage) {
        match msg {
            ValidationServiceMessage::ValidateBlock { block } => {
                let block_hash = block.block_hash;
                let block_height = block.height;

                debug!(?block_hash, ?block_height, "validating block");

                let validation_result = self
                    .validate_block(block)
                    .await
                    .unwrap_or(ValidationResult::Invalid);

                // Notify the block tree service
                if let Err(e) = self.service_senders.block_tree.send(
                    BlockTreeServiceMessage::BlockValidationFinished {
                        block_hash,
                        validation_result,
                    },
                ) {
                    error!(?e, "Failed to send validation result to block tree service");
                }
            }
        }
    }

    /// Perform block validation
    #[tracing::instrument(err, skip_all, fields(block_hash = ?block.block_hash, block_height = ?block.height))]
    async fn validate_block(&self, block: Arc<IrysBlockHeader>) -> eyre::Result<ValidationResult> {
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
        vdf_steps_are_valid(
            &self.pool,
            &vdf_info,
            &self.config.consensus.vdf,
            &self.vdf_state,
        )?;

        // Fast forward VDF steps
        fast_forward_vdf_steps_from_block(&vdf_info, &self.service_senders.vdf_fast_forward);
        self.vdf_state
            .wait_for_step(vdf_info.global_step_number)
            .await;

        let poa = block.poa.clone();
        let miner_address = block.miner_address;
        let block = &block;
        // Recall range validation
        let recall_task = async move {
            recall_recall_range_is_valid(block, &self.config.consensus, &self.vdf_state)
                .await
                .inspect_err(|err| tracing::error!(?err, "poa is invalid"))
                .map(|()| ValidationResult::Valid)
                .unwrap_or(ValidationResult::Invalid)
        }
        .instrument(tracing::info_span!("recall range validation"));

        // POA validation
        let poa_task = {
            let consensus_config = self.config.consensus.clone();
            let block_index_guard = self.block_index_guard.clone();
            let partitions_guard = self.partition_assignments_guard.clone();
            tokio::task::spawn_blocking(move || {
                poa_is_valid(
                    &poa,
                    &block_index_guard,
                    &partitions_guard,
                    &consensus_config,
                    &miner_address,
                )
                .inspect_err(|err| tracing::error!(?err, "poa is invalid"))
                .map(|()| ValidationResult::Valid)
            })
            .instrument(tracing::info_span!("poa task validation"))
        };
        let poa_task = async move {
            let res = poa_task.await;

            match res {
                Ok(res) => res.unwrap_or(ValidationResult::Invalid),
                Err(err) => {
                    tracing::error!(?err, "poa task panicked");
                    ValidationResult::Invalid
                }
            }
        };

        // System transaction validation
        let config = &self.config;
        let service_senders = &self.service_senders;
        let system_tx_task = async move {
            system_transactions_are_valid(
                config,
                service_senders,
                block,
                &self.reth_node_adapter,
                &self.db,
            )
            .instrument(tracing::info_span!("system transaction validation"))
            .await
            .inspect_err(|err| tracing::error!(?err, "system transactions are invalid"))
            .map(|()| ValidationResult::Valid)
            .unwrap_or(ValidationResult::Valid)
        };

        // Wait for all three tasks to complete
        let (recall_result, poa_result, system_tx_result) =
            tokio::join!(recall_task, poa_task, system_tx_task);

        match (recall_result, poa_result, system_tx_result) {
            (ValidationResult::Valid, ValidationResult::Valid, ValidationResult::Valid) => {
                Ok(ValidationResult::Valid)
            }
            _ => Ok(ValidationResult::Invalid),
        }
    }
}
