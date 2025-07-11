use crate::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    block_tree_service::{
        ema_snapshot::{EmaSnapshot, ExponentialMarketAvgCalculation},
        BlockTreeReadGuard,
    },
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    mempool_service::MempoolServiceMessage,
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    shadow_tx_generator::ShadowTxGenerator,
};
use actix::prelude::{Addr, Message};
use alloy_consensus::{
    transaction::SignerRecoverable as _, EthereumTxEnvelope, SignableTransaction as _, TxEip4844,
};
use alloy_eips::BlockHashOrNumber;
use alloy_network::TxSignerSync as _;
use alloy_rpc_types_engine::{
    ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, PayloadAttributes, PayloadStatusEnum,
};
use alloy_signer_local::LocalSigner;
use base58::ToBase58 as _;
use eyre::{eyre, Context as _};
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _, SystemLedger};
use irys_price_oracle::IrysPriceOracle;
use irys_reth::{
    compose_shadow_tx,
    payload::{DeterministicShadowTxKey, ShadowTxStore},
    reth_node_ethereum::EthEngineTypes,
    IrysEthereumNode,
};
use irys_reth_node_bridge::adapter::NodeProvider;
use irys_reward_curve::HalvingCurve;
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, calculate_difficulty,
    next_cumulative_diff, storage_pricing::Amount, Base64, CommitmentTransaction, Config,
    DataLedger, DataTransactionLedger, GossipBroadcastMessage, H256List, IngressProofsList,
    IrysBlockHeader, IrysTransactionHeader, PoaData, Signature, SystemTransactionLedger,
    TxIngressProof, VDFLimiterInfo, H256, U256,
};
use irys_vdf::state::VdfStateReadonly;
use nodit::interval::ii;
use openssl::sha;
use reth::{
    api::{BeaconConsensusEngineHandle, NodeTypes, PayloadKind},
    core::primitives::SealedBlock,
    payload::{EthBuiltPayload, EthPayloadBuilderAttributes, PayloadBuilderHandle},
    revm::primitives::B256,
    rpc::types::BlockId,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use reth_transaction_pool::EthPooledTransaction;
use std::{
    pin::pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

mod block_validation_tracker;
pub use block_validation_tracker::BlockValidationTracker;

/// Commands that can be sent to the block producer service
#[derive(Debug)]
pub enum BlockProducerCommand {
    /// Announce to the node a mining solution has been found
    SolutionFound {
        solution: SolutionContext,
        response: oneshot::Sender<
            eyre::Result<Option<(Arc<irys_types::IrysBlockHeader>, EthBuiltPayload)>>,
        >,
    },
    /// Set the test blocks remaining (for testing)
    SetTestBlocksRemaining(Option<u64>),
}

/// Block producer service that creates blocks from mining solutions
#[derive(Debug)]
pub struct BlockProducerService {
    /// Graceful shutdown handle
    shutdown: GracefulShutdown,
    /// Command receiver
    cmd_rx: mpsc::UnboundedReceiver<BlockProducerCommand>,
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
    pub block_discovery_addr: Addr<BlockDiscoveryActor>,
    /// Mining broadcast service
    pub mining_broadcaster: Addr<BroadcastMiningService>,
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
    pub reth_payload_builder: PayloadBuilderHandle<EthEngineTypes>,
    /// Reth blockchain provider
    pub reth_provider: NodeProvider,
    /// Shadow tx store
    pub shadow_tx_store: ShadowTxStore,
    /// Reth service actor
    pub reth_service: Addr<RethServiceActor>,
    /// Reth beacon engine handle
    pub beacon_engine_handle: BeaconConsensusEngineHandle<<IrysEthereumNode as NodeTypes>::Payload>,
}

impl BlockProducerService {
    /// Spawn a new block producer service
    #[tracing::instrument(skip_all, fields(blocks_remaining_for_test = ?blocks_remaining_for_test))]
    pub fn spawn_service(
        exec: &TaskExecutor,
        inner: Arc<BlockProducerInner>,
        blocks_remaining_for_test: Option<u64>,
        rx: mpsc::UnboundedReceiver<BlockProducerCommand>,
    ) -> tokio::task::JoinHandle<()> {
        info!(
            "Spawning block producer service with blocks_remaining_for_test: {:?}",
            blocks_remaining_for_test
        );

        exec.spawn_critical_with_graceful_shutdown_signal(
            "BlockProducer Service",
            move |shutdown| async move {
                let service = Self {
                    shutdown,
                    cmd_rx: rx,
                    inner,
                    blocks_remaining_for_test,
                };
                service
                    .start()
                    .await
                    .expect("Block producer service encountered an irrecoverable error")
            },
        )
    }

    #[tracing::instrument(skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting block producer service");
        debug!(
            "Service configuration - reward_address: {}, chain_id: {}",
            self.inner.config.node_config.reward_address, self.inner.config.consensus.chain_id
        );
        loop {
            tokio::select! {
                // Handle commands
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            let res = self.handle_command(cmd).await;
                            if let Err(err) = res {
                                // because we don't have our shutdown system fully finished,
                                // block producer may error out while other services are in the middle of shutting down.
                                let is_shutdown = futures::future::poll_immediate(&mut self.shutdown).await.is_some();
                                if is_shutdown {
                                    // graceful shutdown
                                    break;
                                }
                                // legit error - propagate
                                return Err(err)
                            }
                        }
                        None => {
                            warn!("Command channel closed unexpectedly");
                            break;
                        }
                    }
                }
                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for block producer service");
                    break;
                }
            }
        }

        info!("Shutting down block producer service gracefully");
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn handle_command(&mut self, cmd: BlockProducerCommand) -> eyre::Result<()> {
        match cmd {
            BlockProducerCommand::SolutionFound { solution, response } => {
                let solution_hash = solution.solution_hash.0.to_base58();
                info!(
                    solution_hash = %solution_hash,
                    vdf_step = solution.vdf_step,
                    mining_address = %solution.mining_address,
                    "Block producer received mining solution"
                );

                if let Some(blocks_remaining) = self.blocks_remaining_for_test {
                    if blocks_remaining == 0 {
                        info!(
                            solution_hash = %solution_hash,
                            "No more blocks needed for test, skipping block production"
                        );
                        let _ = response.send(Ok(None));
                        return Ok(());
                    }
                    debug!("Test blocks remaining: {}", blocks_remaining);
                }

                let inner = self.inner.clone();
                let result = Self::produce_block_inner(inner, solution).await?;

                // Only decrement blocks_remaining_for_test when a block is successfully produced
                if let Some((irys_block_header, _eth_built_payload)) = &result {
                    info!(
                        block_hash = %irys_block_header.block_hash.0.to_base58(),
                        block_height = irys_block_header.height,
                        "Block production completed successfully"
                    );

                    if let Some(remaining) = self.blocks_remaining_for_test.as_mut() {
                        *remaining = remaining.saturating_sub(1);
                        debug!("Test blocks remaining after production: {}", *remaining);
                    }
                } else {
                    info!("Block production skipped (solution outdated or invalid)");
                }

                let _ = response.send(Ok(result));
            }
            BlockProducerCommand::SetTestBlocksRemaining(remaining) => {
                debug!(
                    old_value = ?self.blocks_remaining_for_test,
                    new_value = ?remaining,
                    "Updating test blocks remaining"
                );
                self.blocks_remaining_for_test = remaining;
            }
        }
        Ok(())
    }

    /// Internal method to produce a block without the non-Send trait
    #[tracing::instrument(skip_all, fields(
        solution_hash = %solution.solution_hash.0.to_base58(),
        vdf_step = solution.vdf_step,
        mining_address = %solution.mining_address
    ))]
    async fn produce_block_inner(
        inner: Arc<BlockProducerInner>,
        solution: SolutionContext,
    ) -> eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>> {
        info!(
            partition_hash = %solution.partition_hash.0.to_base58(),
            chunk_offset = solution.chunk_offset,
            "Starting block production for solution"
        );

        let production_strategy = ProductionStrategy { inner };
        production_strategy.fully_produce_new_block(solution).await
    }
}

#[async_trait::async_trait]
pub trait BlockProdStrategy: Send + Sync {
    fn inner(&self) -> &BlockProducerInner;

    /// Creates PoA data from the solution context
    /// Returns (PoaData, chunk_hash)
    #[tracing::instrument(skip_all, fields(
        partition_hash = %solution.partition_hash.0.to_base58(),
        ledger_id = ?ledger_id
    ))]
    fn create_poa_data(
        &self,
        solution: &SolutionContext,
        ledger_id: Option<u32>,
    ) -> eyre::Result<(PoaData, H256)> {
        debug!(
            chunk_size = solution.chunk.len(),
            recall_chunk_index = solution.recall_chunk_index,
            chunk_offset = solution.chunk_offset,
            "Creating PoA data from solution"
        );

        let poa_chunk = Base64(solution.chunk.clone());
        let poa_chunk_hash = H256(sha::sha256(&poa_chunk.0));

        debug!(
            chunk_hash = %poa_chunk_hash.0.to_base58(),
            has_tx_path = solution.tx_path.is_some(),
            has_data_path = solution.data_path.is_some(),
            "PoA chunk hash calculated"
        );

        let poa = PoaData {
            tx_path: solution.tx_path.clone().map(Base64),
            data_path: solution.data_path.clone().map(Base64),
            chunk: Some(poa_chunk),
            recall_chunk_index: solution.recall_chunk_index,
            ledger_id,
            partition_chunk_offset: solution.chunk_offset,
            partition_hash: solution.partition_hash,
        };
        Ok((poa, poa_chunk_hash))
    }

    /// Fetches a block header from mempool or database
    #[tracing::instrument(skip(self), fields(block_hash = %block_hash.0.to_base58()))]
    async fn fetch_block_header(&self, block_hash: H256) -> eyre::Result<IrysBlockHeader> {
        debug!("Fetching block header");

        // Try mempool first
        let (tx, rx) = oneshot::channel();
        self.inner()
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetBlockHeader(block_hash, false, tx))
            .map_err(|e| eyre!("Failed to send GetBlockHeader message to mempool: {}", e))?;

        match rx
            .await
            .map_err(|e| eyre!("Failed to receive response from mempool: {}", e))?
        {
            Some(header) => {
                debug!(
                    block_height = header.height,
                    "Block header found in mempool"
                );
                Ok(header)
            }
            None => {
                debug!("Block header not in mempool, checking database");
                // Fall back to database
                self.inner()
                    .db
                    .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
                    .ok_or_else(|| {
                        error!(
                            "No block header found for hash {}",
                            block_hash.0.to_base58()
                        );
                        eyre!(
                            "No block header found for hash {}",
                            block_hash.0.to_base58()
                        )
                    })
                    .inspect(|header| {
                        debug!(
                            block_height = header.height,
                            "Block header found in database"
                        );
                    })
            }
        }
    }

    /// Gets the EMA snapshot for a given block
    #[tracing::instrument(skip(self), fields(block_hash = %block_hash.0.to_base58()))]
    fn get_block_ema_snapshot(&self, block_hash: &H256) -> eyre::Result<Arc<EmaSnapshot>> {
        debug!("Fetching EMA snapshot");

        let read = self.inner().block_tree_guard.read();
        read.get_ema_snapshot(block_hash)
            .ok_or_else(|| {
                error!("EMA snapshot not found for block");
                eyre!(
                    "EMA snapshot not found for block {}",
                    block_hash.0.to_base58()
                )
            })
            .inspect(|snapshot| {
                debug!(
                    ema_price_current = %snapshot.ema_price_current_interval,
                    oracle_price_predecessor = %snapshot.oracle_price_for_current_ema_predecessor,
                    "EMA snapshot retrieved"
                );
            })
    }

    /// Selects the parent block for new block production.
    ///
    /// Targets the block with highest cumulative difficulty, but only if fully validated.
    /// If validation is pending, waits up to 10 seconds for completion. Falls back to
    /// the latest validated block on timeout to ensure production continues.
    ///
    /// Returns the selected parent block header and its EMA snapshot.
    #[tracing::instrument(skip(self), level = "debug")]
    async fn parent_irys_block(&self) -> eyre::Result<(IrysBlockHeader, Arc<EmaSnapshot>)> {
        const MAX_WAIT_TIME: Duration = Duration::from_secs(10);

        debug!(
            "Starting parent block selection with max wait time: {:?}",
            MAX_WAIT_TIME
        );

        // Get current chain head for context
        let current_head = {
            let tree = self.inner().block_tree_guard.read();
            let (max_diff, max_hash) = tree.get_max_cumulative_difficulty_block();
            let height = tree.get_block(&max_hash).map(|b| b.height).unwrap_or(0);
            (max_hash, height, max_diff)
        };

        debug!(
            current_head_hash = %current_head.0.0.to_base58(),
            current_head_height = current_head.1,
            cumulative_difficulty = %current_head.2,
            "Current chain head state"
        );

        // Use BlockValidationTracker to select the parent block
        let parent_block_hash = BlockValidationTracker::new(self.inner(), MAX_WAIT_TIME)
            .wait_for_validation()
            .await
            .context("Failed to select validated parent block")?;

        if parent_block_hash != current_head.0 {
            info!(
                selected_parent = %parent_block_hash.0.to_base58(),
                max_diff_block = %current_head.0.0.to_base58(),
                "Parent block differs from max difficulty block (waiting for validation)"
            );
        } else {
            debug!(
                parent_hash = %parent_block_hash.0.to_base58(),
                "Parent block selected (already validated)"
            );
        }

        // Fetch the parent block header
        let header = self
            .fetch_block_header(parent_block_hash)
            .await
            .context("Failed to fetch parent block header")?;

        // Get the EMA snapshot
        let ema_snapshot = self
            .get_block_ema_snapshot(&header.block_hash)
            .context("Failed to get EMA snapshot")?;

        debug!(
            parent_hash = %header.block_hash.0.to_base58(),
            parent_height = header.height,
            parent_difficulty = %header.diff,
            ema_price = %ema_snapshot.ema_price_current_interval,
            "Parent block selection completed"
        );

        Ok((header, ema_snapshot))
    }

    #[tracing::instrument(skip_all, fields(
        solution_hash = %solution.solution_hash.0.to_base58(),
        vdf_step = solution.vdf_step
    ))]
    async fn fully_produce_new_block(
        &self,
        solution: SolutionContext,
    ) -> eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>> {
        // Step 1: Select parent block
        let (prev_block_header, prev_block_ema_snapshot) = self
            .parent_irys_block()
            .await
            .context("Failed to select parent block")?;
        info!(
            parent_hash = %prev_block_header.block_hash.0.to_base58(),
            parent_height = prev_block_header.height,
            "Parent block selected"
        );

        // Step 2: Get parent EVM block
        let prev_evm_block = self
            .get_evm_block(&prev_block_header)
            .await
            .context("Failed to fetch parent EVM block")?;
        debug!(
            evm_block_hash = %prev_block_header.evm_block_hash,
            "Parent EVM block fetched"
        );

        let current_timestamp = current_timestamp(&prev_block_header).await;
        let new_block_height = prev_block_header.height + 1;
        debug!(
            "New block height: {}, timestamp: {}",
            new_block_height, current_timestamp
        );

        // Step 3: Fetch mempool transactions
        let (system_tx_ledger, commitment_txs_to_bill, submit_txs, publish_txs) = self
            .get_mempool_txs(&prev_block_header)
            .await
            .context("Failed to fetch mempool transactions")?;
        info!(
            system_tx_count = system_tx_ledger.len(),
            commitment_tx_count = commitment_txs_to_bill.len(),
            submit_tx_count = submit_txs.len(),
            publish_tx_count = publish_txs.0.len(),
            "Mempool transactions fetched"
        );

        // Step 4: Calculate block reward
        let block_reward = self
            .block_reward(&prev_block_header, current_timestamp)
            .context("Failed to calculate block reward")?;
        debug!("Block reward calculated: {}", block_reward.amount);

        // Step 5: Create EVM block
        let eth_built_payload = self
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &commitment_txs_to_bill,
                &submit_txs,
                block_reward,
                current_timestamp,
            )
            .await
            .context("Failed to create EVM block")?;
        let evm_block = eth_built_payload.block();
        info!(
            evm_block_hash = %evm_block.hash(),
            "EVM block created"
        );

        // Step 6: Produce final block
        let block = self
            .produce_block(
                solution,
                &prev_block_header,
                submit_txs,
                publish_txs,
                system_tx_ledger,
                current_timestamp,
                block_reward,
                evm_block,
                &prev_block_ema_snapshot,
            )
            .await
            .context("Failed to produce final block")?;

        let Some(block) = block else {
            info!("Block production skipped (outdated solution)");
            return Ok(None);
        };

        info!(
            block_hash = %block.block_hash.0.to_base58(),
            block_height = block.height,
            "Block production completed successfully"
        );

        Ok(Some((block, eth_built_payload)))
    }

    /// Extracts and collects all transactions that should be included in a block
    #[tracing::instrument(skip_all, fields(
        block_height = prev_block_header.height + 1,
        timestamp_ms = timestamp_ms
    ))]
    async fn create_evm_block(
        &self,
        prev_block_header: &IrysBlockHeader,
        perv_evm_block: &reth_ethereum_primitives::Block,
        commitment_txs_to_bill: &[CommitmentTransaction],
        submit_txs: &[IrysTransactionHeader],
        reward_amount: Amount<irys_types::storage_pricing::phantoms::Irys>,
        timestamp_ms: u128,
    ) -> eyre::Result<EthBuiltPayload> {
        let block_height = prev_block_header.height + 1;

        debug!(
            reward_address = %self.inner().config.node_config.reward_address,
            reward_amount = %reward_amount.amount,
            commitment_tx_count = commitment_txs_to_bill.len(),
            submit_tx_count = submit_txs.len(),
            "Starting EVM block creation"
        );

        let local_signer = LocalSigner::from(self.inner().config.irys_signer().signer);

        // Generate expected shadow transactions using shared logic
        debug!("Generating shadow transactions");

        let shadow_txs = ShadowTxGenerator::new(
            &block_height,
            &self.inner().config.node_config.reward_address,
            &reward_amount.amount,
            prev_block_header,
        );

        let shadow_txs = shadow_txs
            .generate_all(commitment_txs_to_bill, submit_txs)
            .enumerate()
            .map(|(index, tx_result)| {
                let tx =
                    tx_result.map_err(|e| eyre!("Failed to generate shadow transaction: {}", e))?;
                debug!(
                    tx_index = index,
                    tx_type = ?std::mem::discriminant(&tx),
                    "Processing shadow transaction"
                );

                let mut tx_raw = compose_shadow_tx(self.inner().config.consensus.chain_id, &tx);
                let signature = local_signer
                    .sign_transaction_sync(&mut tx_raw)
                    .map_err(|e| eyre!("Failed to sign shadow transaction: {}", e))?;
                let tx = EthereumTxEnvelope::<TxEip4844>::Legacy(tx_raw.into_signed(signature))
                    .try_into_recovered()
                    .map_err(|e| eyre!("Failed to recover shadow transaction: {}", e))?;

                Ok::<EthPooledTransaction, eyre::Report>(EthPooledTransaction::new(tx, 300))
            })
            .collect::<Result<Vec<_>, _>>()?;

        info!(
            shadow_tx_count = shadow_txs.len(),
            "Shadow transactions generated"
        );

        self.build_and_submit_reth_payload(
            prev_block_header,
            timestamp_ms,
            shadow_txs,
            perv_evm_block.header.mix_hash,
        )
        .await
        .context("Failed to build and submit Reth payload")
    }

    /// Builds and submits a Reth payload with forkchoice update
    #[tracing::instrument(skip_all, fields(
        parent_evm_hash = %prev_block_header.evm_block_hash,
        timestamp_sec = timestamp_ms / 1000,
        shadow_tx_count = shadow_txs.len()
    ))]
    async fn build_and_submit_reth_payload(
        &self,
        prev_block_header: &IrysBlockHeader,
        timestamp_ms: u128,
        shadow_txs: Vec<EthPooledTransaction>,
        parent_mix_hash: B256,
    ) -> eyre::Result<EthBuiltPayload> {
        debug!("Building Reth payload attributes");

        // generate payload attributes
        let attributes = PayloadAttributes {
            timestamp: (timestamp_ms / 1000) as u64, // **THIS HAS TO BE SECONDS**
            prev_randao: parent_mix_hash,
            suggested_fee_recipient: self.inner().config.node_config.reward_address,
            withdrawals: None, // these should ALWAYS be none
            parent_beacon_block_root: Some(prev_block_header.block_hash.into()),
        };

        debug!(
            timestamp_sec = attributes.timestamp,
            fee_recipient = %attributes.suggested_fee_recipient,
            parent_beacon_root = ?attributes.parent_beacon_block_root,
            "Payload attributes created"
        );

        let attributes =
            EthPayloadBuilderAttributes::new(prev_block_header.evm_block_hash, attributes);

        let payload_builder = &self.inner().reth_payload_builder;
        let beacon_engine_handle = &self.inner().beacon_engine_handle;

        // store shadow txs
        let key = DeterministicShadowTxKey::new(attributes.payload_id());
        debug!(
            payload_id = %attributes.payload_id(),
            "Storing shadow transactions"
        );
        self.inner().shadow_tx_store.set_shadow_txs(key, shadow_txs);

        // send & await the payload
        info!("Sending new payload to Reth");

        let payload_id = payload_builder
            .send_new_payload(attributes.clone())
            .await
            .map_err(|e| eyre!("Failed to send payload to builder: {}", e))?
            .map_err(|e| eyre!("Payload builder returned error: {}", e))?;

        debug!(
            payload_id = %payload_id,
            "Payload accepted by builder"
        );

        let built_payload = payload_builder
            .resolve_kind(payload_id, PayloadKind::WaitForPending)
            .await
            .ok_or_else(|| {
                eyre!("Failed to resolve payload future - payload builder returned None")
            })?
            .map_err(|e| eyre!("Failed to build payload: {}", e))?;

        let sidecar = ExecutionPayloadSidecar::from_block(&built_payload.block().clone().unseal());
        let payload = built_payload.clone().try_into_v5().unwrap();
        let new_payload_result = beacon_engine_handle
            .new_payload(ExecutionData {
                payload: ExecutionPayload::V3(payload.execution_payload),
                sidecar,
            })
            .await?;

        eyre::ensure!(
            new_payload_result.status == PayloadStatusEnum::Valid,
            "Reth has gone out of sync {:?}",
            new_payload_result.status
        );

        info!(
            payload_block_hash = %built_payload.block().hash(),
            payload_tx_count = built_payload.block().body().transactions.len(),
            "Reth payload built successfully"
        );

        Ok(built_payload)
    }

    #[tracing::instrument(skip_all, fields(
        solution_hash = %solution.solution_hash.0.to_base58(),
        block_height = prev_block_header.height + 1,
        vdf_step = solution.vdf_step
    ))]
    async fn produce_block(
        &self,
        solution: SolutionContext,
        prev_block_header: &IrysBlockHeader,
        submit_txs: Vec<IrysTransactionHeader>,
        publish_txs: (Vec<IrysTransactionHeader>, Vec<TxIngressProof>),
        system_transaction_ledger: Vec<SystemTransactionLedger>,
        current_timestamp: u128,
        block_reward: Amount<irys_types::storage_pricing::phantoms::Irys>,
        eth_built_payload: &SealedBlock<reth_ethereum_primitives::Block>,
        perv_block_ema_snapshot: &EmaSnapshot,
    ) -> eyre::Result<Option<Arc<IrysBlockHeader>>> {
        let prev_block_hash = prev_block_header.block_hash;
        let block_height = prev_block_header.height + 1;
        let evm_block_hash = eth_built_payload.hash();

        debug!(
            prev_block_hash = %prev_block_hash.0.to_base58(),
            evm_block_hash = %evm_block_hash,
            submit_tx_count = submit_txs.len(),
            publish_tx_count = publish_txs.0.len(),
            "Starting final block production"
        );

        // Check VDF step validity
        if solution.vdf_step <= prev_block_header.vdf_limiter_info.global_step_number {
            warn!(
                solution_vdf_step = solution.vdf_step,
                prev_block_vdf_step = prev_block_header.vdf_limiter_info.global_step_number,
                prev_block_hash = %prev_block_hash.0.to_base58(),
                "Skipping block production - solution VDF step is outdated"
            );
            return Ok(None);
        }

        debug!(
            solution_vdf_step = solution.vdf_step,
            prev_block_vdf_step = prev_block_header.vdf_limiter_info.global_step_number,
            "VDF step validation passed"
        );

        let (publish_txs, proofs) = publish_txs;

        // Publish Ledger Transactions
        let publish_chunks_added =
            calculate_chunks_added(&publish_txs, self.inner().config.consensus.chunk_size);
        let publish_max_chunk_offset = prev_block_header.data_ledgers[DataLedger::Publish]
            .max_chunk_offset
            + publish_chunks_added;
        let opt_proofs = (!proofs.is_empty()).then(|| IngressProofsList::from(proofs));

        // Difficulty adjustment logic
        debug!("Calculating difficulty adjustment");
        let mut last_diff_timestamp = prev_block_header.last_diff_timestamp;
        let current_difficulty = prev_block_header.diff;
        let mut is_difficulty_updated = false;
        let (diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_difficulty,
            &self.inner().config.consensus.difficulty_adjustment,
        );

        // Did an adjustment happen?
        if let Some(stats) = stats {
            if stats.is_adjusted {
                info!(
                    actual_block_time = ?stats.actual_block_time,
                    percent_different = stats.percent_different,
                    target_block_time = ?stats.target_block_time,
                    min_threshold = stats.min_threshold,
                    "🧊 Difficulty adjustment triggered - block time {}% off target",
                    stats.percent_different
                );
                info!(
                    max_difficulty = %U256::MAX,
                    current_difficulty = %current_difficulty,
                    new_difficulty = %diff,
                    "Difficulty values"
                );
                is_difficulty_updated = true;
            } else {
                debug!(
                    actual_block_time = ?stats.actual_block_time,
                    percent_different = stats.percent_different,
                    target_block_time = ?stats.target_block_time,
                    min_threshold = stats.min_threshold,
                    "🧊 No difficulty adjustment - block time within threshold"
                );
            }
            last_diff_timestamp = current_timestamp;
        }

        let cumulative_difficulty = next_cumulative_diff(prev_block_header.cumulative_diff, diff);

        // Use the partition hash to figure out what ledger it belongs to
        debug!("Retrieving epoch snapshot for partition assignment");
        let epoch_snapshot = self
            .inner()
            .block_tree_guard
            .read()
            .get_epoch_snapshot(&prev_block_hash)
            .ok_or_else(|| {
                eyre!(
                    "Failed to retrieve parent epoch snapshot for block {}",
                    prev_block_hash.0.to_base58()
                )
            })?;

        let ledger_id = epoch_snapshot
            .get_data_partition_assignment(solution.partition_hash)
            .and_then(|pa| pa.ledger_id);

        debug!(
            partition_hash = %solution.partition_hash.0.to_base58(),
            ledger_id = ?ledger_id,
            "Partition assignment determined"
        );

        // Create PoA data using the trait method
        let (poa, poa_chunk_hash) = self
            .create_poa_data(&solution, ledger_id)
            .context("Failed to create PoA data")?;
        debug!(
            chunk_hash = %poa_chunk_hash.0.to_base58(),
            "PoA data created"
        );

        // Collect VDF steps
        debug!("Collecting VDF steps for block");
        let mut steps = if prev_block_header.vdf_limiter_info.global_step_number + 1
            > solution.vdf_step - 1
        {
            debug!("No intermediate VDF steps needed");
            H256List::new()
        } else {
            let step_range_start = prev_block_header.vdf_limiter_info.global_step_number + 1;
            let step_range_end = solution.vdf_step - 1;
            debug!(
                step_range_start = step_range_start,
                step_range_end = step_range_end,
                "Fetching VDF step range"
            );
            self.inner().vdf_steps_guard.get_steps(ii(step_range_start, step_range_end))
                .map_err(|e| {
                    error!(
                        vdf_step = solution.vdf_step,
                        block_height = block_height,
                        error = ?e,
                        "VDF step range unavailable"
                    );
                    eyre!("VDF step range {} unavailable while producing block {}, reason: {:?}, aborting", solution.vdf_step, &block_height, e)
                })?
        };
        steps.push(solution.seed.0);
        debug!(vdf_steps_count = steps.len(), "VDF steps collected");

        let ema_calculation = self
            .get_ema_price(prev_block_header, perv_block_ema_snapshot)
            .await
            .context("Failed to calculate EMA price")?;

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
        let submit_chunks_added =
            calculate_chunks_added(&submit_txs, self.inner().config.consensus.chunk_size);
        let submit_max_chunk_offset = prev_block_header.data_ledgers[DataLedger::Submit]
            .max_chunk_offset
            + submit_chunks_added;

        // build a new block header
        let mut irys_block = IrysBlockHeader {
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
            system_ledgers: system_transaction_ledger,
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: DataTransactionLedger::merklize_tx_root(&publish_txs).0,
                    tx_ids: H256List(publish_txs.iter().map(|t| t.id).collect::<Vec<_>>()),
                    max_chunk_offset: publish_max_chunk_offset,
                    expires: None,
                    proofs: opt_proofs,
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root: DataTransactionLedger::merklize_tx_root(&submit_txs).0,
                    tx_ids: H256List(submit_txs.iter().map(|t| t.id).collect::<Vec<_>>()),
                    max_chunk_offset: submit_max_chunk_offset,
                    expires: Some(1622543200), // todo this should be updated `submit_ledger_epoch_length` from the config
                    proofs: None,
                },
            ],
            evm_block_hash,
            vdf_limiter_info: VDFLimiterInfo {
                global_step_number: solution.vdf_step,
                output: solution.seed.into_inner(),
                last_step_checkpoints: solution.checkpoints,
                prev_output: prev_block_header.vdf_limiter_info.output,
                seed: prev_block_header.vdf_limiter_info.seed,
                steps,
                ..Default::default()
            },
            oracle_irys_price: ema_calculation.oracle_price_for_block_inclusion,
            ema_irys_price: ema_calculation.ema,
        };

        // Now that all fields are initialized, Sign the block and initialize its block_hash
        let block_signer = self.inner().config.irys_signer();
        block_signer
            .sign_block_header(&mut irys_block)
            .context("Failed to sign block header")?;
        debug!(
            block_hash = %irys_block.block_hash.0.to_base58(),
            "Block header signed"
        );

        // Send fork choice update to Reth
        debug!("Sending fork choice update to Reth");
        let _res = self
            .inner()
            .reth_service
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Evm(irys_block.evm_block_hash),
                confirmed_hash: None,
                finalized_hash: None,
            })
            .await
            .map_err(|e| eyre!("Failed to send fork choice update: {}", e))?
            .map_err(|e| eyre!("Fork choice update failed: {}", e))?;

        let block = Arc::new(irys_block);

        // Send block discovery message
        info!(
            block_hash = %block.block_hash.0.to_base58(),
            block_height = block.height,
            "Sending block to discovery service for validation"
        );

        match self
            .inner()
            .block_discovery_addr
            .send(BlockDiscoveredMessage(block.clone()))
            .await
        {
            Ok(Ok(())) => {
                debug!("Block discovery message sent successfully");
                Ok(())
            }
            Ok(Err(res)) => {
                error!(
                    block_hash = %block.block_hash.0.to_base58(),
                    block_height = block.height,
                    error = ?res,
                    "Newly produced block failed pre-validation"
                );
                Err(eyre!(
                    "Newly produced block {:?} ({}) failed pre-validation: {:?}",
                    &block.block_hash.0,
                    &block.height,
                    res
                ))
            }
            Err(e) => {
                error!(
                    block_hash = %block.block_hash.0.to_base58(),
                    block_height = block.height,
                    error = ?e,
                    "Failed to deliver BlockDiscoveredMessage"
                );
                Err(eyre!(
                    "Could not deliver BlockDiscoveredMessage for block {} ({}) : {:?}",
                    &block.block_hash.0.to_base58(),
                    &block.height,
                    e
                ))
            }
        }?;

        if is_difficulty_updated {
            debug!("Broadcasting difficulty update");
            self.inner()
                .mining_broadcaster
                .do_send(BroadcastDifficultyUpdate(block.clone()));
        }

        // Broadcast the EVM payload
        debug!("Broadcasting EVM execution payload");
        let execution_payload_gossip_data = GossipBroadcastMessage::from(eth_built_payload.clone());
        if let Err(payload_broadcast_error) = self
            .inner()
            .service_senders
            .gossip_broadcast
            .send(execution_payload_gossip_data)
        {
            error!(
                error = ?payload_broadcast_error,
                "Failed to broadcast execution payload"
            );
        } else {
            debug!("EVM execution payload broadcast successfully");
        }

        info!(
            block_hash = %block.block_hash.0.to_base58(),
            block_height = block_height,
            difficulty = %block.diff,
            cumulative_difficulty = %block.cumulative_diff,
            reward_amount = %block.reward_amount,
            "Block production completed successfully"
        );

        Ok(Some(block.clone()))
    }

    #[tracing::instrument(skip_all, fields(
        prev_timestamp = prev_block_header.timestamp,
        current_timestamp = current_timestamp
    ))]
    fn block_reward(
        &self,
        prev_block_header: &IrysBlockHeader,
        current_timestamp: u128,
    ) -> Result<Amount<irys_types::storage_pricing::phantoms::Irys>, eyre::Error> {
        let prev_timestamp_sec = prev_block_header.timestamp.saturating_div(1000);
        let current_timestamp_sec = current_timestamp.saturating_div(1000);
        let time_diff_sec = current_timestamp_sec.saturating_sub(prev_timestamp_sec);

        debug!(
            prev_timestamp_sec = prev_timestamp_sec,
            current_timestamp_sec = current_timestamp_sec,
            time_diff_sec = time_diff_sec,
            "Calculating block reward"
        );

        let reward_amount = self
            .inner()
            .reward_curve
            .reward_between(prev_timestamp_sec, current_timestamp_sec)
            .map_err(|e| eyre!("Failed to calculate reward from curve: {}", e))?;

        debug!(
            reward_amount = %reward_amount.amount,
            "Block reward calculated"
        );

        Ok(reward_amount)
    }

    #[tracing::instrument(skip_all, fields(
        parent_hash = %parent_block.block_hash.0.to_base58(),
        parent_height = parent_block.height
    ))]
    async fn get_ema_price(
        &self,
        parent_block: &IrysBlockHeader,
        parent_block_ema_snapshot: &EmaSnapshot,
    ) -> eyre::Result<ExponentialMarketAvgCalculation> {
        debug!("Fetching current oracle price");

        let oracle_irys_price = self
            .inner()
            .price_oracle
            .current_price()
            .await
            .context("Failed to fetch oracle price")?;

        debug!(
            oracle_price = %oracle_irys_price,
            "Oracle price fetched"
        );

        let ema_calculation = parent_block_ema_snapshot.calculate_ema_for_new_block(
            parent_block,
            oracle_irys_price,
            self.inner().config.consensus.token_price_safe_range,
            self.inner().config.consensus.ema.price_adjustment_interval,
        );

        debug!(
            ema_price = %ema_calculation.ema,
            oracle_price_for_inclusion = %ema_calculation.oracle_price_for_block_inclusion,
            "EMA price calculated"
        );

        Ok(ema_calculation)
    }

    #[tracing::instrument(skip_all, fields(
        parent_hash = %prev_block_header.block_hash.0.to_base58(),
        parent_height = prev_block_header.height
    ))]
    async fn get_mempool_txs(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> eyre::Result<(
        Vec<SystemTransactionLedger>,
        Vec<CommitmentTransaction>,
        Vec<IrysTransactionHeader>,
        (Vec<IrysTransactionHeader>, Vec<TxIngressProof>),
    )> {
        let block_height = prev_block_header.height + 1;
        let blocks_in_epoch = self.inner().config.consensus.epoch.num_blocks_in_epoch;
        let is_epoch_block = block_height % blocks_in_epoch == 0;

        debug!(
            new_block_height = block_height,
            is_epoch_block = is_epoch_block,
            blocks_in_epoch = blocks_in_epoch,
            "Fetching mempool transactions"
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner()
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetBestMempoolTxs(
                Some(BlockId::Hash(prev_block_header.evm_block_hash.into())),
                tx,
            ))
            .map_err(|e| eyre!("Failed to send GetBestMempoolTxs message: {}", e))?;

        let mempool_txs = rx
            .await
            .map_err(|e| eyre!("Failed to receive mempool response: {}", e))?
            .map_err(|e| eyre!("Mempool returned error: {}", e))?;

        info!(
            commitment_tx_count = mempool_txs.commitment_tx.len(),
            submit_tx_count = mempool_txs.submit_tx.len(),
            publish_tx_count = mempool_txs.publish_tx.0.len(),
            ingress_proof_count = mempool_txs.publish_tx.1.len(),
            "Received transactions from mempool"
        );

        debug!(
            commitment_tx_ids = ?mempool_txs.commitment_tx.iter().map(|t| t.id.0.to_base58()).collect::<Vec<_>>(),
            "Commitment transaction IDs"
        );

        let commitment_txs_to_bill;
        let system_transaction_ledger;

        if is_epoch_block {
            info!("Creating epoch block - processing commitment rollup");

            // === EPOCH BLOCK: Rollup all commitments from the current epoch ===
            // Epoch blocks don't add new commitments - they summarize all commitments
            // that were validated throughout the epoch into a single rollup entry
            let entry = self
                .inner()
                .block_tree_guard
                .read()
                .get_commitment_snapshot(&prev_block_header.block_hash);

            if let Ok(entry) = entry {
                let mut txids = H256List::new();
                let commitments = entry.get_epoch_commitments();
                let commitment_count = commitments.len();

                // Collect all commitment transaction IDs from the epoch
                for tx in commitments.iter() {
                    txids.push(tx.id);
                }

                info!(
                    epoch_commitment_count = commitment_count,
                    "Creating commitment rollup for epoch block"
                );

                debug!(
                    rollup_tx_ids = ?txids.iter().map(|id| id.0.to_base58()).collect::<Vec<_>>(),
                    "Epoch commitment rollup transaction IDs"
                );

                system_transaction_ledger = SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: txids,
                };

                // IMPORTANT: On epoch blocks we don't bill the user for commitment txs
                commitment_txs_to_bill = vec![];
            } else {
                error!("Failed to find commitment snapshot for epoch block");
                eyre::bail!("Could not find commitment snapshot for current epoch");
            }
        } else {
            debug!("Creating regular block - processing new commitments");

            // === REGULAR BLOCK: Process new commitment transactions ===
            // Regular blocks add fresh commitment transactions from the mempool
            // and create ledger entries that reference these new commitments
            let mut txids = H256List::new();

            // Add each new commitment transaction to the ledger
            mempool_txs.commitment_tx.iter().for_each(|ctx| {
                txids.push(ctx.id);
            });

            if !txids.is_empty() {
                debug!(
                    commitment_count = txids.len(),
                    "Adding new commitment transactions to system ledger"
                );
            }

            system_transaction_ledger = SystemTransactionLedger {
                ledger_id: SystemLedger::Commitment.into(),
                tx_ids: txids,
            };
            // IMPORTANT: Commitment txs get billed on regular blocks
            commitment_txs_to_bill = mempool_txs.commitment_tx;
        };

        let system_ledgers = if !system_transaction_ledger.tx_ids.is_empty() {
            vec![system_transaction_ledger]
        } else {
            Vec::new()
        };

        debug!(
            system_ledger_count = system_ledgers.len(),
            commitment_txs_to_bill_count = commitment_txs_to_bill.len(),
            "Mempool transaction processing completed"
        );

        Ok((
            system_ledgers,
            commitment_txs_to_bill,
            mempool_txs.submit_tx,
            mempool_txs.publish_tx,
        ))
    }

    #[tracing::instrument(skip_all, fields(
        parent_evm_hash = %prev_block_header.evm_block_hash,
        parent_height = prev_block_header.height
    ))]
    async fn get_evm_block(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> eyre::Result<reth_ethereum_primitives::Block> {
        use reth::providers::BlockReader as _;

        debug!("Fetching parent EVM block from Reth provider");

        let parent = {
            let mut attempts = 0;
            loop {
                if attempts > 50 {
                    error!(
                        attempts = attempts,
                        "Failed to fetch parent EVM block after maximum attempts"
                    );
                    break None;
                }

                if attempts > 0 && attempts % 10 == 0 {
                    warn!(
                        attempts = attempts,
                        evm_block_hash = %prev_block_header.evm_block_hash,
                        "Still waiting for parent EVM block"
                    );
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
                            evm_block_hash = %prev_block_header.evm_block_hash,
                            attempts = attempts,
                            "Parent EVM block retrieved successfully"
                        );
                        break Some(block);
                    }
                    None => {
                        attempts += 1;
                        debug!(attempt = attempts, "EVM block not yet available, retrying");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
        .ok_or_else(|| {
            eyre!(
                "Failed to get parent EVM block {} after 50 attempts",
                prev_block_header.evm_block_hash
            )
        })?;
        // TODO: fix genesis hash computation when using init-state (persist modified chainspec?)
        // for now we just skip the check for the genesis block
        if prev_block_header.height > 0 {
            debug!("Validating parent EVM block hash");
            let computed_parent_evm_hash = parent.header.hash_slow();
            eyre::ensure!(
                computed_parent_evm_hash == prev_block_header.evm_block_hash,
                "Reth parent block hash mismatch for height {} - computed {} but expected {}",
                &prev_block_header.height,
                &computed_parent_evm_hash,
                &prev_block_header.evm_block_hash
            );
            debug!("Parent EVM block hash validation passed");
        } else {
            debug!("Skipping hash validation for genesis block");
        }
        Ok(parent)
    }
}

pub struct ProductionStrategy {
    pub inner: Arc<BlockProducerInner>,
}

impl BlockProdStrategy for ProductionStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.inner
    }
}

#[tracing::instrument(skip_all, fields(prev_timestamp = prev_block_header.timestamp))]
pub async fn current_timestamp(prev_block_header: &IrysBlockHeader) -> u128 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    // This exists to prevent block validation errors in the unlikely* case two blocks are produced with the exact same timestamp
    // This can happen due to EVM blocks using second-precision time, instead of our millisecond precision
    // this just waits until the next second (timers (afaict) never undersleep, so we don't need an extra buffer here)
    // *dev configs can easily trigger this behaviour
    // as_secs does not take into account/round the underlying nanos at all
    let now = if now.as_secs()
        == Duration::from_millis(prev_block_header.timestamp as u64).as_secs()
    {
        let nanos_into_sec = now.subsec_nanos();
        let nano_to_next_sec = 1_000_000_000 - nanos_into_sec;
        let time_to_wait = Duration::from_nanos(nano_to_next_sec as u64);
        info!(
            wait_duration_ms = time_to_wait.as_millis(),
            current_sec = now.as_secs(),
            prev_block_sec = Duration::from_millis(prev_block_header.timestamp as u64).as_secs(),
            "Waiting to prevent timestamp overlap with previous block"
        );
        tokio::time::sleep(time_to_wait).await;
        let new_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        debug!(
            new_timestamp_ms = new_time.as_millis(),
            "Timestamp adjusted to next second"
        );
        new_time
    } else {
        now
    };
    now.as_millis()
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
#[tracing::instrument(level = "debug", fields(tx_count = txs.len()))]
pub fn calculate_chunks_added(txs: &[IrysTransactionHeader], chunk_size: u64) -> u64 {
    let bytes_added = txs.iter().fold(0, |acc, tx| {
        acc + tx.data_size.div_ceil(chunk_size) * chunk_size
    });

    let chunks_added = bytes_added / chunk_size;

    debug!(
        tx_count = txs.len(),
        total_bytes = bytes_added,
        chunks_added = chunks_added,
        chunk_size = chunk_size,
        "Calculated chunks needed for transactions"
    );

    chunks_added
}

/// When a block is confirmed, this message broadcasts the block header and the
/// submit ledger TX that were added as part of this block.
/// This works for bootstrap node mining, but eventually blocks will be received
/// from peers and confirmed and their tx will be negotiated though the mempool.
#[derive(Debug, Clone, Message)]
#[rtype(result = "eyre::Result<()>")]
pub struct BlockConfirmedMessage(
    pub Arc<IrysBlockHeader>,
    pub Arc<Vec<IrysTransactionHeader>>,
);

/// Similar to [`BlockConfirmedMessage`] (but takes ownership of parameters) and
/// acts as a placeholder for when the node will maintain a block tree of
/// confirmed blocks and produce finalized blocks for the canonical chain when
///  enough confirmations have occurred. Chunks are moved from the in-memory
/// index to the storage modules when a block is finalized.
#[derive(Debug, Clone, Message)]
#[rtype(result = "eyre::Result<()>")]
pub struct BlockFinalizedMessage {
    /// Block being finalized
    pub block_header: Arc<IrysBlockHeader>,
    /// Include all the blocks transaction headers [Submit, Publish]
    pub all_txs: Arc<Vec<IrysTransactionHeader>>,
}
