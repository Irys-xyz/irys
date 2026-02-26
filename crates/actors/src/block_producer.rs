use crate::{
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl},
    chunk_ingress_service::ChunkIngressState,
    mempool_guard::MempoolReadGuard,
    mempool_service::MempoolTxs,
    metrics,
    mining_bus::{BroadcastDifficultyUpdate, MiningBus},
    services::ServiceSenders,
    shadow_tx_generator::{PublishLedgerWithTxs, ShadowTxGenerator},
};
use alloy_consensus::{
    EthereumTxEnvelope, SignableTransaction as _, TxEip4844, transaction::SignerRecoverable as _,
};
use alloy_eips::BlockHashOrNumber;
use alloy_network::TxSignerSync as _;
use alloy_rpc_types_engine::{
    ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, PayloadAttributes, PayloadStatusEnum,
};
use alloy_signer_local::LocalSigner;
use eyre::{OptionExt as _, eyre};
use irys_domain::{
    BlockIndex, BlockTreeReadGuard, CommitmentSnapshot, EmaSnapshot, EpochSnapshot,
    ExponentialMarketAvgCalculation, HardforkConfigExt as _,
};
use irys_price_oracle::IrysPriceOracle;
use irys_reth::{
    IrysEthereumNode, IrysPayloadAttributes, IrysPayloadBuilderAttributes, IrysPayloadTypes,
    compose_shadow_tx, reth_node_ethereum::EthEngineTypes,
};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reth_node_bridge::node::NodeProvider;
use irys_reward_curve::HalvingCurve;
use irys_types::SystemLedger;
use irys_types::{
    AdjustmentStats, Base64, BlockBody, CommitmentTransaction, Config, DataLedger,
    DataTransactionHeader, DataTransactionLedger, H256, H256List, IrysAddress, IrysBlockHeader,
    IrysTokenPrice, PoaData, SealedBlock as IrysSealedBlock, SendTraced as _, Signature,
    SystemTransactionLedger, TokioServiceHandle, Traced, U256, UnixTimestamp, UnixTimestampMs,
    VDFLimiterInfo, app_state::DatabaseProvider, block_production::SolutionContext,
    calculate_difficulty, next_cumulative_diff, storage_pricing::Amount,
};
use irys_vdf::state::VdfStateReadonly;
use ledger_expiry::LedgerExpiryBalanceDelta;
use nodit::interval::ii;
use openssl::sha;
use reth::{
    api::{ConsensusEngineHandle, NodeTypes, PayloadKind},
    core::primitives::SealedBlock,
    payload::{EthBuiltPayload, PayloadBuilderHandle},
    revm::primitives::B256,
    tasks::shutdown::Shutdown,
};
use reth_payload_primitives::{PayloadBuilderAttributes as _, PayloadBuilderError};
use reth_transaction_pool::EthPooledTransaction;
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tracing::{Instrument as _, debug, error, error_span, info, warn};

/// Error type for block production that distinguishes between retryable and irrecoverable errors
#[derive(Debug, thiserror::Error)]
pub enum BlockProductionError {
    /// Retryable errors that should trigger a rebuild attempt with a new parent
    #[error("retryable error during block production")]
    Retryable {
        #[source]
        source: eyre::Error,
    },
    /// Irrecoverable errors that should abort block production
    #[error("irrecoverable error during block production")]
    Irrecoverable {
        #[source]
        source: eyre::Error,
    },
}

impl From<eyre::Report> for BlockProductionError {
    fn from(source: eyre::Report) -> Self {
        Self::Irrecoverable { source }
    }
}

/// Classifies PayloadBuilderError into retryable or irrecoverable categories
fn classify_payload_error(err: PayloadBuilderError) -> BlockProductionError {
    match err {
        // Retryable errors - parent block/header not yet available
        e @ (PayloadBuilderError::MissingParentHeader(_)
        | PayloadBuilderError::MissingParentBlock(_)) => {
            BlockProductionError::Retryable { source: e.into() }
        }

        // All other errors are irrecoverable
        e => BlockProductionError::Irrecoverable { source: e.into() },
    }
}

mod block_validation_tracker;
pub mod ledger_expiry;
pub use block_validation_tracker::BlockValidationTracker;
use irys_types::v2::GossipBroadcastMessageV2;

/// Result of checking parent validity and solution compatibility
#[derive(Debug)]
pub enum ParentCheckResult {
    /// Parent is still the best canonical block - keep current block
    ParentStillBest,
    /// Parent changed but solution is valid - must rebuild on new parent
    MustRebuild { new_parent: H256 },
    /// Solution is completely invalid and must be discarded
    SolutionInvalid {
        new_parent: H256,
        reason: InvalidReason,
    },
}

/// Reason why a solution is completely invalid
#[derive(Debug)]
pub enum InvalidReason {
    /// Solution VDF step is at or before the new parent's VDF step
    VdfTooOld {
        parent_vdf_step: u64,
        solution_vdf_step: u64,
    },
}

/// Commands that can be sent to the block producer service
#[derive(Debug)]
pub enum BlockProducerCommand {
    /// Announce to the node a mining solution has been found
    SolutionFound {
        solution: SolutionContext,
        response: oneshot::Sender<eyre::Result<Option<(Arc<IrysSealedBlock>, EthBuiltPayload)>>>,
    },
    /// Set the test blocks remaining (for testing)
    SetTestBlocksRemaining(Option<u64>),
}

/// Block producer service that creates blocks from mining solutions
#[derive(Debug)]
pub struct BlockProducerService {
    /// Graceful shutdown handle
    shutdown: Shutdown,
    /// Command receiver
    cmd_rx: mpsc::UnboundedReceiver<Traced<BlockProducerCommand>>,
    /// Inner logic
    inner: Arc<BlockProducerInner>,
    /// Enforces block production limits during testing
    blocks_remaining_for_test: Option<u64>,
}

#[derive(Debug)]
pub struct BlockProducerInner {
    /// Reference to the global database
    pub db: DatabaseProvider,
    /// Message the block discovery actor when a block is produced locally
    pub block_discovery: BlockDiscoveryFacadeImpl,
    /// Mining broadcast service
    pub mining_broadcaster: MiningBus,
    /// Reference to all the services we can send messages to
    pub service_senders: ServiceSenders,
    /// Global config
    pub config: Config,
    /// The block reward curve
    pub reward_curve: Arc<HalvingCurve>,
    /// Store last VDF Steps
    pub vdf_steps_guard: VdfStateReadonly,
    /// Get the head of the chain
    pub block_tree_guard: BlockTreeReadGuard,
    /// The Irys price oracle
    pub price_oracle: Arc<IrysPriceOracle>,
    /// Reth node payload builder
    pub reth_payload_builder: PayloadBuilderHandle<EthEngineTypes<IrysPayloadTypes>>,
    /// Reth blockchain provider
    pub reth_provider: NodeProvider,
    /// Reth beacon engine handle
    pub consensus_engine_handle: ConsensusEngineHandle<<IrysEthereumNode as NodeTypes>::Payload>,
    /// Block index
    pub block_index: BlockIndex,
    /// Read guard for mempool state
    pub mempool_guard: MempoolReadGuard,
    /// Reth node adapter for RPC queries (balance checks during tx selection)
    pub reth_node_adapter: IrysRethNodeAdapter,
    /// Shared state handle for chunk ingress (used by tx selection)
    pub chunk_ingress_state: ChunkIngressState,
}

/// Event emitted on epoch blocks to refund Unpledge commitments (fee charged at inclusion; value refunded at epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnpledgeRefundEvent {
    pub account: IrysAddress,
    pub amount: U256,
    pub irys_ref_txid: H256,
}

impl Ord for UnpledgeRefundEvent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.amount
            .cmp(&other.amount)
            .then(self.irys_ref_txid.cmp(&other.irys_ref_txid))
    }
}

impl PartialOrd for UnpledgeRefundEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Event emitted on epoch blocks to refund Unstake commitments (fee charged at inclusion; value refunded at epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnstakeRefundEvent {
    pub account: IrysAddress,
    pub amount: U256,
    pub irys_ref_txid: H256,
}

impl Ord for UnstakeRefundEvent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.amount
            .cmp(&other.amount)
            .then(self.irys_ref_txid.cmp(&other.irys_ref_txid))
    }
}

impl PartialOrd for UnstakeRefundEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Named result bundle for mempool-derived inputs to block production.
#[derive(Debug, Clone)]
pub struct MempoolTxsBundle {
    /// Commitment txs included in the commitment ledger
    pub commitment_txs: Vec<CommitmentTransaction>,

    /// commitment txs included in the commitment ledger and billed to
    /// the user during shadow tx processing (empty on epochs)
    pub commitment_txs_to_bill: Vec<CommitmentTransaction>,

    pub submit_txs: Vec<DataTransactionHeader>,
    pub publish_txs: PublishLedgerWithTxs,
    pub aggregated_miner_fees: LedgerExpiryBalanceDelta,

    /// Unpledge refund events to emit on epoch blocks; empty on non-epoch blocks
    pub commitment_refund_events: Vec<UnpledgeRefundEvent>,
    /// Unstake refund events to emit on epoch blocks; empty on non-epoch blocks
    pub unstake_refund_events: Vec<UnstakeRefundEvent>,

    /// Epoch snapshot for the parent block - used to resolve reward addresses
    pub epoch_snapshot: Arc<EpochSnapshot>,
}

impl BlockProducerService {
    /// Spawn a new block producer service
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_block_producer")]
    pub fn spawn_service(
        inner: Arc<BlockProducerInner>,
        blocks_remaining_for_test: Option<u64>,
        rx: mpsc::UnboundedReceiver<Traced<BlockProducerCommand>>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!(
            "Spawning block producer service with blocks_remaining_for_test: {:?}",
            blocks_remaining_for_test
        );

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let handle = runtime_handle.spawn(
            async move {
                let service = Self {
                    shutdown: shutdown_rx,
                    cmd_rx: rx,
                    inner,
                    blocks_remaining_for_test,
                };
                service
                    .start()
                    .await
                    .expect("Block producer service encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "block_producer_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, ret, err)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting block producer service");
        debug!(
            "Service configuration - reward_address: {}, chain_id: {}",
            self.inner.config.node_config.reward_address, self.inner.config.consensus.chain_id
        );
        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for block producer service");
                    break;
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(traced) => {
                            let (cmd, parent_span) = traced.into_parts();
                            let span = tracing::trace_span!(parent: &parent_span, "block_producer_handle_command");
                            if self.handle_command(cmd).instrument(span).await? {
                                break;
                            }
                        }
                        None => {
                            warn!("Command channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("Shutting down block producer service gracefully");
        Ok(())
    }

    /// Handles a single command. Returns `true` if the service should shut down.
    async fn handle_command(&mut self, cmd: BlockProducerCommand) -> eyre::Result<bool> {
        match cmd {
            BlockProducerCommand::SolutionFound { solution, response } => {
                let solution_hash = solution.solution_hash;
                info!(
                    solution.hash = %solution_hash,
                    solution.vdf_step = solution.vdf_step,
                    solution.mining_address = %solution.mining_address,
                    "Block producer received mining solution"
                );

                if let Some(blocks_remaining) = self.blocks_remaining_for_test {
                    if blocks_remaining == 0 {
                        info!(
                            solution.hash = %solution_hash,
                            "No more blocks needed for test, skipping block production"
                        );
                        let _ = response.send(Ok(None));
                        return Ok(false);
                    }
                    debug!("Test blocks remaining: {}", blocks_remaining);
                }

                let production_strategy = ProductionStrategy {
                    inner: self.inner.clone(),
                };

                // Race production against shutdown so we don't block graceful shutdown.
                let result = tokio::select! {
                    biased;
                    _ = &mut self.shutdown => {
                        info!("Shutdown during block production, cancelling");
                        let _ = response.send(Ok(None));
                        return Ok(true);
                    }
                    r = production_strategy.fully_produce_new_block(solution) => r?
                };

                if let Some((block, eth_built_payload)) = result {
                    // Final guard: ensure tests haven't exhausted quota
                    if matches!(self.blocks_remaining_for_test, Some(0)) {
                        info!("Test guard exhausted; dropping candidate block before publication");
                        let _ = response.send(Ok(None));
                        return Ok(false);
                    }

                    info!(
                        block.hash = %block.header().block_hash,
                        block.height = block.header().height,
                        "Block publication completed successfully"
                    );
                    metrics::record_block_produced();

                    if let Some(remaining) = self.blocks_remaining_for_test.as_mut() {
                        *remaining = remaining.saturating_sub(1);
                        debug!("Test blocks remaining after publication: {}", *remaining);
                    }

                    let _ = response.send(Ok(Some((block, eth_built_payload))));
                    return Ok(false);
                } else {
                    info!("Block production skipped (solution outdated or invalid)");
                    let _ = response.send(Ok(None));
                    return Ok(false);
                }
            }
            BlockProducerCommand::SetTestBlocksRemaining(remaining) => {
                debug!(
                    custom.old_value = ?self.blocks_remaining_for_test,
                    custom.new_value = ?remaining,
                    "Updating test blocks remaining"
                );
                self.blocks_remaining_for_test = remaining;
            }
        }
        Ok(false)
    }
}

#[async_trait::async_trait]
pub trait BlockProdStrategy {
    fn inner(&self) -> &BlockProducerInner;

    /// Creates PoA data from the solution context
    /// Returns (PoaData, chunk_hash)
    fn create_poa_data(
        &self,
        solution: &SolutionContext,
        ledger_id: Option<u32>,
    ) -> eyre::Result<(PoaData, H256)> {
        let poa_chunk = Base64(solution.chunk.clone());
        let poa_chunk_hash = H256(sha::sha256(&poa_chunk.0));
        let poa = PoaData {
            tx_path: solution.tx_path.clone().map(Base64),
            data_path: solution.data_path.clone().map(Base64),
            chunk: Some(poa_chunk),

            ledger_id,
            partition_chunk_offset: solution.chunk_offset,
            partition_hash: solution.partition_hash,
        };
        Ok((poa, poa_chunk_hash))
    }

    /// Fetches a block header from mempool or database
    async fn fetch_block_header(&self, block_hash: H256) -> eyre::Result<IrysBlockHeader> {
        crate::block_header_lookup::get_block_header(
            &self.inner().block_tree_guard,
            &self.inner().db,
            block_hash,
            false,
        )?
        .ok_or_else(|| eyre!("No block header found for hash {}", block_hash))
    }

    /// Gets the EMA snapshot for a given block
    fn get_block_ema_snapshot(&self, block_hash: &H256) -> eyre::Result<Arc<EmaSnapshot>> {
        let read = self.inner().block_tree_guard.read();
        read.get_ema_snapshot(block_hash)
            .ok_or_else(|| eyre!("EMA snapshot not found for block {}", block_hash))
    }

    /// Checks parent validity and determines if the solution is still valid.
    /// Returns a descriptive result indicating the required action.
    async fn check_parent_and_solution_validity(
        &self,
        parent_hash: &H256,
        solution: &SolutionContext,
    ) -> ParentCheckResult {
        let tree = self.inner().block_tree_guard.read();
        let (_max_difficulty, current_best) = tree.get_max_cumulative_difficulty_block();

        // Check if parent is still the best
        if current_best == *parent_hash {
            return ParentCheckResult::ParentStillBest;
        }

        // Parent changed - get new parent's VDF step
        let new_parent_vdf_step = tree
            .get_block(&current_best)
            .map(|b| b.vdf_limiter_info.global_step_number)
            .unwrap_or(0);

        // Check if solution is too old (at or before new parent's VDF step)
        if solution.vdf_step <= new_parent_vdf_step {
            return ParentCheckResult::SolutionInvalid {
                new_parent: current_best,
                reason: InvalidReason::VdfTooOld {
                    parent_vdf_step: new_parent_vdf_step,
                    solution_vdf_step: solution.vdf_step,
                },
            };
        }

        // Parent changed but solution is valid - must rebuild on new parent
        ParentCheckResult::MustRebuild {
            new_parent: current_best,
        }
    }

    /// Core block production logic that can be used for both initial production and rebuilds.
    async fn produce_block_with_parent(
        &self,
        solution: &SolutionContext,
        prev_block_header: IrysBlockHeader,
        prev_block_ema_snapshot: Arc<EmaSnapshot>,
    ) -> Result<
        Option<(
            Arc<IrysSealedBlock>,
            Option<AdjustmentStats>,
            EthBuiltPayload,
        )>,
        BlockProductionError,
    > {
        if solution.vdf_step <= prev_block_header.vdf_limiter_info.global_step_number {
            warn!(
                "Skipping solution for old step number {}, previous block step number {} for block {}",
                solution.vdf_step,
                prev_block_header.vdf_limiter_info.global_step_number,
                prev_block_header.block_hash
            );
            return Ok(None);
        }

        let prev_evm_block = self.get_evm_block(&prev_block_header).await?;
        let current_timestamp = current_timestamp(&prev_block_header).await;

        let mempool_bundle = self
            .get_mempool_txs(&prev_block_header, current_timestamp)
            .await?;

        let block_reward = self.block_reward(&prev_block_header)?;

        let (eth_built_payload, final_treasury) = self
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &mempool_bundle,
                block_reward,
                current_timestamp,
                solution.solution_hash,
            )
            .await?;
        let evm_block = eth_built_payload.block();

        let block_result = self
            .produce_block_without_broadcasting(
                solution,
                &prev_block_header,
                mempool_bundle,
                current_timestamp,
                block_reward,
                evm_block,
                &prev_block_ema_snapshot,
                final_treasury,
            )
            .await?;

        let Some((block, stats)) = block_result else {
            return Ok(None);
        };

        Ok(Some((block, stats, eth_built_payload)))
    }

    /// Selects the parent block for new block production.
    ///
    /// Targets the block with highest cumulative difficulty, but only if fully validated.
    /// Waits indefinitely for validation to complete, switching targets if a new block
    /// with higher cumulative difficulty appears.
    ///
    /// Returns the selected parent block header and its EMA snapshot.
    #[tracing::instrument(skip_all, level = "debug")]
    async fn parent_irys_block(&self) -> eyre::Result<(IrysBlockHeader, Arc<EmaSnapshot>)> {
        let inner = self.inner();
        // Use BlockValidationTracker to select the parent block
        let parent_block_hash = BlockValidationTracker::new(
            inner.block_tree_guard.clone(),
            inner.service_senders.clone(),
        )
        .wait_for_validation()
        .await?;

        // Fetch the parent block header
        let header = self.fetch_block_header(parent_block_hash).await?;

        // Get the EMA snapshot
        let ema_snapshot = self.get_block_ema_snapshot(&header.block_hash)?;

        Ok((header, ema_snapshot))
    }

    async fn fully_produce_new_block_without_gossip(
        &self,
        solution: &SolutionContext,
    ) -> Result<
        Option<(
            Arc<IrysSealedBlock>,
            Option<AdjustmentStats>,
            EthBuiltPayload,
        )>,
        BlockProductionError,
    > {
        const MAX_RETRY_ATTEMPTS: usize = 5;
        let mut retry_count = 0;

        loop {
            // Fetch the current best parent block
            let (prev_block_header, prev_block_ema_snapshot) = self.parent_irys_block().await?;
            let block_hash = prev_block_header.block_hash;

            // Attempt to produce the block
            match self
                .produce_block_with_parent(solution, prev_block_header, prev_block_ema_snapshot)
                .instrument(error_span!("produce_block_with_parent", parent.block = ?block_hash))
                .await
            {
                Ok(result) => {
                    if retry_count > 0 {
                        error!(
                            solution.hash = %solution.solution_hash,
                            solution.vdf_step = solution.vdf_step,
                            retry.count = retry_count,
                            "RETRY_SUCCESS: Block produced after retryable errors"
                        );
                    }
                    return Ok(result);
                }
                Err(BlockProductionError::Retryable { source }) => {
                    retry_count += 1;
                    metrics::record_block_producer_retry();
                    if retry_count >= MAX_RETRY_ATTEMPTS {
                        error!(
                            solution.hash = %solution.solution_hash,
                            solution.vdf_step = solution.vdf_step,
                            error.type = "retryable",
                            error.source = %source,
                            retry.count = retry_count,
                            retry.max_attempts = MAX_RETRY_ATTEMPTS,
                            "Max retry attempts reached for retryable error, aborting"
                        );
                        return Err(BlockProductionError::Irrecoverable {
                            source: source
                                .wrap_err(format!("max retries ({}) exceeded", MAX_RETRY_ATTEMPTS)),
                        });
                    }

                    warn!(
                        solution.hash = %solution.solution_hash,
                        solution.vdf_step = solution.vdf_step,
                        error.type = "retryable",
                        error.source = %source,
                        retry.attempt = retry_count,
                        retry.max_attempts = MAX_RETRY_ATTEMPTS,
                        "Retryable error during block production, will retry with new parent"
                    );
                    // Continue loop to retry with fresh parent
                }
                Err(e @ BlockProductionError::Irrecoverable { .. }) => {
                    error!(
                        solution.hash = %solution.solution_hash,
                        solution.vdf_step = solution.vdf_step,
                        error.type = "irrecoverable",
                        "Irrecoverable error during block production, aborting"
                    );
                    return Err(e);
                }
            }
        }
    }

    /// Produces a new block candidate with automatic parent chain rebuild capability.
    /// This does NOT broadcast or publish the block.
    ///
    /// # Race Condition Handling
    /// This function addresses a critical race condition where the canonical parent block
    /// can change while we're producing a block, which would waste the valuable mining solution.
    ///
    /// After producing a block, we check if the parent is still the best canonical block.
    /// If not, we rebuild the block on the new parent, reusing the same solution hash.
    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn fully_produce_new_block_candidate(
        &self,
        solution: SolutionContext,
    ) -> Result<
        Option<(
            Arc<IrysSealedBlock>,
            Option<AdjustmentStats>,
            EthBuiltPayload,
        )>,
        BlockProductionError,
    > {
        let mut rebuild_attempts = 0;

        // Initial block production
        let mut result = self
            .fully_produce_new_block_without_gossip(&solution)
            .await?;

        // Check if we need to rebuild on a new parent
        while let Some((ref block, _, _)) = result {
            let parent_hash = &block.header().previous_block_hash;

            match self
                .check_parent_and_solution_validity(parent_hash, &solution)
                .await
            {
                ParentCheckResult::ParentStillBest => {
                    // Parent is still the best, keep the current block
                    break;
                }

                ParentCheckResult::MustRebuild { new_parent } => {
                    info!(
                        solution.hash = %solution.solution_hash,
                        solution.vdf_step = solution.vdf_step,
                        block.new_parent = %new_parent,
                        block.rebuild_attempt = rebuild_attempts + 1,
                        "Parent changed but solution is valid - rebuilding on new parent"
                    );

                    // Rebuild the block on the new parent
                    result = self
                        .fully_produce_new_block_without_gossip(&solution)
                        .await?;
                    rebuild_attempts += 1;
                }

                ParentCheckResult::SolutionInvalid { new_parent, reason } => {
                    // Log the specific reason why solution is invalid
                    match &reason {
                        InvalidReason::VdfTooOld {
                            parent_vdf_step,
                            solution_vdf_step,
                        } => {
                            warn!(
                                solution.hash = %solution.solution_hash,
                                solution.vdf_step = solution.vdf_step,
                                block.new_parent = %new_parent,
                                block.parent_vdf_step = parent_vdf_step,
                                "Solution is too old for new parent (vdf_step {} <= {}), discarding",
                                solution_vdf_step,
                                parent_vdf_step
                            );
                        }
                    }
                    // Solution is completely invalid, cannot produce a block
                    return Ok(None);
                }
            }
        }

        let Some((block, stats, eth_built_payload)) = result else {
            return Ok(None);
        };

        if rebuild_attempts > 0 {
            info!(
                block.solution_hash = %solution.solution_hash,
                block.final_parent = %block.header().previous_block_hash,
                block.block_height = block.header().height,
                block.rebuild_count = rebuild_attempts,
                "REBUILD_SUCCESS: Block successfully rebuilt after parent changes"
            );
            metrics::record_block_producer_rebuild(rebuild_attempts as u64);
        }

        if !block.header().data_ledgers[DataLedger::Publish]
            .tx_ids
            .is_empty()
        {
            debug!(
                "Publish Block:\n hash:{}\n height: {}\n solution_hash: {}\n global_step:{}\n parent: {}\n publish txids: {:#?}",
                block.header().block_hash,
                block.header().height,
                block.header().solution_hash,
                block.header().vdf_limiter_info.global_step_number,
                block.header().previous_block_hash,
                block.header().data_ledgers[DataLedger::Publish].tx_ids,
            );
        }

        Ok(Some((block, stats, eth_built_payload)))
    }

    /// Produces and broadcasts a new block. Kept for tests and direct strategies.
    async fn fully_produce_new_block(
        &self,
        solution: SolutionContext,
    ) -> eyre::Result<Option<(Arc<IrysSealedBlock>, EthBuiltPayload)>> {
        let Some((block, stats, eth_built_payload)) =
            self.fully_produce_new_block_candidate(solution).await?
        else {
            return Ok(None);
        };

        let block = self
            .broadcast_block(block, stats, &eth_built_payload)
            .await?;
        let Some(block) = block else { return Ok(None) };
        Ok(Some((block, eth_built_payload)))
    }

    /// Extracts and collects all transactions that should be included in a block
    /// Returns the EthBuiltPayload and the final treasury balance
    async fn create_evm_block(
        &self,
        prev_block_header: &IrysBlockHeader,
        perv_evm_block: &reth_ethereum_primitives::Block,
        mempool: &MempoolTxsBundle,
        reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
        timestamp_ms: UnixTimestampMs,
        solution_hash: H256,
    ) -> Result<(EthBuiltPayload, U256), BlockProductionError> {
        let block_height = prev_block_header.height + 1;
        let local_signer = LocalSigner::from(self.inner().config.irys_signer().signer);

        // Get treasury balance from previous block
        let initial_treasury_balance = prev_block_header.treasury;

        // Generate expected shadow transactions using shared logic
        let mut shadow_tx_generator = ShadowTxGenerator::new(
            &block_height,
            &self.inner().config.node_config.reward_address,
            &reward_amount.amount,
            prev_block_header,
            &solution_hash,
            &self.inner().config.consensus,
            &mempool.commitment_txs_to_bill,
            &mempool.submit_txs,
            &mempool.publish_txs,
            initial_treasury_balance,
            &mempool.aggregated_miner_fees,
            &mempool.commitment_refund_events,
            &mempool.unstake_refund_events,
            &mempool.epoch_snapshot,
        )?;

        let mut shadow_txs = Vec::new();
        for tx_result in shadow_tx_generator.by_ref() {
            let metadata = tx_result?;
            let mut tx_raw = compose_shadow_tx(
                self.inner().config.consensus.chain_id,
                &metadata.shadow_tx,
                metadata.transaction_fee,
            );
            let signature = local_signer
                .sign_transaction_sync(&mut tx_raw)
                .expect("shadow tx must always be signable");
            let tx = EthereumTxEnvelope::<TxEip4844>::Eip1559(tx_raw.into_signed(signature))
                .try_into_recovered()
                .expect("shadow tx must always be signable");

            shadow_txs.push(EthPooledTransaction::new(tx, 300));
        }

        // Get the final treasury balance after all transactions
        let final_treasury_balance = shadow_tx_generator.treasury_balance();

        let payload = self
            .build_and_submit_reth_payload(
                prev_block_header,
                timestamp_ms,
                shadow_txs,
                perv_evm_block.header.mix_hash,
            )
            .await?;

        Ok((payload, final_treasury_balance))
    }

    #[tracing::instrument(level = "trace", skip_all, fields(
        payload.parent_evm_hash = %prev_block_header.evm_block_hash,
        payload.timestamp_sec = timestamp_ms.to_secs().as_secs(),
        payload.shadow_tx_count = shadow_txs.len()
    ))]
    async fn build_and_submit_reth_payload(
        &self,
        prev_block_header: &IrysBlockHeader,
        timestamp_ms: UnixTimestampMs,
        shadow_txs: Vec<EthPooledTransaction>,
        parent_mix_hash: B256,
    ) -> Result<EthBuiltPayload, BlockProductionError> {
        debug!("Building Reth payload attributes");

        // generate payload attributes with shadow transactions
        let rpc_attributes = IrysPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: timestamp_ms.to_secs().as_secs(), // **THIS HAS TO BE SECONDS**
                prev_randao: parent_mix_hash,
                suggested_fee_recipient: self.inner().config.node_config.reward_address.into(),
                withdrawals: None, // these should ALWAYS be none
                parent_beacon_block_root: Some(prev_block_header.block_hash.into()),
            },
            shadow_txs,
        };

        debug!(
            payload.timestamp_sec = rpc_attributes.inner.timestamp,
            payload.fee_recipient = %rpc_attributes.inner.suggested_fee_recipient,
            payload.parent_beacon_root = ?rpc_attributes.inner.parent_beacon_block_root,
            payload.shadow_tx_count = rpc_attributes.shadow_txs.len(),
            "Payload attributes created"
        );

        // Convert to builder attributes - this computes the payload ID including shadow txs
        let attributes = IrysPayloadBuilderAttributes::try_new(
            prev_block_header.evm_block_hash,
            rpc_attributes,
            0, // version
        )
        .expect("IrysPayloadBuilderAttributes::try_new is infallible");

        let payload_builder = &self.inner().reth_payload_builder;
        let consensus_engine_handle = &self.inner().consensus_engine_handle;

        tracing::debug!(
            payload.id = %attributes.payload_id(),
            prev_height = prev_block_header.height,
            "Built payload attributes with shadow transactions"
        );

        // send & await the payload
        info!("Sending new payload to Reth");

        let payload_id = payload_builder
            .send_new_payload(attributes.clone())
            .await
            .map_err(|e| eyre!("Failed to send payload to builder: {}", e))?
            .map_err(classify_payload_error)?;

        debug!(
            payload.id = %payload_id,
            "Payload accepted by builder"
        );

        let built_payload = payload_builder
            .resolve_kind(payload_id, PayloadKind::WaitForPending)
            .await
            .ok_or_else(|| {
                eyre!("Failed to resolve payload future - payload builder returned None")
            })?
            .map_err(classify_payload_error)?;

        let evm_block_hash = built_payload.block().hash();
        tracing::debug!(payload.evm_block_hash = ?evm_block_hash, "produced a new evm block");
        let sidecar = ExecutionPayloadSidecar::from_block(&built_payload.block().clone().unseal());
        let payload = built_payload.clone().try_into_v5().unwrap_or_else(|e| {
            panic!(
                "failed to convert built payload to v5 for evm block hash {:?}: {:?}",
                evm_block_hash, e
            )
        });
        let new_payload_result = consensus_engine_handle
            .new_payload(ExecutionData {
                payload: ExecutionPayload::V3(payload.execution_payload),
                sidecar,
            })
            .await
            .map_err(|e| BlockProductionError::Irrecoverable {
                source: eyre::eyre!("beacon engine error: {}", e),
            })?;

        if new_payload_result.status != PayloadStatusEnum::Valid {
            return Err(BlockProductionError::Irrecoverable {
                source: eyre::eyre!("Reth has gone out of sync: {:?}", new_payload_result.status),
            });
        }

        info!(
            payload.block_hash = %built_payload.block().hash(),
            payload.tx_count = built_payload.block().body().transactions.len(),
            "Reth payload built successfully"
        );

        Ok(built_payload)
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn produce_block_without_broadcasting(
        &self,
        solution: &SolutionContext,
        prev_block_header: &IrysBlockHeader,
        mempool_bundle: MempoolTxsBundle,
        current_timestamp: UnixTimestampMs,
        block_reward: Amount<irys_types::storage_pricing::phantoms::Irys>,
        eth_built_payload: &SealedBlock<reth_ethereum_primitives::Block>,
        perv_block_ema_snapshot: &EmaSnapshot,
        final_treasury: U256,
    ) -> eyre::Result<Option<(Arc<IrysSealedBlock>, Option<AdjustmentStats>)>> {
        let prev_block_hash = prev_block_header.block_hash;
        let block_height = prev_block_header.height + 1;
        let evm_block_hash = eth_built_payload.hash();

        // Publish Ledger Transactions
        let publish_chunks_added = calculate_chunks_added(
            &mempool_bundle.publish_txs.txs,
            self.inner().config.consensus.chunk_size,
        );
        let publish_total_chunks =
            prev_block_header.data_ledgers[DataLedger::Publish].total_chunks + publish_chunks_added;
        let opt_proofs = mempool_bundle.publish_txs.proofs.clone();

        // Difficulty adjustment logic
        let mut last_diff_timestamp = prev_block_header.last_diff_timestamp;
        let current_difficulty = prev_block_header.diff;
        let (diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_difficulty,
            &self.inner().config.consensus.difficulty_adjustment,
        );

        // Did an adjustment happen?
        if stats.is_some() {
            last_diff_timestamp = current_timestamp;
        }

        let cumulative_difficulty = next_cumulative_diff(prev_block_header.cumulative_diff, diff);

        // Use the partition hash to figure out what ledger it belongs to
        let epoch_snapshot = self
            .inner()
            .block_tree_guard
            .read()
            .get_epoch_snapshot(&prev_block_hash)
            .expect("parent epoch snapshot to be retrievable");

        let ledger_id = epoch_snapshot
            .get_data_partition_assignment(solution.partition_hash)
            .and_then(|pa| pa.ledger_id);

        // Create PoA data using the trait method
        let (poa, poa_chunk_hash) = self.create_poa_data(solution, ledger_id)?;

        let mut steps = if prev_block_header.vdf_limiter_info.global_step_number + 1
            > solution.vdf_step - 1
        {
            H256List::new()
        } else {
            self.inner().vdf_steps_guard.get_steps(ii(prev_block_header.vdf_limiter_info.global_step_number + 1, solution.vdf_step - 1))
                .map_err(|e| eyre!("VDF step range {} unavailable while producing block {}, reason: {:?}, aborting", solution.vdf_step, &block_height, e))?
        };
        steps.push(solution.seed.0);

        let ema_calculation = self
            .get_ema_price(prev_block_header, perv_block_ema_snapshot)
            .await?;

        // Update the last_epoch_hash field, which tracks the most recent epoch boundary
        //
        // The logic works as follows:
        // 1. Start with the previous block's last_epoch_hash as default
        // 2. Special case: At the first block after an epoch boundary (block_height % blocks_in_epoch == 1),
        //    update last_epoch_hash to point to the epoch block itself (prev_block_hash)
        // 3. This creates a chain of references where each block knows which epoch it belongs to,
        //    and which block marked the beginning of that epoch
        let mut last_epoch_hash = prev_block_header.last_epoch_hash;

        // If this is the first block following an epoch boundary block
        if block_height > 0
            && block_height % self.inner().config.consensus.epoch.num_blocks_in_epoch == 1
        {
            // Record the hash of the epoch block (previous block) as our epoch reference
            last_epoch_hash = prev_block_hash;
        }
        let submit_chunks_added = calculate_chunks_added(
            &mempool_bundle.submit_txs,
            self.inner().config.consensus.chunk_size,
        );
        let submit_total_chunks =
            prev_block_header.data_ledgers[DataLedger::Submit].total_chunks + submit_chunks_added;

        let system_ledgers = if !mempool_bundle.commitment_txs.is_empty() {
            let mut txids = H256List::new();
            for ctx in mempool_bundle.commitment_txs.iter() {
                txids.push(ctx.id());
            }
            vec![SystemTransactionLedger {
                ledger_id: SystemLedger::Commitment.into(),
                tx_ids: txids,
            }]
        } else {
            vec![]
        };

        // build a new block header
        let mut irys_block = IrysBlockHeader::V1(irys_types::IrysBlockHeaderV1 {
            block_hash: H256::zero(), // block_hash is initialized after signing
            height: block_height,
            diff,
            cumulative_diff: cumulative_difficulty,
            last_diff_timestamp,
            solution_hash: solution.solution_hash,
            previous_solution_hash: prev_block_header.solution_hash,
            last_epoch_hash,
            chunk_hash: poa_chunk_hash,
            previous_block_hash: prev_block_hash,
            previous_cumulative_diff: prev_block_header.cumulative_diff,
            poa,
            reward_address: self.inner().config.node_config.reward_address,
            reward_amount: block_reward.amount,
            miner_address: solution.mining_address,
            signature: Signature::test_signature().into(), // temp value until block is signed with the mining singer
            timestamp: current_timestamp,
            system_ledgers,
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: DataTransactionLedger::merklize_tx_root(
                        &mempool_bundle.publish_txs.txs,
                    )
                    .0,
                    tx_ids: H256List(
                        mempool_bundle
                            .publish_txs
                            .txs
                            .iter()
                            .map(|t| t.id)
                            .collect::<Vec<_>>(),
                    ),
                    total_chunks: publish_total_chunks,
                    expires: None,
                    proofs: opt_proofs,
                    required_proof_count: Some(
                        self.inner()
                            .config
                            .number_of_ingress_proofs_total_at(current_timestamp.to_secs())
                            .try_into()?,
                    ),
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root: DataTransactionLedger::merklize_tx_root(&mempool_bundle.submit_txs).0,
                    tx_ids: H256List(
                        mempool_bundle
                            .submit_txs
                            .iter()
                            .map(|t| t.id)
                            .collect::<Vec<_>>(),
                    ),
                    total_chunks: submit_total_chunks,
                    expires: Some(
                        self.inner()
                            .config
                            .consensus
                            .epoch
                            .submit_ledger_epoch_length,
                    ),
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            evm_block_hash,
            vdf_limiter_info: VDFLimiterInfo::new(
                solution,
                prev_block_header,
                steps,
                &self.inner().config,
            ),
            oracle_irys_price: ema_calculation.oracle_price_for_block_inclusion,
            ema_irys_price: ema_calculation.ema,
            treasury: final_treasury,
        });

        // Now that all fields are initialized, Sign the block and initialize its block_hash
        let block_signer = self.inner().config.irys_signer();
        block_signer.sign_block_header(&mut irys_block)?;

        // Build BlockTransactions from the mempool bundle
        let mut all_data_txs = Vec::new();
        all_data_txs.extend(mempool_bundle.submit_txs);
        all_data_txs.extend(mempool_bundle.publish_txs.txs);

        let block_body = BlockBody {
            block_hash: irys_block.block_hash,
            commitment_transactions: mempool_bundle.commitment_txs,
            data_transactions: all_data_txs,
        };

        let sealed_block = IrysSealedBlock::new(irys_block, block_body)?;

        Ok(Some((Arc::new(sealed_block), stats)))
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn broadcast_block(
        &self,
        block: Arc<IrysSealedBlock>,
        stats: Option<AdjustmentStats>,
        eth_built_payload: &EthBuiltPayload,
    ) -> eyre::Result<Option<Arc<IrysSealedBlock>>> {
        let mut is_difficulty_updated = false;
        let block_header = block.header();

        if let Some(stats) = stats {
            if stats.is_adjusted {
                info!(
                    "🧊 block_time: {:?} is {}% off the target block_time of {:?} and above the minimum threshold of {:?}%, adjusting difficulty. ",
                    stats.actual_block_time,
                    stats.percent_different,
                    stats.target_block_time,
                    stats.min_threshold
                );
                info!(
                    block.max_difficulty = ?U256::MAX,
                    block.previous_cumulative_diff = ?block_header.previous_cumulative_diff,
                    block.current_diff = ?block_header.diff,
                    "Difficulty data",
                );
                is_difficulty_updated = true;
            } else {
                info!(
                    "🧊 block_time: {:?} is {}% off the target block_time of {:?} and below the minimum threshold of {:?}%. No difficulty adjustment.",
                    stats.actual_block_time,
                    stats.percent_different,
                    stats.target_block_time,
                    stats.min_threshold
                );
            }
        }

        match self
            .inner()
            .block_discovery
            .handle_block(block.clone(), false)
            .await
        {
            Ok(()) => Ok(()),
            e @ Err(BlockDiscoveryError::InternalError(_)) => {
                error!(
                    "Internal Block discovery error for block {} ({}) : {:?}",
                    &block_header.block_hash, &block_header.height, e
                );
                Err(eyre!(
                    "Internal Block discovery error for block {} ({}) : {:?}",
                    &block_header.block_hash,
                    &block_header.height,
                    e
                ))
            }
            Err(e) => {
                error!(
                    "Newly produced block {:?} ({}) failed pre-validation: {:?}",
                    &block_header.block_hash.0, &block_header.height, e
                );
                Err(eyre!(
                    "Newly produced block {:?} ({}) failed pre-validation: {:?}",
                    &block_header.block_hash.0,
                    &block_header.height,
                    e
                ))
            }
        }?;

        // Gossip the EVM payload
        let execution_payload_gossip_data =
            GossipBroadcastMessageV2::from(eth_built_payload.block().clone());
        if let Err(payload_broadcast_error) = self
            .inner()
            .service_senders
            .gossip_broadcast
            .send_traced(execution_payload_gossip_data)
        {
            error!(
                block.hash = ?block.header().block_hash,
                block.height = ?block.header().height,
                payload.hash = ?eth_built_payload.block().hash(),
                "Failed to broadcast execution payload: {:?}",
                payload_broadcast_error
            );
        }

        if is_difficulty_updated {
            self.inner()
                .mining_broadcaster
                .send_difficulty(BroadcastDifficultyUpdate(Arc::clone(block.header())));
        }

        info!(
            block.height = ?block.header().height,
            block.hash = ?block.header().block_hash(),
            block.timestamp_ms = block.header().timestamp.as_millis(),
            "Finished producing block",
        );

        Ok(Some(block.clone()))
    }

    fn block_reward(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> Result<Amount<irys_types::storage_pricing::phantoms::Irys>, eyre::Error> {
        // Calculate rewards using fixed block intervals, not actual timestamps.
        // This prevents block producers from manipulating timestamps to maximize rewards
        // and ensures consistent emissions over time.

        let previous_height = prev_block_header.height();

        let target_block_time_seconds = self
            .inner()
            .config
            .consensus
            .difficulty_adjustment
            .block_time;

        // Use height * target_block_time for consistent reward curve positioning
        let previous_block_seconds = (previous_height * target_block_time_seconds) as u128;
        let current_block_seconds = previous_block_seconds + target_block_time_seconds as u128;

        let reward_amount = self
            .inner()
            .reward_curve
            .reward_between(previous_block_seconds, current_block_seconds)?;
        Ok(reward_amount)
    }

    async fn get_ema_price(
        &self,
        parent_block: &IrysBlockHeader,
        parent_block_ema_snapshot: &EmaSnapshot,
    ) -> eyre::Result<ExponentialMarketAvgCalculation> {
        let (fresh_price, oracle_updated_at) = self.inner().price_oracle.current_snapshot()?;

        let oracle_irys_price = choose_oracle_price(
            parent_block.timestamp.to_secs(),
            parent_block.oracle_irys_price,
            fresh_price,
            oracle_updated_at,
        );

        let ema_calculation = parent_block_ema_snapshot.calculate_ema_for_new_block(
            parent_block,
            oracle_irys_price,
            self.inner().config.consensus.token_price_safe_range,
            self.inner().config.consensus.ema.price_adjustment_interval,
        );

        Ok(ema_calculation)
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn get_mempool_txs(
        &self,
        prev_block_header: &IrysBlockHeader,
        block_timestamp: UnixTimestampMs,
    ) -> eyre::Result<MempoolTxsBundle> {
        // Fetch mempool once
        let mut mempool_txs = self.fetch_best_mempool_txs(prev_block_header).await?;
        // Sort txs to be of deterministic order
        mempool_txs
            .submit_tx
            .sort_by(irys_types::DataTransactionHeader::compare_tx);
        mempool_txs.commitment_tx.sort();

        let block_height = prev_block_header.height + 1;
        let is_epoch = self.is_epoch_block(block_height);

        // Fetch epoch snapshot (needed for reward address resolution)
        let (parent_epoch_snapshot, parent_commitment_snapshot) =
            self.fetch_parent_snapshots(prev_block_header)?;

        if !is_epoch {
            // Filter commitments by version using block timestamp
            self.inner()
                .config
                .consensus
                .hardforks
                .retain_valid_commitment_versions(
                    &mut mempool_txs.commitment_tx,
                    block_timestamp.to_secs(),
                );

            // Filter UpdateRewardAddress by Borealis activation
            if !self
                .inner()
                .config
                .consensus
                .hardforks
                .is_update_reward_address_allowed_for_epoch(&parent_epoch_snapshot)
            {
                mempool_txs.commitment_tx.retain(|tx| {
                    !matches!(
                        tx.commitment_type(),
                        irys_types::CommitmentTypeV2::UpdateRewardAddress { .. }
                    )
                });
            }

            debug!(
                block.height = block_height,
                custom.commitment_ids = ?mempool_txs
                    .commitment_tx
                    .iter()
                    .map(CommitmentTransaction::id)
                    .collect::<Vec<_>>(),
                "Selected best mempool txs"
            );
            return Ok(self.build_non_epoch_bundle(mempool_txs, parent_epoch_snapshot));
        }

        // =====
        // ONLY EPOCH BLOCK PROCESSING
        // =====

        let aggregated_miner_fees = self
            .calculate_expired_ledger_fees(&parent_epoch_snapshot, block_height)
            .await?;

        let commitment_refund_events = self.derive_unpledge_refunds(&parent_commitment_snapshot)?;
        let unstake_refund_events =
            crate::commitment_refunds::derive_unstake_refunds_from_snapshot(
                &parent_commitment_snapshot,
                &self.inner().config.consensus,
            )?;

        // note: we do not filter txs by version for epoch blocks
        let commitment_txs = parent_commitment_snapshot.get_epoch_commitments();

        Ok(MempoolTxsBundle {
            // on epoch blocks we don't bill the end-user
            commitment_txs,
            commitment_txs_to_bill: vec![],
            submit_txs: mempool_txs.submit_tx,
            publish_txs: mempool_txs.publish_tx,
            aggregated_miner_fees,
            commitment_refund_events,
            unstake_refund_events,
            epoch_snapshot: parent_epoch_snapshot,
        })
    }

    fn is_epoch_block(&self, height: u64) -> bool {
        height.is_multiple_of(self.inner().config.consensus.epoch.num_blocks_in_epoch)
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn fetch_best_mempool_txs(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> eyre::Result<MempoolTxs> {
        let ctx = crate::tx_selector::TxSelectionContext {
            block_tree: &self.inner().block_tree_guard,
            db: &self.inner().db,
            reth_adapter: &self.inner().reth_node_adapter,
            config: &self.inner().config,
            mempool_state: self.inner().mempool_guard.atomic_state(),
            chunk_ingress_state: &self.inner().chunk_ingress_state,
        };
        crate::tx_selector::select_best_txs(prev_block_header.block_hash, &ctx).await
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn fetch_parent_snapshots(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> eyre::Result<(Arc<EpochSnapshot>, Arc<CommitmentSnapshot>)> {
        let read = self.inner().block_tree_guard.read();
        let epoch = read
            .get_epoch_snapshot(&prev_block_header.block_hash)
            .ok_or_eyre("parent blocks epoch snapshot must be available")?;
        let commit = read
            .get_commitment_snapshot(&prev_block_header.block_hash)
            .map_err(|e| {
                eyre!(
                    "Could not find commitment snapshot for current epoch: {}",
                    e
                )
            })?;
        Ok((epoch, commit))
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn build_commitment_ledger_epoch(
        &self,
        commit_snapshot: &CommitmentSnapshot,
    ) -> SystemTransactionLedger {
        let mut txids = H256List::new();
        let commitments = commit_snapshot.get_epoch_commitments();
        for tx in commitments.iter() {
            txids.push(tx.id());
        }
        debug!(
            custom.tx_count = commitments.len(),
            "Producing epoch rollup for commitment ledger"
        );
        SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment.into(),
            tx_ids: txids,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn derive_unpledge_refunds(
        &self,
        commit_snapshot: &CommitmentSnapshot,
    ) -> eyre::Result<Vec<UnpledgeRefundEvent>> {
        crate::commitment_refunds::derive_unpledge_refunds_from_snapshot(
            commit_snapshot,
            &self.inner().config.consensus,
        )
    }

    fn build_non_epoch_bundle(
        &self,
        mempool_txs: MempoolTxs,
        epoch_snapshot: Arc<EpochSnapshot>,
    ) -> MempoolTxsBundle {
        MempoolTxsBundle {
            commitment_txs_to_bill: mempool_txs.commitment_tx.clone(),
            commitment_txs: mempool_txs.commitment_tx,
            submit_txs: mempool_txs.submit_tx,
            publish_txs: mempool_txs.publish_tx,
            aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
            commitment_refund_events: Vec::new(),
            unstake_refund_events: Vec::new(),
            epoch_snapshot,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn get_evm_block(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> eyre::Result<reth_ethereum_primitives::Block> {
        use reth::providers::BlockReader as _;

        let parent = {
            let mut attempts = 0;
            loop {
                if attempts > 50 {
                    break None;
                }
                // NOTE: using BlockReader trait will only read the block from the existing reth instance.
                // whereas, if we use the reth rpc, it will fetch the block from reth peers (not what we want)!
                let result = self
                    .inner()
                    .reth_provider
                    .block(BlockHashOrNumber::Hash(prev_block_header.evm_block_hash))?;
                match result {
                    Some(block) => {
                        info!(
                            "Got parent EVM block {} after {} attempts",
                            &prev_block_header.evm_block_hash, &attempts
                        );
                        break Some(block);
                    }
                    None => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
        .unwrap_or_else(|| {
            panic!(
                "Should be able to get the parent EVM block {} {}",
                &prev_block_header.evm_block_hash, &prev_block_header.height
            )
        });
        // TODO: fix genesis hash computation when using init-state (persist modified chainspec?)
        // for now we just skip the check for the genesis block
        if prev_block_header.height > 0 {
            let computed_parent_evm_hash = parent.header.hash_slow();
            eyre::ensure!(
                computed_parent_evm_hash == prev_block_header.evm_block_hash,
                "reth parent block hash mismatch for height {} - computed {} got {}",
                &prev_block_header.height,
                &computed_parent_evm_hash,
                &prev_block_header.evm_block_hash
            );
        }
        Ok(parent)
    }

    /// Calculates the aggregated fees owed to miners when data ledgers expire.
    ///
    /// Delegates to the dedicated ledger_expiry module for processing.
    /// Currently processes Submit ledger as that's the only expiring ledger type.
    async fn calculate_expired_ledger_fees(
        &self,
        parent_epoch_snapshot: &EpochSnapshot,
        block_height: u64,
    ) -> eyre::Result<LedgerExpiryBalanceDelta> {
        ledger_expiry::calculate_expired_ledger_fees(
            parent_epoch_snapshot,
            block_height,
            DataLedger::Submit, // Currently only Submit ledgers expire
            &self.inner().config,
            self.inner().block_index.clone(),
            &self.inner().block_tree_guard,
            &self.inner().mempool_guard,
            &self.inner().db,
            true, // we expect the txs to be promoted otherwise return perm fee
        )
        .in_current_span()
        .await
    }
}

/// Chooses between parent block price and fresh oracle price for EMA calculation.
///
/// Oracle prices are preferred if they were updated within the last 3 minutes,
/// accounting for typical reporting delays from oracle providers.
#[inline]
#[tracing::instrument(level = "trace", skip_all)]
fn choose_oracle_price(
    parent_ts: UnixTimestamp,
    parent_price: IrysTokenPrice,
    oracle_price: IrysTokenPrice,
    oracle_updated_at: UnixTimestamp,
) -> IrysTokenPrice {
    // Allow 3 minutes grace period for oracle prices to account for reporting delays
    const ORACLE_MAX_AGE_SECS: u64 = 3 * 60;

    // Prefer oracle if it was updated within the grace period
    let oracle_is_fresh = parent_ts.as_secs() <= oracle_updated_at.as_secs() + ORACLE_MAX_AGE_SECS;

    let (chosen, source) = if oracle_is_fresh {
        (oracle_price, "oracle_fresh")
    } else {
        (parent_price, "parent_fallback")
    };

    tracing::debug!(
        parent_secs = parent_ts.as_secs(),
        oracle_secs = oracle_updated_at.as_secs(),
        parent_price = %parent_price,
        oracle_price = %oracle_price,
        chosen_price = %chosen,
        source,
        "selected oracle price for EMA calculation"
    );

    chosen
}

pub struct ProductionStrategy {
    pub inner: Arc<BlockProducerInner>,
}

impl BlockProdStrategy for ProductionStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.inner
    }
}

#[tracing::instrument(level = "trace", skip_all)]
pub async fn current_timestamp(prev_block_header: &IrysBlockHeader) -> UnixTimestampMs {
    let now_ms = UnixTimestampMs::now().unwrap();
    let prev_secs = prev_block_header.timestamp.to_secs();

    // If we're in the same second as the previous block, wait until the next second.
    // This prevents EVM timestamp collisions (EVM uses second-precision).
    // Dev configs can easily trigger this behaviour due to fast block times.
    if now_ms.to_secs() == prev_secs {
        let ms_into_sec = (now_ms.as_millis() % 1000) as u64;
        let wait_duration = Duration::from_millis(1000 - ms_into_sec);
        info!(
            "Waiting {:.2?} to prevent timestamp overlap",
            &wait_duration
        );
        tokio::time::sleep(wait_duration).await;
        return UnixTimestampMs::now().unwrap();
    }

    now_ms
}

/// Calculates the total number of full chunks needed to store a list of transactions,
/// taking into account padding for partial chunks. Each transaction's data is padded
/// to the next full chunk boundary if it doesn't align perfectly with the chunk size.
///
/// # Arguments
/// * `txs` - Vector of transaction headers containing data size information
/// * `chunk_size` - Size of each chunk in bytes
///
/// # Returns
/// Total number of chunks needed, including padding for partial chunks
pub fn calculate_chunks_added(txs: &[DataTransactionHeader], chunk_size: u64) -> u64 {
    let bytes_added = txs.iter().fold(0, |acc, tx| {
        acc + tx.data_size.div_ceil(chunk_size) * chunk_size
    });

    bytes_added / chunk_size
}

#[cfg(test)]
mod oracle_choice_tests {
    use super::choose_oracle_price;
    use irys_types::{UnixTimestamp, storage_pricing::Amount};
    use rust_decimal_macros::dec;

    const MAX_AGE_SECS: u64 = 3 * 60; // 3 minutes

    #[test]
    fn chooses_parent_when_oracle_is_too_stale() {
        let parent_price = Amount::token(dec!(2.0)).unwrap();
        let oracle_price = Amount::token(dec!(1.0)).unwrap();

        // Oracle is more than 3 minutes behind parent - use parent
        let chosen = choose_oracle_price(
            UnixTimestamp::from_secs(1000),
            parent_price,
            oracle_price,
            UnixTimestamp::from_secs(1000 - MAX_AGE_SECS - 1), // 181 seconds behind
        );
        assert_eq!(
            chosen, parent_price,
            "should choose parent price when oracle is more than 3 minutes stale"
        );
    }

    #[test]
    fn chooses_oracle_when_within_tolerance() {
        let parent_price = Amount::token(dec!(2.0)).unwrap();
        let oracle_price = Amount::token(dec!(1.0)).unwrap();

        // Oracle is exactly at the 3-minute tolerance boundary - still use oracle
        let chosen = choose_oracle_price(
            UnixTimestamp::from_secs(1000),
            parent_price,
            oracle_price,
            UnixTimestamp::from_secs(1000 - MAX_AGE_SECS), // exactly 180 seconds behind
        );
        assert_eq!(
            chosen, oracle_price,
            "should choose oracle price when exactly at 3-minute tolerance"
        );
    }

    #[test]
    fn chooses_oracle_when_oracle_is_fresher() {
        let parent_price = Amount::token(dec!(2.0)).unwrap();
        let oracle_price = Amount::token(dec!(1.0)).unwrap();

        // Oracle is fresher than parent - definitely use oracle
        let chosen = choose_oracle_price(
            UnixTimestamp::from_secs(100),
            parent_price,
            oracle_price,
            UnixTimestamp::from_secs(200),
        );
        assert_eq!(
            chosen, oracle_price,
            "should choose oracle price when oracle is fresher"
        );
    }

    #[test]
    fn chooses_oracle_when_timestamps_are_equal() {
        let parent_price = Amount::token(dec!(2.0)).unwrap();
        let oracle_price = Amount::token(dec!(1.0)).unwrap();

        // Both at same second - prefer oracle (authoritative source)
        let chosen = choose_oracle_price(
            UnixTimestamp::from_secs(500),
            parent_price,
            oracle_price,
            UnixTimestamp::from_secs(500),
        );
        assert_eq!(
            chosen, oracle_price,
            "should choose oracle price when timestamps are equal"
        );
    }
}
