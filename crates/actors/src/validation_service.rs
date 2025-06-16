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

use crate::block_tree_service::{BlockState, BlockTreeReadGuard, ChainState};
use crate::block_validation::{recall_recall_range_is_valid, system_transactions_are_valid};
use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeServiceMessage, ValidationResult},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
    services::ServiceSenders,
};
use eyre::ensure;
use futures::future::poll_immediate;
use futures::{FutureExt, Stream, StreamExt};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{app_state::DatabaseProvider, BlockHash, Config, IrysBlockHeader};
use irys_vdf::rayon;
use irys_vdf::state::{vdf_steps_are_valid, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use priority_queue::PriorityQueue;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::future::Future;
use std::{
    cmp::Reverse,
    pin::{pin, Pin},
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

/// Wrapper around active validations with capacity management and priority ordering
struct ActiveValidations {
    /// Priority queue of (block_hash, future) with priority based on canonical chain distance
    validations: PriorityQueue<BlockHash, Reverse<u64>>,
    /// Map from block hash to the actual future
    futures: std::collections::HashMap<BlockHash, Pin<Box<dyn Future<Output = ()> + Send>>>,
    max_capacity: usize,
    block_tree_guard: BlockTreeReadGuard,
}

impl ActiveValidations {
    fn new(max_capacity: usize, block_tree_guard: BlockTreeReadGuard) -> Self {
        Self {
            validations: PriorityQueue::new(),
            futures: std::collections::HashMap::new(),
            max_capacity,
            block_tree_guard,
        }
    }

    fn can_add_more(&self) -> bool {
        self.validations.len() < self.max_capacity
    }

    /// Calculate the priority for a block based on its nearest canonical chain ancestor
    fn calculate_priority(&self, block_hash: &BlockHash) -> Reverse<u64> {
        let block_tree = self.block_tree_guard.read();

        let mut current_hash = *block_hash;
        let mut current_height = 0u64;

        // Walk up the chain to find the nearest canonical (Onchain) ancestor
        while let Some((block, chain_state)) = block_tree.get_block_and_status(&current_hash) {
            current_height = block.height;

            // Check if this block is on the canonical chain
            if matches!(chain_state, ChainState::Onchain) {
                break;
            }

            // Move to parent block
            current_hash = block.previous_block_hash;

            // Safety check to prevent infinite loops
            if block.height == 0 {
                break;
            }
        }

        // Lower heights get higher priority (processed first)
        Reverse(current_height)
    }

    fn push(&mut self, block_hash: BlockHash, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        let priority = self.calculate_priority(&block_hash);
        self.futures.insert(block_hash, future);
        self.validations.push(block_hash, priority);
    }

    fn len(&self) -> usize {
        self.validations.len()
    }

    fn is_empty(&self) -> bool {
        self.validations.is_empty()
    }
}

impl Stream for ActiveValidations {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut completed_blocks = Vec::new();

        assert_eq!(
            self.validations.len(),
            self.futures.len(),
            "validations and futures ouf of sync"
        );
        if self.validations.is_empty() {
            return Poll::Pending;
        }

        // Poll futures in priority order
        for (block_hash, _priority) in self.validations.clone().iter() {
            if let Some(future) = self.futures.get_mut(block_hash) {
                if let Poll::Ready(()) = future.poll_unpin(cx) {
                    completed_blocks.push(*block_hash);
                }
            }
        }

        // Remove completed validations
        for block_hash in &completed_blocks {
            self.validations.remove(block_hash);
            self.futures.remove(block_hash);
        }

        if !completed_blocks.is_empty() {
            // Signal that validations have completed
            Poll::Ready(Some(()))
        } else {
            // No completions, wait for more work
            Poll::Pending
        }
    }
}

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
    block_tree_guard: BlockTreeReadGuard,
    /// Rayon thread pool that executes vdf steps   
    pool: rayon::ThreadPool,
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

        let mut shutdown_future = pin!(&mut self.shutdown);
        let mut active_validations = pin!(ActiveValidations::new(
            10,
            self.inner.block_tree_guard.clone()
        ));

        'shutdown: loop {
            'outer: {
                // Check if we should try to receive new messages
                if active_validations.can_add_more() {
                    // check if we received a shutdown signal
                    if let Some(shutdwon) = poll_immediate(&mut shutdown_future).await {
                        drop(shutdwon);
                        break 'shutdown;
                    }

                    // Try to receive a new message
                    let msg = match self.msg_rx.try_recv() {
                        Ok(msg) => msg,
                        Err(TryRecvError::Empty) => {
                            // no messages in the queue
                            break 'outer;
                        }
                        Err(TryRecvError::Disconnected) => {
                            warn!("receiver channel closed");
                            // receiver channel closed
                            break 'shutdown;
                        }
                    };

                    // Transform message to validation future
                    let Some((block_hash, fut)) =
                        self.inner.clone().create_validation_future(msg).await
                    else {
                        // validation future was not created. The task failed during vdf validation
                        break 'outer;
                    };
                    active_validations.push(block_hash, fut.boxed());

                    // we keep reading new block entries until there are no entries or we cannot add any more
                    continue 'shutdown;
                }
            }

            // Process any completed validations (non-blocking)
            while let Some(_) = poll_immediate(active_validations.next()).await {
                // One or more validations completed - continue processing
            }

            // If no active validations and channel closed, exit
            if active_validations.is_empty() && self.msg_rx.is_closed() {
                break 'shutdown;
            }

            // Yield to prevent busy loop
            tokio::task::yield_now().await;
        }

        // Drain remaining validations
        info!(
            "draining {} active validations before shutdown",
            active_validations.len()
        );
        while !active_validations.is_empty() {
            // Drain remaining validations
            if active_validations.next().await.is_some() {
                // Validation completed during shutdown
            }
        }

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
                let concurrent_task = async move {
                    let validation_result = self
                        .validate_block(block.clone())
                        .await
                        .unwrap_or(ValidationResult::Invalid);

                    // If validation is successful, wait for parent to be validated before reporting
                    if matches!(validation_result, ValidationResult::Valid) {
                        let parent_hash = block.previous_block_hash;
                        loop {
                            let parent_chain_state = {
                                let block_tree = block_tree_guard.read();
                                block_tree
                                    .get_block_and_status(&parent_hash)
                                    .map(|(_header, state)| state.clone())
                                    .clone()
                            };
                            let Some(parent_chain_state) = parent_chain_state else {
                                tracing::warn!(
                                    "validated a valid block that is not inside the block tree"
                                );
                                break;
                            };
                            match parent_chain_state {
                                ChainState::Onchain
                                | ChainState::Validated(_)
                                | ChainState::NotOnchain(BlockState::ValidBlock) => {
                                    // Parent is ready, we can proceed
                                    break;
                                }
                                ChainState::NotOnchain(BlockState::Unknown)
                                | ChainState::NotOnchain(BlockState::ValidationScheduled) => {
                                    // Parent not ready, yield and try again when polled later
                                    tokio::task::yield_now().await;
                                    continue;
                                }
                            }
                        }
                    }

                    // Notify the block tree service
                    if let Err(e) = self.service_senders.block_tree.send(
                        BlockTreeServiceMessage::BlockValidationFinished {
                            block_hash,
                            validation_result,
                        },
                    ) {
                        error!(?e, "Failed to send validation result to block tree service");
                    }
                };
                Some((block_hash, concurrent_task))
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

    /// Perform block validation
    #[tracing::instrument(err, skip_all, fields(block_hash = ?block.block_hash, block_height = ?block.height))]
    async fn validate_block(&self, block: Arc<IrysBlockHeader>) -> eyre::Result<ValidationResult> {
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
