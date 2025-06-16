use crate::block_tree_service::{
    BlockState, BlockTreeReadGuard, BlockTreeServiceMessage, ChainState, ValidationResult,
};
use crate::block_validation::{
    poa_is_valid, recall_recall_range_is_valid, system_transactions_are_valid,
};
use crate::validation_service::ValidationServiceInner;
use irys_types::{BlockHash, IrysBlockHeader};
use std::sync::Arc;
use tracing::{debug, error, warn, Instrument};

/// Handles the execution of a single block validation task
pub(crate) struct BlockValidationTask {
    block: Arc<IrysBlockHeader>,
    block_hash: BlockHash,
    service_inner: Arc<ValidationServiceInner>,
    block_tree_guard: BlockTreeReadGuard,
}

impl BlockValidationTask {
    pub(crate) fn new(
        block: Arc<IrysBlockHeader>,
        block_hash: BlockHash,
        service_inner: Arc<ValidationServiceInner>,
        block_tree_guard: BlockTreeReadGuard,
    ) -> Self {
        Self {
            block,
            block_hash,
            service_inner,
            block_tree_guard,
        }
    }

    /// Execute the complete validation task
    pub(crate) async fn execute(self) {
        let validation_result = self
            .validate_block()
            .await
            .unwrap_or(ValidationResult::Invalid);

        // If validation is successful, wait for parent to be validated before reporting
        if matches!(validation_result, ValidationResult::Valid) {
            if !self.wait_for_parent_validation().await {
                // Task was cancelled due to height difference
                return;
            }
        }

        // Notify the block tree service
        self.send_validation_result(validation_result);
    }

    /// Wait for parent validation to complete
    /// Returns false if the task should be cancelled due to height difference
    async fn wait_for_parent_validation(&self) -> bool {
        let parent_hash = self.block.previous_block_hash;

        loop {
            // Check if block height is too far behind canonical tip
            if self.should_exit_due_to_height_diff() {
                debug!(
                    ?self.block_hash,
                    block_height = ?self.block.height,
                    "exiting validation task - block too far behind canonical tip"
                );
                return false;
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

        true
    }

    /// Check if the block should exit due to height difference from canonical tip
    fn should_exit_due_to_height_diff(&self) -> bool {
        let block_tree = self.block_tree_guard.read();
        let tip_hash = block_tree.tip;

        if let Some(tip_block) = block_tree.get_block(&tip_hash) {
            let height_diff = tip_block.height.saturating_sub(self.block.height);
            height_diff
                > self
                    .service_inner
                    .config
                    .consensus
                    .validation_height_diff_threshold
        } else {
            false
        }
    }

    /// Get the chain state of the parent block
    fn get_parent_chain_state(&self, parent_hash: &BlockHash) -> Option<ChainState> {
        let block_tree = self.block_tree_guard.read();
        block_tree
            .get_block_and_status(parent_hash)
            .map(|(_header, state)| state.clone())
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
    fn send_validation_result(&self, validation_result: ValidationResult) {
        if let Err(e) = self.service_inner.service_senders.block_tree.send(
            BlockTreeServiceMessage::BlockValidationFinished {
                block_hash: self.block_hash,
                validation_result,
            },
        ) {
            error!(?e, "Failed to send validation result to block tree service");
        }
    }

    /// Perform block validation
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
            .inspect_err(|err| tracing::error!(?err, "poa is invalid"))
            .map(|()| ValidationResult::Valid)
            .unwrap_or(ValidationResult::Invalid)
        }
        .instrument(tracing::info_span!("recall range validation"));

        // POA validation
        let poa_task = {
            let consensus_config = self.service_inner.config.consensus.clone();
            let block_index_guard = self.service_inner.block_index_guard.clone();
            let partitions_guard = self.service_inner.partition_assignments_guard.clone();
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
        let config = &self.service_inner.config;
        let service_senders = &self.service_inner.service_senders;
        let system_tx_task = async move {
            system_transactions_are_valid(
                config,
                service_senders,
                block,
                &self.service_inner.reth_node_adapter,
                &self.service_inner.db,
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
