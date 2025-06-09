//! Validation service module.
//!
//! The validation service is responsible for validating blocks by:
//! - Validating VDF (Verifiable Delay Function) steps
//! - Validating recall range
//! - Validating PoA (Proof of Access)
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
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{app_state::DatabaseProvider, Config, IrysBlockHeader};
use irys_vdf::state::{vdf_steps_are_valid, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::{pin::pin, sync::Arc};
use tokio::{
    sync::mpsc::UnboundedReceiver,
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, error, info, instrument, trace, warn, Instrument};

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
    vdf_state_readonly: VdfStateReadonly,
    /// Reference to global config for node
    config: Config,
    /// Service channels
    service_senders: ServiceSenders,
    /// Reth node adapter for RPC calls
    reth_node_adapter: IrysRethNodeAdapter,
    /// Database provider for transaction lookups
    db: DatabaseProvider,
    /// Active validation tasks
    validation_tasks: JoinSet<()>,
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
                        block_index_guard,
                        partition_assignments_guard,
                        vdf_state_readonly,
                        config,
                        service_senders,
                        reth_node_adapter,
                        db,
                        validation_tasks: JoinSet::new(),
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
            tokio::select! {
                // Handle incoming messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => self.inner.handle_message(msg),
                        None => {
                            warn!("receiver channel closed");
                            break None;
                        }
                    }
                }
                // Clean up completed tasks
                Some(result) = self.inner.validation_tasks.join_next() => {
                    match result {
                        Ok(()) => {
                            debug!("validation task completed successfully");
                        }
                        Err(e) if e.is_cancelled() => {
                            debug!("validation task was cancelled");
                        }
                        Err(e) => {
                            error!(?e, "validation task panicked");
                        }
                    }
                }
                // Handle shutdown signal
                shutdown = &mut shutdown_future => {
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
            self.inner.handle_message(msg);
        }

        // Abort all active validation tasks and wait for them to finish
        info!(
            "aborting {} active validation tasks",
            self.inner.validation_tasks.len()
        );
        self.inner.validation_tasks.shutdown().await;

        // Explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        info!("shutting down validation service");
        Ok(())
    }
}

impl ValidationServiceInner {
    /// Handle incoming messages
    #[instrument(skip_all)]
    fn handle_message(&mut self, msg: ValidationServiceMessage) {
        match msg {
            ValidationServiceMessage::ValidateBlock { block } => {
                self.spawn_validation_task(block);
            }
        }
    }

    /// Spawn a validation task for a block
    fn spawn_validation_task(&mut self, block: Arc<IrysBlockHeader>) {
        let block_hash = block.block_hash;
        let block_height = block.height;

        debug!(?block_hash, ?block_height, "spawning validation task");

        // Clone necessary data for the spawned task
        let block_index_guard = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let vdf_state = self.vdf_state_readonly.clone();
        let config = self.config.clone();
        let service_senders = self.service_senders.clone();
        let reth_adapter = self.reth_node_adapter.clone();
        let db = self.db.clone();

        // Spawn the validation task
        self.validation_tasks.spawn(async move {
            let validation_result = validate_block_concurrent(
                block,
                block_index_guard,
                partitions_guard,
                vdf_state,
                config,
                service_senders.clone(),
                reth_adapter,
                db,
            )
            .await
            .unwrap_or(ValidationResult::Invalid);

            // Also notify the block tree service
            if let Err(e) =
                service_senders
                    .block_tree
                    .send(BlockTreeServiceMessage::BlockValidationFinished {
                        block_hash,
                        validation_result,
                    })
            {
                error!(?e, "Failed to send validation result to block tree service");
            };
        });
    }
}

/// Perform concurrent block validation
#[tracing::instrument(err, skip_all, fields(block_hash = ?block.block_hash, block_height = ?block.height))]
async fn validate_block_concurrent(
    block: Arc<IrysBlockHeader>,
    block_index_guard: BlockIndexReadGuard,
    partitions_guard: PartitionAssignmentsReadGuard,
    vdf_state: VdfStateReadonly,
    config: Config,
    service_senders: ServiceSenders,
    reth_adapter: IrysRethNodeAdapter,
    db: DatabaseProvider,
) -> eyre::Result<ValidationResult> {
    let vdf_info = block.vdf_limiter_info.clone();

    // First, wait for the previous VDF step to be available
    let first_step_number = vdf_info.first_step_number();
    let prev_output_step_number = first_step_number.saturating_sub(1);

    vdf_state.wait_for_step(prev_output_step_number).await;
    let stored_previous_step = vdf_state
        .get_step(prev_output_step_number)
        .expect("to get the step, since we've just waited for it");

    trace!(
        expected = ?stored_previous_step,
        got = ?vdf_info.prev_output,
        "Previous output from the block is not equal to the saved step with the same index"
    );
    ensure!(
        stored_previous_step == vdf_info.prev_output,
        "Previous output from the block is not equal to the saved step with the same index"
    );

    // Spawn VDF validation task
    let vdf_config = config.consensus.vdf.clone();
    let vdf_state_for_validation = vdf_state.clone();
    let vdf_info_clone = vdf_info.clone();
    tokio::task::spawn_blocking(move || {
        vdf_steps_are_valid(&vdf_info_clone, &vdf_config, vdf_state_for_validation)
    })
    .await??;

    // Fast forward VDF steps
    fast_forward_vdf_steps_from_block(&vdf_info, service_senders.vdf_fast_forward.clone()).await;
    vdf_state.wait_for_step(vdf_info.global_step_number).await;

    // Recall range validation
    let recall_task = {
        let block = block.clone();
        let consensus_config = config.consensus.clone();
        let vdf_state = vdf_state.clone();
        tokio::spawn(async move {
            recall_recall_range_is_valid(&block, &consensus_config, &vdf_state)
                .await
                .inspect_err(|err| tracing::error!(?err, "poa is invalid"))
                .map(|_| ValidationResult::Valid)
                .unwrap_or(ValidationResult::Invalid)
        })
        .instrument(tracing::info_span!("recall range validation"))
    };

    // POA validation
    let poa_task = {
        let poa = block.poa.clone();
        let miner_address = block.miner_address;
        let consensus_config = config.consensus.clone();
        tokio::task::spawn_blocking(move || {
            poa_is_valid(
                &poa,
                &block_index_guard,
                &partitions_guard,
                &consensus_config,
                &miner_address,
            )
            .inspect_err(|err| tracing::error!(?err, "poa is invalid"))
            .map(|_| ValidationResult::Valid)
            .unwrap_or(ValidationResult::Invalid)
        })
        .instrument(tracing::info_span!("poa task validation"))
    };

    // System transaction validation
    let system_tx_task = {
        let block = block.clone();
        let reth_adapter = reth_adapter.clone();
        let db = db.clone();
        tokio::spawn(async move {
            system_transactions_are_valid(&block, &reth_adapter, &db)
                .await
                .inspect_err(|err| tracing::error!(?err, "system transactions are invalid"))
                .map(|_| ValidationResult::Valid)
                // TODO: we just assume all system txs are valid until we fix the validator
                .unwrap_or(ValidationResult::Valid)
        })
        .instrument(tracing::info_span!("system transaction validation"))
    };

    // Wait for all three tasks to complete
    let (recall_result, poa_result, system_tx_result) =
        tokio::try_join!(recall_task, poa_task, system_tx_task)?;

    match (recall_result, poa_result, system_tx_result) {
        (ValidationResult::Valid, ValidationResult::Valid, ValidationResult::Valid) => {
            Ok(ValidationResult::Valid)
        }
        _ => Ok(ValidationResult::Invalid),
    }
}
