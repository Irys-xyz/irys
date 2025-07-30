//! Block validation task execution module.
//!
//! Handles individual block validation through a two-stage pipeline:
//!
//! ## Stage 1: VDF validation (execute_vdf)
//! - **VDF**: Verifies VDF steps are valid & fast-forwards the node if they are
//! Uses a single preemptible task slot to prevent thread overutilisation
//!
//! ## Stage 2: Concurrent Validation (execute_concurrent)
//! Three concurrent validation stages:
//! - **Recall Range**: Async data recall and storage proof verification
//! - **POA**: Blocking cryptographic proof-of-access validation
//! - **Shadow Transactions**: Async Reth integration validation
//!
//! ## Stage 3: Parent Dependency Resolution  
//! After successful validation, tasks wait for parent block validation using
//! cooperative yielding. Tasks are cancelled if too far behind canonical tip.

use crate::block_tree_service::{BlockTreeServiceMessage, ValidationResult};
use crate::block_validation::{
    commitment_txs_are_valid, is_seed_data_valid, poa_is_valid, recall_recall_range_is_valid,
    shadow_transactions_are_valid,
};
use crate::validation_service::active_validations::BlockPriorityMeta;
use crate::validation_service::{ValidationServiceInner, VdfValidationResult};
use irys_domain::{BlockState, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, IrysBlockHeader};
use irys_vdf::state::CancelEnum;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tracing::{debug, error, warn, Instrument as _};

/// Result of waiting for parent validation to complete
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentValidationResult {
    /// Parent validation is complete, task can proceed
    Ready,
    /// Task should be cancelled due to height difference from canonical tip
    Cancelled,
}

/// Handles the execution of a single block validation task
#[derive(Clone)]
pub(crate) struct BlockValidationTask {
    pub block: Arc<IrysBlockHeader>,
    pub service_inner: Arc<ValidationServiceInner>,
    pub block_tree_guard: BlockTreeReadGuard,
    pub priority: BlockPriorityMeta,
}

impl Ord for BlockValidationTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}
impl PartialOrd for BlockValidationTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for BlockValidationTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority // captures the block, so this should be good enough
    }
}

impl Eq for BlockValidationTask {}

impl BlockValidationTask {
    pub(crate) fn new(
        block: Arc<IrysBlockHeader>,
        service_inner: Arc<ValidationServiceInner>,
        block_tree_guard: BlockTreeReadGuard,
        meta: BlockPriorityMeta,
    ) -> Self {
        Self {
            block,
            service_inner,
            block_tree_guard,
            priority: meta,
        }
    }

    /// Execute the concurrent validation task
    #[tracing::instrument(skip_all, fields(block_hash = %self.block.block_hash, block_height = %self.block.height))]
    pub(crate) async fn execute_concurrent(self) {
        let validation_result = self
            .validate_block()
            .await
            .unwrap_or(ValidationResult::Invalid);

        // If validation is successful, wait for parent to be validated before reporting
        if matches!(validation_result, ValidationResult::Valid) {
            match self.wait_for_parent_validation().await {
                ParentValidationResult::Cancelled => {
                    // Task was cancelled due to height difference
                    return;
                }
                ParentValidationResult::Ready => {
                    // Parent is ready, continue to report validation result
                }
            }
        }

        // Notify the block tree service
        self.send_validation_result(validation_result);
    }

    #[tracing::instrument(skip_all, fields(block_hash = %self.block.block_hash, block_height = %self.block.height))]
    pub(crate) async fn execute_vdf(self, cancel: Arc<AtomicU8>) -> VdfValidationResult {
        let inner = Arc::clone(&self.service_inner);
        let block = Arc::clone(&self.block);

        // run the VDF validation
        // we use a task here as it'll drive the future more consistently than `poll_immediate`
        let cancel2 = Arc::clone(&cancel);
        let res = tokio::spawn(
            async move { inner.ensure_vdf_is_valid(&block, cancel2).await }.in_current_span(),
        )
        .await
        .expect("Failed to join ensure_vdf_is_valid task");

        let mapped_res = res
            .map(|()| VdfValidationResult::Valid)
            .unwrap_or_else(|e| {
                // use the value of `cancel` to figure out if we errored because we were cancelled
                // TODO: switch this out for a definite Result type
                let cancel_state = cancel.load(Ordering::Relaxed);
                if cancel_state == CancelEnum::Cancelled as u8 {
                    VdfValidationResult::Cancelled
                } else {
                    VdfValidationResult::Invalid(e)
                }
            });
        debug!("Finished validating with result {:?}", &mapped_res);
        mapped_res
    }

    /// Wait for parent validation to complete
    /// We do this because just because a block is valid internally, if it's not connected to a valid chain it's still not valid
    #[tracing::instrument(skip_all, fields(block_hash = %self.block.block_hash, block_height = %self.block.height))]
    async fn wait_for_parent_validation(&self) -> ParentValidationResult {
        let parent_hash = self.block.previous_block_hash;

        loop {
            // Check if block height is too far behind canonical tip
            if self.should_exit_due_to_height_diff() {
                let _span = tracing::debug_span!("height_diff_exit", block_hash = %self.block.block_hash, block_height = %self.block.height).entered();
                debug!("exiting validation task - block too far behind canonical tip");
                return ParentValidationResult::Cancelled;
            }

            let Some(parent_chain_state) = self.get_parent_chain_state(&parent_hash) else {
                warn!("validated a valid block that is not inside the block tree");
                break;
            };

            if self.is_parent_ready(&parent_chain_state) {
                // Parent is ready, we can proceed
                break;
            } else {
                // Parent not ready, yield and try again when polled later
                tokio::task::yield_now().await;
                continue;
            }
        }

        ParentValidationResult::Ready
    }

    /// Check if the block should exit due to height difference from canonical tip
    fn should_exit_due_to_height_diff(&self) -> bool {
        let block_tree = self.block_tree_guard.read();
        let tip_hash = block_tree.tip;

        if let Some(tip_block) = block_tree.get_block(&tip_hash) {
            let height_diff = tip_block.height.saturating_sub(self.block.height);
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

    /// Check if the parent is ready for this block to be reported
    fn is_parent_ready(&self, parent_state: &ChainState) -> bool {
        matches!(
            parent_state,
            ChainState::Onchain
                | ChainState::Validated(_)
                | ChainState::NotOnchain(BlockState::ValidBlock)
        )
    }

    /// Send the validation result to the block tree service
    pub(crate) fn send_validation_result(&self, validation_result: ValidationResult) {
        if let Err(e) = self.service_inner.service_senders.block_tree.send(
            BlockTreeServiceMessage::BlockValidationFinished {
                block_hash: self.block.block_hash,
                validation_result,
            },
        ) {
            error!(?e, "Failed to send validation result to block tree service");
        }
    }

    /// Perform block validation
    #[tracing::instrument(skip_all, err, fields(block_hash = %self.block.block_hash, block_height = %self.block.height))]
    async fn validate_block(&self) -> eyre::Result<ValidationResult> {
        let poa = self.block.poa.clone();
        let miner_address = self.block.miner_address;
        let block = &self.block;
        // Recall range validation
        let recall_task = async move {
            recall_recall_range_is_valid(
                block,
                &self.service_inner.config.consensus,
                &self.service_inner.vdf_state,
            )
            .await
            .inspect_err(|err| tracing::error!(?err, "recall range validation failed"))
            .map(|()| ValidationResult::Valid)
            .unwrap_or(ValidationResult::Invalid)
        }
        .instrument(tracing::info_span!("recall_range_validation", block_hash = %self.block.block_hash, block_height = %self.block.height));

        let epoch_snapshot = self
            .block_tree_guard
            .read()
            .get_epoch_snapshot(&block.block_hash)
            .expect("block should have an epoch snapshot in the block_tree");

        // POA validation
        let poa_task = {
            let consensus_config = self.service_inner.config.consensus.clone();
            let block_index_guard = self.service_inner.block_index_guard.clone();
            let block_hash = self.block.block_hash;
            let block_height = self.block.height;
            tokio::task::spawn_blocking(move || {
                poa_is_valid(
                    &poa,
                    &block_index_guard,
                    &epoch_snapshot,
                    &consensus_config,
                    &miner_address,
                )
                .inspect_err(|err| tracing::error!(?err, "poa validation failed"))
                .map(|()| ValidationResult::Valid)
            })
            .instrument(tracing::info_span!("poa_validation", block_hash = %block_hash, block_height = %block_height))
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

        // Shadow transaction validation
        let config = &self.service_inner.config;
        let service_senders = &self.service_inner.service_senders;
        let shadow_tx_task = async move {
            shadow_transactions_are_valid(
                config,
                service_senders,
                block,
                &self.service_inner.reth_node_adapter,
                &self.service_inner.db,
                self.service_inner.execution_payload_provider.clone(),
            )
            .instrument(tracing::info_span!("shadow_tx_validation", block_hash = %self.block.block_hash, block_height = %self.block.height))
            .await
            .inspect_err(|err| tracing::error!(?err, "shadow transaction validation failed"))
            .map(|()| ValidationResult::Valid)
            .unwrap_or(ValidationResult::Invalid)
        };

        let vdf_reset_frequency = self.service_inner.config.consensus.vdf.reset_frequency as u64;
        let seeds_validation_task = async move {
            let binding = self.block_tree_guard.read();
            let previous_block = binding
                .get_block(&self.block.previous_block_hash)
                .expect("previous block should exist");
            is_seed_data_valid(&self.block, previous_block, vdf_reset_frequency)
        };

        // Commitment transaction ordering validation
        let commitment_ordering_task = async move {
            commitment_txs_are_valid(
                config,
                service_senders,
                block,
                &self.service_inner.db,
                &self.block_tree_guard,
            )
            .instrument(tracing::info_span!("commitment_ordering_validation", block_hash = %self.priority.block.block_hash, block_height = %self.priority.block.height))
            .await
            .inspect_err(|err| tracing::error!(?err, "commitment ordering validation failed"))
            .map(|()| ValidationResult::Valid)
            .unwrap_or(ValidationResult::Invalid)
        };

        // Wait for all validation tasks to complete
        let (
            recall_result,
            poa_result,
            shadow_tx_result,
            seeds_validation_result,
            commitment_ordering_result,
        ) = tokio::join!(
            recall_task,
            poa_task,
            shadow_tx_task,
            seeds_validation_task,
            commitment_ordering_task
        );

        match (
            recall_result,
            poa_result,
            shadow_tx_result,
            seeds_validation_result,
            commitment_ordering_result,
        ) {
            (
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
            ) => {
                tracing::debug!("block validation successful");
                Ok(ValidationResult::Valid)
            }
            _ => {
                tracing::debug!("block validation failed");
                Ok(ValidationResult::Invalid)
            }
        }
    }
}
