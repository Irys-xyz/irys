//! Block validation task execution module.
//!
//! Handles individual block validation through a two-stage pipeline:
//!
//! ## Stage 1: VDF validation (execute_vdf)
//! - **VDF**: Verifies VDF steps are valid & fast-forwards the node if they are
//! Uses a single preemptible task slot to prevent thread overutilisation
//!
//! ## Stage 2: Concurrent Validation (execute_concurrent)
//! Six concurrent validation stages:
//! - **Recall Range**: Async data recall and storage proof verification
//! - **POA**: Blocking cryptographic proof-of-access validation
//! - **Shadow Transactions**: Async Reth integration validation
//! - **Seeds**: Validates VDF seed data
//! - **Commitment Ordering**: Validates commitment transaction ordering
//! - **Data Transaction Fees**: Validates data transaction fees using block's EMA
//!
//! ## Stage 3: Parent Dependency Resolution
//! After successful validation, tasks wait for parent block validation using
//! cooperative yielding. Tasks are cancelled if too far behind canonical tip.

use crate::block_tree_service::ValidationResult;
use crate::block_validation::{
    ValidationError, commitment_txs_are_valid, data_txs_are_valid, is_seed_data_valid,
    poa_is_valid, recall_recall_range_is_valid, shadow_transactions_are_valid,
    submit_payload_to_reth,
};
use crate::validation_service::ValidationServiceInner;
use eyre::Context as _;
use futures::FutureExt as _;
use irys_domain::{BlockState, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, SealedBlock, SystemLedger};
use std::ops::ControlFlow;
use std::sync::Arc;
use tracing::{Instrument as _, debug, error, warn};

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
pub(super) struct BlockValidationTask {
    pub sealed_block: Arc<SealedBlock>,
    pub service_inner: Arc<ValidationServiceInner>,
    pub block_tree_guard: BlockTreeReadGuard,
    pub skip_vdf_validation: bool,
    pub parent_span: tracing::Span,
}

impl PartialEq for BlockValidationTask {
    fn eq(&self, other: &Self) -> bool {
        self.sealed_block.header().block_hash == other.sealed_block.header().block_hash
    }
}

impl Eq for BlockValidationTask {}

impl std::hash::Hash for BlockValidationTask {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(self.sealed_block.header().block_hash.as_bytes());
    }
}

impl PartialOrd for BlockValidationTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BlockValidationTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Ordering is handled by ValidationPriority, this is
        // just to satisfy PriorityQueue requirement for the value to be Ord
        self.sealed_block
            .header()
            .block_hash
            .cmp(&other.sealed_block.header().block_hash)
    }
}

impl BlockValidationTask {
    pub(super) fn new(
        sealed_block: Arc<SealedBlock>,
        service_inner: Arc<ValidationServiceInner>,
        block_tree_guard: BlockTreeReadGuard,
        skip_vdf_validation: bool,
        parent_span: tracing::Span,
    ) -> Self {
        Self {
            sealed_block,
            service_inner,
            block_tree_guard,
            skip_vdf_validation,
            parent_span,
        }
    }

    /// Execute the concurrent validation task
    #[tracing::instrument(parent = &self.parent_span, skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    pub(super) async fn execute_concurrent(self) -> ValidationResult {
        let parent_got_cancelled = || {
            // Task was cancelled due to height difference
            // Return invalid to prevent this block from being accepted
            tracing::warn!(
                block.hash = %self.sealed_block.header().block_hash,
                "Validation cancelled due to height difference"
            );
            ValidationResult::Invalid(ValidationError::ValidationCancelled {
                reason: "height difference".to_string(),
            })
        };

        let wait_for_parent_validation = self
            .exit_if_block_is_too_old(|_| ControlFlow::Continue(()))
            .boxed();
        let validate_block = self.validate_block().boxed();
        match futures::future::select(validate_block, wait_for_parent_validation).await {
            futures::future::Either::Left((validation_result, _block_too_old_future)) => {
                // If validation is successful, wait for parent to be validated before reporting
                if matches!(validation_result, ValidationResult::Valid) {
                    match self.wait_for_parent_validation().await {
                        ParentValidationResult::Cancelled => return parent_got_cancelled(),
                        ParentValidationResult::Ready => {
                            // Parent is ready, continue to report validation result
                        }
                    }
                }

                validation_result
            }
            futures::future::Either::Right((_, _validation_task)) => {
                return parent_got_cancelled();
            }
        }
    }

    /// Wait for parent validation to complete
    /// We do this because just because a block is valid internally, if it's not connected to a valid chain it's still not valid
    #[tracing::instrument(skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    async fn wait_for_parent_validation(&self) -> ParentValidationResult {
        let parent_chain_state_check =
            |parent_hash: BlockHash| match self.get_parent_chain_state(&parent_hash) {
                None => {
                    // Parent doesn't exist in tree - this is an error condition
                    error!(
                        block.parent_hash = %parent_hash,
                        block.hash = %self.sealed_block.header().block_hash,
                        block.height = %self.sealed_block.header().height,
                        "CRITICAL: Parent block not found"
                    );
                    ControlFlow::Break(ParentValidationResult::Cancelled)
                }
                Some(parent_state) if self.is_parent_ready(&parent_state) => {
                    debug!("Parent validation complete");
                    ControlFlow::Break(ParentValidationResult::Ready)
                }
                Some(_) => {
                    // Parent exists but not ready, wait for updates
                    ControlFlow::Continue(())
                }
            };

        self.exit_if_block_is_too_old(parent_chain_state_check)
            .await
    }

    #[tracing::instrument(skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    async fn exit_if_block_is_too_old(
        &self,
        extra_checks: impl Fn(BlockHash) -> ControlFlow<ParentValidationResult, ()>,
    ) -> ParentValidationResult {
        let parent_hash = self.sealed_block.header().previous_block_hash;

        // Subscribe to block state updates
        let mut block_state_rx = self
            .service_inner
            .service_senders
            .subscribe_block_state_updates();

        loop {
            // 1. Check cancellation condition first
            if self.should_exit_due_to_height_diff() {
                let block_tree = self.block_tree_guard.read();
                let tip_hash = block_tree.tip;
                if let Some(tip_block) = block_tree.get_block(&tip_hash) {
                    let height_diff = tip_block
                        .height
                        .saturating_sub(self.sealed_block.header().height);
                    warn!(
                        block.hash = %self.sealed_block.header().block_hash,
                        block.height = %self.sealed_block.header().height,
                        block.height_diff= height_diff,
                        config.threshold = self.service_inner.config.consensus.block_tree_depth,
                        "Cancelling validation: block too far behind tip"
                    );
                }
                return ParentValidationResult::Cancelled;
            }

            match extra_checks(parent_hash) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(result) => return result,
            }

            // 3. Wait for relevant state changes
            debug!(block.parent_hash = %parent_hash, "Waiting for parent validation");
            match block_state_rx.recv().await {
                Ok(event) if event.block_hash == parent_hash => {
                    // Parent state changed, loop back to check
                    continue;
                }
                Ok(_) => {
                    // Not our parent, continue waiting
                    continue;
                }
                Err(_) => {
                    // Channel closed - treat as error
                    error!(
                        block.parent_hash = %parent_hash,
                        block.hash = %self.sealed_block.header().block_hash,
                        block.height = %self.sealed_block.header().height,
                        "Block state channel closed while waiting for parent"
                    );
                    return ParentValidationResult::Cancelled;
                }
            }
        }
    }

    /// Check if the block should exit due to height difference from canonical tip
    fn should_exit_due_to_height_diff(&self) -> bool {
        let block_tree = self.block_tree_guard.read();
        let tip_hash = block_tree.tip;

        if let Some(tip_block) = block_tree.get_block(&tip_hash) {
            let height_diff = tip_block
                .height
                .saturating_sub(self.sealed_block.header().height);
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

    /// Perform block validation
    #[tracing::instrument(skip_all, fields(block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height))]
    async fn validate_block(&self) -> ValidationResult {
        let skip_vdf_validation = self.skip_vdf_validation;
        let poa = self.sealed_block.header().poa.clone();
        let miner_address = self.sealed_block.header().miner_address;
        let block = self.sealed_block.header();

        // Recall range validation
        let recall_task = async move {
            recall_recall_range_is_valid(
                block,
                &self.service_inner.config.consensus,
                &self.service_inner.vdf_state,
            )
            .await
            .map(|()| ValidationResult::Valid)
            .unwrap_or_else(|err| {
                tracing::error!(
                    custom.error = ?err,
                    "recall range validation failed"
                );
                ValidationResult::Invalid(ValidationError::RecallRangeInvalid(err.to_string()))
            })
        }
        .instrument(tracing::info_span!("recall_range_validation", block.hash = %self.sealed_block.header().block_hash, block.height = %self.sealed_block.header().height));

        let parent_epoch_snapshot = match self
            .block_tree_guard
            .read()
            .get_epoch_snapshot(&block.previous_block_hash)
        {
            Some(snapshot) => snapshot,
            None => {
                tracing::error!(
                    block.parent_hash = %block.previous_block_hash,
                    "Parent epoch snapshot not found"
                );
                return ValidationResult::Invalid(ValidationError::ParentEpochSnapshotMissing {
                    block_hash: block.previous_block_hash,
                });
            }
        };
        tracing::info!("Using parent epoch snapshot for PoA validation");

        // POA validation
        let block_hash_for_error_log = self.sealed_block.header().block_hash;
        let block_height_for_error_log = self.sealed_block.header().height;
        let poa_task = {
            let consensus_config = self.service_inner.config.consensus.clone();
            let block_index_guard = self.service_inner.block_index_guard.clone();
            let block_hash = self.sealed_block.header().block_hash;
            let block_height = self.sealed_block.header().height;
            {
                let poa_span = tracing::info_span!(
                    "poa_validation",
                    block.hash = %block_hash,
                    block.height = %block_height
                );
                tokio::task::spawn_blocking(move || {
                    let _guard = poa_span.enter();
                    if skip_vdf_validation {
                        debug!(block.hash = ?block_hash, "Skipping POA validation due to skip_vdf_validation flag");
                        return Ok(ValidationResult::Valid);
                    }
                    poa_is_valid(
                        &poa,
                        &block_index_guard,
                        &parent_epoch_snapshot,
                        &consensus_config,
                        &miner_address,
                    )
                    .map(|()| ValidationResult::Valid)
                })
            }
        };

        let poa_task = async move {
            let res = poa_task.await;

            match res {
                Ok(res) => res.unwrap_or_else(|e| {
                    tracing::error!(
                        block.hash = %block_hash_for_error_log,
                        block.height = %block_height_for_error_log,
                        custom.error = ?e,
                        "PoA validation failed"
                    );
                    ValidationResult::Invalid(ValidationError::PreValidation(e))
                }),
                Err(err) => {
                    tracing::error!(
                        block.hash = %block_hash_for_error_log,
                        block.height = %block_height_for_error_log,
                        custom.error = ?err,
                        "poa task panicked"
                    );
                    ValidationResult::Invalid(ValidationError::TaskPanicked {
                        task: "poa".to_string(),
                        details: format!("{:?}", err),
                    })
                }
            }
        };

        // Shadow transaction validation (pure validation, no reth submission)
        let config = &self.service_inner.config;
        let service_senders = &self.service_inner.service_senders;

        // Get parent epoch snapshot for expired ledger fee calculation
        let parent_epoch_snapshot = match self
            .block_tree_guard
            .read()
            .get_epoch_snapshot(&block.previous_block_hash)
        {
            Some(snapshot) => snapshot,
            None => {
                tracing::error!(
                    block.parent_hash = %block.previous_block_hash,
                    "Parent epoch snapshot not found for shadow tx validation"
                );
                return ValidationResult::Invalid(ValidationError::ParentEpochSnapshotMissing {
                    block_hash: block.previous_block_hash,
                });
            }
        };

        // Get block index (convert read guard to Arc<RwLock>)
        let block_index = self.service_inner.block_index_guard.inner();

        let sealed_block_for_shadow = self.sealed_block.clone();
        let shadow_tx_task = async move {
            let parent_commitment_snapshot = self
                .block_tree_guard
                .read()
                .get_commitment_snapshot(&block.previous_block_hash)
                .with_context(|| {
                    format!(
                        "parent block {} should have a commitment snapshot in the block_tree",
                        block.previous_block_hash
                    )
                })?;
            shadow_transactions_are_valid(
                config,
                service_senders,
                &self.block_tree_guard,
                &self.service_inner.mempool_guard,
                block,
                &self.service_inner.db,
                self.service_inner.execution_payload_provider.clone(),
                parent_epoch_snapshot,
                parent_commitment_snapshot,
                block_index,
                sealed_block_for_shadow.transactions(),
            )
            .instrument(tracing::info_span!(
                "shadow_tx_validation",
                block.hash = %self.sealed_block.header().block_hash,
                block.height = %self.sealed_block.header().height
            ))
            .await
            .inspect_err(|err| {
                tracing::error!(
                    custom.error = ?err,
                    "shadow transaction validation failed"
                )
            })
        };

        let vdf_reset_frequency = self.service_inner.config.vdf.reset_frequency as u64;
        let seeds_block_hash = self.sealed_block.header().block_hash;
        let seeds_block_height = self.sealed_block.header().height;
        let seeds_validation_task = async move {
            let binding = self.block_tree_guard.read();
            let previous_block =
                match binding.get_block(&self.sealed_block.header().previous_block_hash) {
                    Some(block) => block,
                    None => {
                        tracing::error!(
                            block.parent_hash = %self.sealed_block.header().previous_block_hash,
                            "Previous block not found in block tree"
                        );
                        return ValidationResult::Invalid(ValidationError::ParentBlockMissing {
                            block_hash: self.sealed_block.header().previous_block_hash,
                        });
                    }
                };
            is_seed_data_valid(
                self.sealed_block.header(),
                previous_block,
                vdf_reset_frequency,
            )
        }
        .instrument(tracing::info_span!(
            "seeds_validation",
            block.hash = %seeds_block_hash,
            block.height = %seeds_block_height
        ));

        // Commitment transaction ordering validation
        let sealed_block_for_commitment = self.sealed_block.clone();
        let commitment_ordering_task = async move {
            commitment_txs_are_valid(
                config,
                block,
                &self.block_tree_guard,
                sealed_block_for_commitment
                    .transactions()
                    .get_ledger_system_txs(SystemLedger::Commitment),
            )
            .instrument(tracing::info_span!("commitment_ordering_validation"))
            .await
            .map(|()| ValidationResult::Valid)
            .unwrap_or_else(|err| {
                tracing::error!(
                    custom.error = ?err,
                    "commitment ordering validation failed"
                );
                ValidationResult::Invalid(err)
            })
        };

        // Data transaction fee validation
        let sealed_block_for_data = self.sealed_block.clone();
        let data_txs_validation_task = async move {
            let txs = sealed_block_for_data.transactions();
            let mut term_txs: Vec<irys_types::DataTransactionHeader> = Vec::new();
            for ledger in [
                irys_types::DataLedger::OneYear,
                irys_types::DataLedger::ThirtyDay,
            ] {
                term_txs.extend_from_slice(txs.get_ledger_txs(ledger));
            }
            data_txs_are_valid(
                config,
                service_senders,
                block,
                &self.service_inner.db,
                &self.block_tree_guard,
                txs.get_ledger_txs(irys_types::DataLedger::Submit),
                txs.get_ledger_txs(irys_types::DataLedger::Publish),
                &term_txs,
            )
            .instrument(tracing::info_span!(
                "data_txs_validation",
                block.hash = %self.sealed_block.header().block_hash,
                block.height = %self.sealed_block.header().height
            ))
            .await
            .map(|()| ValidationResult::Valid)
            .unwrap_or_else(|e| {
                tracing::error!(
                    custom.error = ?e,
                    "data transaction validation failed"
                );
                ValidationResult::Invalid(ValidationError::PreValidation(e))
            })
        };

        // Wait for all validation tasks to complete
        let (
            recall_result,
            poa_result,
            shadow_tx_result,
            seeds_validation_result,
            commitment_ordering_result,
            data_txs_result,
        ) = tokio::join!(
            recall_task,
            poa_task,
            shadow_tx_task,
            seeds_validation_task,
            commitment_ordering_task,
            data_txs_validation_task
        );

        // Check shadow_tx_result first to extract ExecutionData
        let execution_data = match shadow_tx_result {
            Ok(data) => data,
            Err(err) => {
                tracing::error!(custom.error = ?err, "Shadow transaction validation failed, not submitting to reth");
                return ValidationResult::Invalid(ValidationError::ShadowTransactionInvalid(
                    err.to_string(),
                ));
            }
        };

        match (
            &recall_result,
            &poa_result,
            &seeds_validation_result,
            &commitment_ordering_result,
            &data_txs_result,
        ) {
            (
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
            ) => {
                tracing::debug!("All consensus validations successful, submitting to reth");

                // All consensus layer validations passed, now submit to execution layer
                let reth_result = submit_payload_to_reth(
                    self.sealed_block.header(),
                    &self.service_inner.reth_node_adapter,
                    execution_data,
                )
                .instrument(tracing::error_span!(
                    "reth_submission",
                    block.hash = %self.sealed_block.header().block_hash,
                    block.height = %self.sealed_block.header().height
                ))
                .await;

                match reth_result {
                    Ok(()) => {
                        tracing::debug!("Reth execution layer validation successful");
                        ValidationResult::Valid
                    }
                    Err(err) => {
                        tracing::error!(custom.error = ?err, "Reth execution layer validation failed");
                        ValidationResult::Invalid(ValidationError::ExecutionLayerFailed(
                            err.to_string(),
                        ))
                    }
                }
            }
            _ => {
                tracing::debug!("Consensus validation failed, not submitting to reth");
                // At least one validation failed, return the first Invalid result
                let first_invalid = [
                    &recall_result,
                    &poa_result,
                    &seeds_validation_result,
                    &commitment_ordering_result,
                    &data_txs_result,
                ]
                .into_iter()
                .find_map(|r| match r {
                    ValidationResult::Invalid(e) => Some(e.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    ValidationError::Other("consensus validation failed".to_string())
                });
                ValidationResult::Invalid(first_invalid)
            }
        }
    }
}
