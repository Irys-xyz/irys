use crate::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    mempool_service::MempoolServiceMessage,
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    shadow_tx_generator::ShadowTxGenerator,
};
use actix::prelude::*;
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
use eyre::eyre;
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _, SystemLedger};
use irys_domain::{BlockTreeReadGuard, EmaSnapshot, ExponentialMarketAvgCalculation};
use irys_price_oracle::IrysPriceOracle;
use irys_reth::{
    compose_shadow_tx,
    payload::{DeterministicShadowTxKey, ShadowTxStore},
    reth_node_ethereum::EthEngineTypes,
    IrysEthereumNode,
};
use irys_reth_node_bridge::node::NodeProvider;
use irys_reward_curve::HalvingCurve;
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, calculate_difficulty,
    next_cumulative_diff, storage_pricing::Amount, Base64, CommitmentTransaction, Config,
    DataLedger, DataTransactionLedger, GossipBroadcastMessage, H256List, IngressProofsList,
    IrysBlockHeader, IrysTransactionHeader, PoaData, Signature, SystemTransactionLedger,
    TokioServiceHandle, TxIngressProof, VDFLimiterInfo, H256, U256,
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
    tasks::shutdown::Shutdown,
};
use reth_transaction_pool::EthPooledTransaction;
use std::{
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
    shutdown: Shutdown,
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
        inner: Arc<BlockProducerInner>,
        blocks_remaining_for_test: Option<u64>,
        rx: mpsc::UnboundedReceiver<BlockProducerCommand>,
    ) -> TokioServiceHandle {
        info!(
            "Spawning block producer service with blocks_remaining_for_test: {:?}",
            blocks_remaining_for_test
        );

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let handle = tokio::spawn(async move {
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
        });

        TokioServiceHandle {
            name: "block_producer_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
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
            recall_chunk_index: solution.recall_chunk_index,
            ledger_id,
            partition_chunk_offset: solution.chunk_offset,
            partition_hash: solution.partition_hash,
        };
        Ok((poa, poa_chunk_hash))
    }

    /// Fetches a block header from mempool or database
    async fn fetch_block_header(&self, block_hash: H256) -> eyre::Result<IrysBlockHeader> {
        // Try mempool first
        let (tx, rx) = oneshot::channel();
        self.inner()
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetBlockHeader(block_hash, false, tx))?;

        match rx.await? {
            Some(header) => Ok(header),
            None => {
                // Fall back to database
                self.inner()
                    .db
                    .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
                    .ok_or_else(|| eyre!("No block header found for hash {}", block_hash))
            }
        }
    }

    /// Gets the EMA snapshot for a given block
    fn get_block_ema_snapshot(&self, block_hash: &H256) -> eyre::Result<Arc<EmaSnapshot>> {
        let read = self.inner().block_tree_guard.read();
        read.get_ema_snapshot(block_hash)
            .ok_or_else(|| eyre!("EMA snapshot not found for block {}", block_hash))
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
        let inner = self.inner();
        // Use BlockValidationTracker to select the parent block
        let parent_block_hash = BlockValidationTracker::new(
            inner.block_tree_guard.clone(),
            inner.service_senders.clone(),
            MAX_WAIT_TIME,
        )
        .wait_for_validation()
        .await?;

        // Fetch the parent block header
        let header = self.fetch_block_header(parent_block_hash).await?;

        // Get the EMA snapshot
        let ema_snapshot = self.get_block_ema_snapshot(&header.block_hash)?;

        Ok((header, ema_snapshot))
    }

    async fn fully_produce_new_block(
        &self,
        solution: SolutionContext,
    ) -> eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>> {
        let (prev_block_header, prev_block_ema_snapshot) = self.parent_irys_block().await?;
        let prev_evm_block = self.get_evm_block(&prev_block_header).await?;
        let current_timestamp = current_timestamp(&prev_block_header).await;

        let (system_tx_ledger, commitment_txs_to_bill, submit_txs, publish_txs) =
            self.get_mempool_txs(&prev_block_header).await?;
        let block_reward = self.block_reward(&prev_block_header, current_timestamp)?;
        let eth_built_payload = self
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &commitment_txs_to_bill,
                &submit_txs,
                block_reward,
                current_timestamp,
            )
            .await?;
        let evm_block = eth_built_payload.block();

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
            .await?;

        let Some(block) = block else { return Ok(None) };
        Ok(Some((block, eth_built_payload)))
    }

    /// Extracts and collects all transactions that should be included in a block
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
        let local_signer = LocalSigner::from(self.inner().config.irys_signer().signer);
        // Generate expected shadow transactions using shared logic
        let shadow_txs = ShadowTxGenerator::new(
            &block_height,
            &self.inner().config.node_config.reward_address,
            &reward_amount.amount,
            prev_block_header,
        );
        let shadow_txs = shadow_txs
            .generate_all(commitment_txs_to_bill, submit_txs)
            .map(|tx_result| {
                let tx = tx_result?;
                let mut tx_raw = compose_shadow_tx(self.inner().config.consensus.chain_id, &tx);
                let signature = local_signer
                    .sign_transaction_sync(&mut tx_raw)
                    .expect("shadow tx must always be signable");
                let tx = EthereumTxEnvelope::<TxEip4844>::Legacy(tx_raw.into_signed(signature))
                    .try_into_recovered()
                    .expect("shadow tx must always be signable");

                Ok::<EthPooledTransaction, eyre::Report>(EthPooledTransaction::new(tx, 300))
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.build_and_submit_reth_payload(
            prev_block_header,
            timestamp_ms,
            shadow_txs,
            perv_evm_block.header.mix_hash,
        )
        .await
    }

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

        if solution.vdf_step <= prev_block_header.vdf_limiter_info.global_step_number {
            warn!("Skipping solution for old step number {}, previous block step number {} for block {}", solution.vdf_step, prev_block_header.vdf_limiter_info.global_step_number, prev_block_hash.0.to_base58());
            return Ok(None);
        }

        let (publish_txs, proofs) = publish_txs;

        // Publish Ledger Transactions
        let publish_chunks_added =
            calculate_chunks_added(&publish_txs, self.inner().config.consensus.chunk_size);
        let publish_max_chunk_offset = prev_block_header.data_ledgers[DataLedger::Publish]
            .max_chunk_offset
            + publish_chunks_added;
        let opt_proofs = (!proofs.is_empty()).then(|| IngressProofsList::from(proofs));

        // Difficulty adjustment logic
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
                info!("🧊 block_time: {:?} is {}% off the target block_time of {:?} and above the minimum threshold of {:?}%, adjusting difficulty. ", stats.actual_block_time, stats.percent_different, stats.target_block_time, stats.min_threshold);
                info!(
                    " max: {}\nlast: {}\nnext: {}",
                    U256::MAX,
                    current_difficulty,
                    diff
                );
                is_difficulty_updated = true;
            } else {
                info!("🧊 block_time: {:?} is {}% off the target block_time of {:?} and below the minimum threshold of {:?}%. No difficulty adjustment.", stats.actual_block_time, stats.percent_different, stats.target_block_time, stats.min_threshold);
            }
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
        let (poa, poa_chunk_hash) = self.create_poa_data(&solution, ledger_id)?;

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
            vdf_limiter_info: VDFLimiterInfo::new(
                &solution,
                prev_block_header,
                steps,
                &self.inner().config,
            ),
            oracle_irys_price: ema_calculation.oracle_price_for_block_inclusion,
            ema_irys_price: ema_calculation.ema,
        };

        // Now that all fields are initialized, Sign the block and initialize its block_hash
        let block_signer = self.inner().config.irys_signer();
        block_signer.sign_block_header(&mut irys_block)?;
        let _res = self
            .inner()
            .reth_service
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Evm(irys_block.evm_block_hash),
                confirmed_hash: None,
                finalized_hash: None,
            })
            .await??;

        let block = Arc::new(irys_block);
        match self
            .inner()
            .block_discovery_addr
            .send(BlockDiscoveredMessage(block.clone()))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(res)) => {
                error!(
                    "Newly produced block {:?} ({}) failed pre-validation: {:?}",
                    &block.block_hash.0, &block.height, res
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
                    "Could not deliver BlockDiscoveredMessage for block {} ({}) : {:?}",
                    &block.block_hash.0.to_base58(),
                    &block.height,
                    e
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
            self.inner()
                .mining_broadcaster
                .do_send(BroadcastDifficultyUpdate(block.clone()));
        }

        // Broadcast the EVM payload
        let execution_payload_gossip_data = GossipBroadcastMessage::from(eth_built_payload.clone());
        if let Err(payload_broadcast_error) = self
            .inner()
            .service_senders
            .gossip_broadcast
            .send(execution_payload_gossip_data)
        {
            error!(
                "Failed to broadcast execution payload: {:?}",
                payload_broadcast_error
            );
        }

        info!(
            "Finished producing block {}, ({})",
            &block.block_hash.0.to_base58(),
            &block_height
        );

        Ok(Some(block.clone()))
    }

    fn block_reward(
        &self,
        prev_block_header: &IrysBlockHeader,
        current_timestamp: u128,
    ) -> Result<Amount<irys_types::storage_pricing::phantoms::Irys>, eyre::Error> {
        let reward_amount = self.inner().reward_curve.reward_between(
            // adjust ms -> sec
            prev_block_header.timestamp.saturating_div(1000),
            current_timestamp.saturating_div(1000),
        )?;
        Ok(reward_amount)
    }

    async fn get_ema_price(
        &self,
        parent_block: &IrysBlockHeader,
        parent_block_ema_snapshot: &EmaSnapshot,
    ) -> eyre::Result<ExponentialMarketAvgCalculation> {
        let oracle_irys_price = self.inner().price_oracle.current_price().await?;
        let ema_calculation = parent_block_ema_snapshot.calculate_ema_for_new_block(
            parent_block,
            oracle_irys_price,
            self.inner().config.consensus.token_price_safe_range,
            self.inner().config.consensus.ema.price_adjustment_interval,
        );

        Ok(ema_calculation)
    }

    async fn get_mempool_txs(
        &self,
        prev_block_header: &IrysBlockHeader,
    ) -> eyre::Result<(
        Vec<SystemTransactionLedger>,
        Vec<CommitmentTransaction>,
        Vec<IrysTransactionHeader>,
        (Vec<IrysTransactionHeader>, Vec<TxIngressProof>),
    )> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner()
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetBestMempoolTxs(
                Some(BlockId::Hash(prev_block_header.evm_block_hash.into())),
                tx,
            ))
            .expect("to send MempoolServiceMessage");
        let mempool_txs = rx.await.expect("to receive txns")?;
        let block_height = prev_block_header.height + 1;
        let is_epoch_block =
            block_height % self.inner().config.consensus.epoch.num_blocks_in_epoch == 0;
        debug!(
            "get_best_mempool_txs for block height: {} returned: {:#?}",
            block_height,
            mempool_txs
                .commitment_tx
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
        );
        let commitment_txs_to_bill;
        let system_transaction_ledger;
        if is_epoch_block {
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

                // Collect all commitment transaction IDs from the epoch
                for tx in commitments.iter() {
                    txids.push(tx.id);
                }

                debug!(
                    "Producing epoch block at height {} with commitments rollup tx {:#?}",
                    block_height, txids
                );

                system_transaction_ledger = SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: txids,
                };

                // IMPORTANT: On epoch blocks we don't bill the user for commitment txs
                commitment_txs_to_bill = vec![];
            } else {
                eyre::bail!("Could not find commitment snapshot for current epoch");
            }
        } else {
            // === REGULAR BLOCK: Process new commitment transactions ===
            // Regular blocks add fresh commitment transactions from the mempool
            // and create ledger entries that reference these new commitments
            let mut txids = H256List::new();

            // Add each new commitment transaction to the ledger
            mempool_txs.commitment_tx.iter().for_each(|ctx| {
                txids.push(ctx.id);
            });
            debug!(
                "Producing block at height {} with commitment tx {:#?}",
                block_height, txids
            );
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
        Ok((
            system_ledgers,
            commitment_txs_to_bill,
            mempool_txs.submit_tx,
            mempool_txs.publish_tx,
        ))
    }

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
        .expect("Should be able to get the parent EVM block");
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
}

pub struct ProductionStrategy {
    pub inner: Arc<BlockProducerInner>,
}

impl BlockProdStrategy for ProductionStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.inner
    }
}

pub async fn current_timestamp(prev_block_header: &IrysBlockHeader) -> u128 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    // This exists to prevent block validation errors in the unlikely* case two blocks are produced with the exact same timestamp
    // This can happen due to EVM blocks using second-precision time, instead of our millisecond precision
    // this just waits until the next second (timers (afaict) never undersleep, so we don't need an extra buffer here)
    // *dev configs can easily trigger this behaviour
    // as_secs does not take into account/round the underlying nanos at all
    let now =
        if now.as_secs() == Duration::from_millis(prev_block_header.timestamp as u64).as_secs() {
            let nanos_into_sec = now.subsec_nanos();
            let nano_to_next_sec = 1_000_000_000 - nanos_into_sec;
            let time_to_wait = Duration::from_nanos(nano_to_next_sec as u64);
            info!("Waiting {:.2?} to prevent timestamp overlap", &time_to_wait);
            tokio::time::sleep(time_to_wait).await;
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
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
pub fn calculate_chunks_added(txs: &[IrysTransactionHeader], chunk_size: u64) -> u64 {
    let bytes_added = txs.iter().fold(0, |acc, tx| {
        acc + tx.data_size.div_ceil(chunk_size) * chunk_size
    });

    bytes_added / chunk_size
}
/// When a block is confirmed, this message broadcasts the block header and the
/// submit ledger TX that were added as part of this block.
/// This works for bootstrap node mining, but eventually blocks will be received
/// from peers and confirmed and their tx will be negotiated though the mempool.
#[derive(Message, Debug, Clone)]
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
#[derive(Message, Debug, Clone)]
#[rtype(result = "eyre::Result<()>")]
pub struct BlockFinalizedMessage {
    /// Block being finalized
    pub block_header: Arc<IrysBlockHeader>,
    /// Include all the blocks transaction headers [Submit, Publish]
    pub all_txs: Arc<Vec<IrysTransactionHeader>>,
}
