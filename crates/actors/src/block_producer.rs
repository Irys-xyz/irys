use crate::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    block_tree_service::BlockTreeReadGuard,
    block_validation::generate_expected_system_transactions,
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    ema_service::EmaServiceMessage,
    epoch_service::{EpochServiceActor, GetPartitionAssignmentMessage},
    mempool_service::{MempoolFacade, MempoolServiceFacadeImpl},
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    CommitmentCacheMessage,
};
use actix::prelude::*;
use actors::mocker::Mocker;
use alloy_consensus::{
    transaction::SignerRecoverable, EthereumTxEnvelope, SignableTransaction, TxEip4844,
};
use alloy_network::TxSignerSync;
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_signer_local::LocalSigner;
use base58::ToBase58;
use eyre::eyre;
use irys_database::{
    block_header_by_hash, cached_data_root_by_data_root, db::IrysDatabaseExt as _,
    tables::IngressProofs, tx_header_by_txid, SystemLedger,
};
use irys_price_oracle::IrysPriceOracle;
use irys_reth::compose_system_tx;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, calculate_difficulty,
    next_cumulative_diff, Base64, Config, DataLedger, DataTransactionLedger, H256List,
    IngressProofsList, IrysBlockHeader, IrysTransactionHeader, PoaData, Signature,
    SystemTransactionLedger, TxIngressProof, VDFLimiterInfo, H256, U256,
};
use irys_vdf::state::VdfStateReadonly;
use nodit::interval::ii;
use openssl::sha;
use reth::rpc::types::BlockId;
use reth::{payload::EthBuiltPayload, rpc::eth::EthApiServer as _};
use reth_db::cursor::*;
use reth_db::Database;
use reth_transaction_pool::EthPooledTransaction;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{debug, error, info, warn, Span};

/// Used to mock up a `BlockProducerActor`
pub type BlockProducerMockActor = Mocker<BlockProducerActor>;

/// A mocked [`BlockProducerActor`] only needs to implement [`SolutionFoundMessage`]
#[derive(Debug)]
pub struct MockedBlockProducerAddr(pub Recipient<SolutionFoundMessage>);

/// `BlockProducerActor` creates blocks from mining solutions
#[derive(Debug, Clone)]
pub struct BlockProducerActor {
    /// Reference to the global database
    pub db: DatabaseProvider,
    /// Message the block discovery actor when a block is produced locally
    pub block_discovery_addr: Addr<BlockDiscoveryActor>,
    /// Tracks the global state of partition assignments on the protocol
    pub epoch_service: Addr<EpochServiceActor>,
    /// Reference to all the services we can send messages to
    pub service_senders: ServiceSenders,
    /// Mempool facade to interact with mempool service
    pub mempool: MempoolServiceFacadeImpl,
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
    /// Enforces block production limits during testing
    ///
    /// Controls the exact number of blocks produced to ensure test determinism.
    /// Since mining is probabilistic, solutions can be found nearly simultaneously
    /// before mining can be stopped after the first solution. This guard prevents
    /// producing extra blocks that would cause non-deterministic test behavior.
    pub blocks_remaining_for_test: Option<u64>,
    /// Tracing span
    pub span: Span,
    /// Reth node adapter
    pub reth_node_adapter: IrysRethNodeAdapter,
}

/// Actors can handle this message to learn about the `block_producer` actor at startup
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct RegisterBlockProducerMessage(pub Addr<BlockProducerActor>);

impl Actor for BlockProducerActor {
    type Context = Context<Self>;
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct SetTestBlocksRemainingMessage(pub Option<u64>);

impl Handler<SetTestBlocksRemainingMessage> for BlockProducerActor {
    type Result = ();

    fn handle(
        &mut self,
        msg: SetTestBlocksRemainingMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.blocks_remaining_for_test = msg.0;
    }
}

#[derive(Message, Debug)]
#[rtype(result = "eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>>")]
/// Announce to the node a mining solution has been found.
pub struct SolutionFoundMessage(pub SolutionContext);

impl Handler<SolutionFoundMessage> for BlockProducerActor {
    type Result =
        AtomicResponse<Self, eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>>>;
    #[tracing::instrument(skip_all, fields(
        minting_address = ?msg.0.mining_address,
        partition_hash = ?msg.0.partition_hash,
        chunk_offset = ?msg.0.chunk_offset,
        tx_path = ?msg.0.tx_path.is_none(),
        chunk = ?msg.0.chunk.len(),
    ))]
    fn handle(&mut self, msg: SolutionFoundMessage, _ctx: &mut Self::Context) -> Self::Result {
        let span = self.span.clone();
        let _span = span.enter();
        let solution = msg.0;
        info!(
            "BlockProducerActor solution received: solution_hash={}",
            solution.solution_hash.0.to_base58()
        );

        if let Some(blocks_remaining) = self.blocks_remaining_for_test {
            if blocks_remaining == 0 {
                info!(
                    "No more blocks needed for this test, skipping block production for solution_hash={}"
                    , solution.solution_hash.0.to_base58()
                );
                return AtomicResponse::new(Box::pin(fut::ready(Ok(None))));
            }
        }

        let mining_broadcaster_addr = BroadcastMiningService::from_registry();

        let Self {
            db,
            block_discovery_addr,
            epoch_service,
            service_senders,
            mempool,
            config,
            vdf_steps_guard,
            block_tree_guard,
            price_oracle,
            reward_curve,
            reth_node_adapter,
            ..
        } = self.clone();

        AtomicResponse::new(Box::pin( async move {
            // Get the current head of the longest chain, from the block_tree, to build off of
            let (canonical_blocks, _not_onchain_count) = block_tree_guard.read().get_canonical_chain();
            let (latest_block_hash, prev_block_height, _publish_tx, _submit_tx) = canonical_blocks.last().unwrap();
            info!(?latest_block_hash, ?prev_block_height, "Starting block production, previous block");

            let prev_block_header = match db.view_eyre(|tx| block_header_by_hash(tx, latest_block_hash, false)) {
                Ok(Some(header)) => Ok(header),
                Ok(None) => Err(eyre!("No block header found for hash {} ({})", latest_block_hash, prev_block_height + 1)),
                Err(e) =>  Err(eyre!("Failed to get previous block ({}) header: {}", prev_block_height, e))
            }?;
            let prev_block_hash = prev_block_header.block_hash;

            if solution.vdf_step <= prev_block_header.vdf_limiter_info.global_step_number {
                warn!("Skipping solution for old step number {}, previous block step number {} for block {} ({}) ", solution.vdf_step, prev_block_header.vdf_limiter_info.global_step_number, prev_block_hash.0.to_base58(),  prev_block_height);
                return Ok(None)
            }

            // make sure the parent block is canonical on the reth side so we can build upon it & query for balances
            // do this early
            RethServiceActor::from_registry().send(ForkChoiceUpdateMessage{
                head_hash: BlockHashType::Evm(prev_block_header.evm_block_hash),
                confirmed_hash: None,
                finalized_hash: None,
            }).await??;

            // Get all the ingress proofs for data promotion
            let mut publish_txs: Vec<IrysTransactionHeader> = Vec::new();
            let mut proofs: Vec<TxIngressProof> = Vec::new();
            {
                let read_tx = db.tx().map_err(|e|
                    eyre!("Failed to create DB transaction: {}", e)
                )?;

                let mut read_cursor = read_tx.new_cursor::<IngressProofs>().map_err(|e|
                    eyre!("Failed to create DB read cursor: {}", e)
                )?;

                let walker = read_cursor.walk(None).map_err(|e|
                    eyre!("Failed to create DB read cursor walker: {}", e)
                )?;

                let ingress_proofs = walker.collect::<Result<HashMap<_, _>, _>>().map_err(|e|
                    eyre!("Failed to collect ingress proofs from database: {}", e)
                )?;

                let mut publish_txids: Vec<H256> = Vec::new();
                // Loop tough all the data_roots with ingress proofs and find corresponding transaction ids
                for data_root in ingress_proofs.keys() {
                    let cached_data_root = cached_data_root_by_data_root(&read_tx, *data_root).unwrap();
                    if let Some(cached_data_root) = cached_data_root {
                        debug!(tx_ids = ?cached_data_root.txid_set, "publishing");
                        publish_txids.extend(cached_data_root.txid_set);
                    }
                }

                // Loop though all the pending tx to see which haven't been promoted
                for txid in &publish_txids {
                    let tx_header = match tx_header_by_txid(&read_tx, txid) {
                        Ok(Some(header)) => header,
                        Ok(None) => {
                            error!("No transaction header found for txid: {}", txid);
                            continue;
                        },
                        Err(e) => {
                            error!("Error fetching transaction header for txid {}: {}", txid, e);
                            continue;
                        }
                    };

                    // If there's no ingress proof included in the tx header, it means the tx still needs to be promoted
                    if tx_header.ingress_proofs.is_none() {
                        // Get the proof
                        match ingress_proofs.get(&tx_header.data_root) {
                            Some(proof) => {
                                let mut tx_header = tx_header.clone();
                                let proof = TxIngressProof {
                                    proof: proof.proof,
                                    signature: proof.signature,
                                };
                                proofs.push(proof.clone());
                                tx_header.ingress_proofs = Some(proof);
                                publish_txs.push(tx_header);
                            },
                            None => {
                                error!("No ingress proof found for data_root: {}", tx_header.data_root);
                                continue;
                            }
                        }
                    }
                }
            }

            {
                let txs = &publish_txs.iter().map(|h| h.id.0.to_base58()).collect::<Vec<_>>();
                debug!(?txs, "Publish transactions");
            }

            // Publish Ledger Transactions
            let publish_chunks_added = calculate_chunks_added(&publish_txs, config.consensus.chunk_size);
            let publish_max_chunk_offset =  prev_block_header.data_ledgers[DataLedger::Publish].max_chunk_offset + publish_chunks_added;
            let opt_proofs = (!proofs.is_empty()).then(|| IngressProofsList::from(proofs));

             // try to get the parent EVM block
             // we need to make sure it's present here, as `GetBestMempoolTxs` relies on it
             let parent = {
                let mut attempts = 0;
                loop {
                    if attempts > 50 {
                        break None;
                    }
                    let result = reth_node_adapter
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_hash(prev_block_header.evm_block_hash, false)
                        .await?;
                    match result {
                        Some(block) => {
                            info!("Got parent EVM block {} after {} attempts",&prev_block_header.evm_block_hash, &attempts);
                            break Some(block)
                        },
                        None => {
                            attempts += 1;
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
            }.expect("Should be able to get the parent EVM block");

            eyre::ensure!(parent.header.hash == prev_block_header.evm_block_hash, "reth parent block hash mismatch");

            // Submit Ledger Transactions
            // make sure the parent EVM block is present before calling this!
            let submit_txs = mempool.get_best_mempool_txs(Some(BlockId::Hash(prev_block_header.evm_block_hash.into()))).await.expect("to receive txns");

            let submit_chunks_added = calculate_chunks_added(&submit_txs.storage_tx, config.consensus.chunk_size);
            let submit_max_chunk_offset = prev_block_header.data_ledgers[DataLedger::Submit].max_chunk_offset + submit_chunks_added;
            let submit_txids = submit_txs.storage_tx.iter().map(|h| h.id).collect::<Vec<H256>>();

            // Commitment Transactions
            let block_height = prev_block_header.height + 1;
            let is_epoch_block = block_height % config.consensus.epoch.num_blocks_in_epoch == 0;

            // Construct commitment ledger based on block type (epoch vs regular)
            let commitment_ledger = if is_epoch_block {
                // In epoch blocks: collect and reference all previously validated commitments
                // from the current epoch without re-inserting them into the database
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = service_senders.commitment_cache.send(CommitmentCacheMessage::GetEpochCommitments { response: tx });

                // Get the commitments and create a new H256List with their IDs
                let commitments = rx.await.expect("to receive epoch commitments");
                let mut txids = H256List::new();

                for tx in commitments.iter() {
                    debug!("Epoch block includes commitment: {}", tx.id.0.to_base58());
                    txids.push(tx.id);
                }

                SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: txids
                }
            } else {
                // In regular blocks: process new commitment transactions
                // from the mempool and create a ledger entry referencing them
                let mut txids = H256List::new();
                submit_txs.commitment_tx.iter().for_each(|ctx| {
                    txids.push(ctx.id);
                });

                SystemTransactionLedger {
                    ledger_id: SystemLedger::Commitment.into(),
                    tx_ids: txids
                }
            };

            // Only add the system ledger to the block when commitments exist
            // Empty ledgers are excluded to optimize block size
            let system_ledgers = if !commitment_ledger.tx_ids.is_empty() {
                vec![commitment_ledger]
            } else {
                Vec::new()
            };

            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

            // This exists to prevent block validation errors in the unlikely* case two blocks are produced with the exact same timestamp
            // This can happen due to EVM blocks using second-precision time, instead of our millisecond precision
            // this just waits until the next second (timers (afaict) never undersleep, so we don't need an extra buffer here)
            // *dev configs can easily trigger this behaviour
            // as_secs does not take into account/round the underlying nanos at all
            let now =  if now.as_secs() == Duration::from_millis(prev_block_header.timestamp as u64).as_secs(){
                let nanos_into_sec = now.subsec_nanos();
                let nano_to_next_sec = 1_000_000_000 - nanos_into_sec;
                let time_to_wait = Duration::from_nanos(nano_to_next_sec as u64);
                info!("Waiting {:.2?} to prevent timestamp overlap", &time_to_wait);
                tokio::time::sleep(time_to_wait).await;
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
            }else {
                now
            };

            // Difficulty adjustment logic
            let current_timestamp = now.as_millis();
            let mut last_diff_timestamp = prev_block_header.last_diff_timestamp;
            let current_difficulty = prev_block_header.diff;
            let mut is_difficulty_updated = false;
            let (diff, stats) = calculate_difficulty(block_height, last_diff_timestamp, current_timestamp, current_difficulty, &config.consensus.difficulty_adjustment);

            // Did an adjustment happen?
            if let Some(stats) = stats {
                if stats.is_adjusted {
                    info!("🧊 block_time: {:?} is {}% off the target block_time of {:?} and above the minimum threshold of {:?}%, adjusting difficulty. ", stats.actual_block_time, stats.percent_different, stats.target_block_time, stats.min_threshold);
                    info!(" max: {}\nlast: {}\nnext: {}", U256::MAX, current_difficulty, diff);
                    is_difficulty_updated = true;
                } else {
                    info!("🧊 block_time: {:?} is {}% off the target block_time of {:?} and below the minimum threshold of {:?}%. No difficulty adjustment.", stats.actual_block_time, stats.percent_different, stats.target_block_time, stats.min_threshold);
                }
                last_diff_timestamp = current_timestamp;
            }

            let cumulative_difficulty = next_cumulative_diff(prev_block_header.cumulative_diff, diff);

            // Use the partition hash to figure out what ledger it belongs to
            let ledger_id = epoch_service
                .send(GetPartitionAssignmentMessage(solution.partition_hash))
                .await?
                .and_then(|pa| pa.ledger_id);

            let poa_chunk = Base64(solution.chunk);
            let poa_chunk_hash = H256(sha::sha256(&poa_chunk.0));
            let poa = PoaData {
                tx_path: solution.tx_path.map(Base64),
                data_path: solution.data_path.map(Base64),
                chunk: Some(poa_chunk),
                recall_chunk_index: solution.recall_chunk_index,
                ledger_id,
                partition_chunk_offset: solution.chunk_offset,
                partition_hash: solution.partition_hash,
            };

            let mut steps = if prev_block_header.vdf_limiter_info.global_step_number + 1 > solution.vdf_step - 1 {
                H256List::new()
            } else {
                vdf_steps_guard.get_steps(ii(prev_block_header.vdf_limiter_info.global_step_number + 1, solution.vdf_step - 1))
                .map_err(|e| eyre!("VDF step range {} unavailable while producing block {}, reason: {:?}, aborting", solution.vdf_step, &block_height, e))?
            };
            steps.push(solution.seed.0);

            // fetch the irys price from the oracle
            let oracle_irys_price = price_oracle.current_price().await?;
            // fetch the ema price to use
            let (tx, rx) = tokio::sync::oneshot::channel();
            service_senders.ema.send(EmaServiceMessage::GetPriceDataForNewBlock { response: tx, height_of_new_block: block_height, oracle_price: oracle_irys_price })?;
            let ema_irys_price = rx.await??;

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
            if block_height > 0 && block_height % config.consensus.epoch.num_blocks_in_epoch == 1 {
                // Record the hash of the epoch block (previous block) as our epoch reference
                last_epoch_hash = prev_block_hash;
            }

            let reward_amount = reward_curve.reward_between(
                // adjust ms -> sec
                prev_block_header.timestamp.saturating_div(1000),
                current_timestamp.saturating_div(1000)
            )?;

            let local_signer = LocalSigner::from(config.irys_signer().signer.clone());
            // Generate expected system transactions using shared logic
            let expected_system_txs = generate_expected_system_transactions(
                block_height,
                config.node_config.reward_address,
                reward_amount.amount.into(),
                H256(prev_block_header.evm_block_hash.0),
                &submit_txs.storage_tx,
            )?;
            let system_txs = expected_system_txs.into_iter().map(|tx| {
                let mut tx_raw = compose_system_tx(config.consensus.chain_id, &tx);
                let signature = local_signer.sign_transaction_sync(&mut tx_raw).expect("system tx must always be signable");
                let tx = EthereumTxEnvelope::<TxEip4844>::Legacy(tx_raw.into_signed(signature))
                    .try_into_recovered()
                    .expect("system tx must always be signable");

                EthPooledTransaction::new(tx.clone(), 300)
            }).collect::<Vec<_>>();

            // generate payload attributes
            let timestamp = now.as_secs();
            let payload_attrs = PayloadAttributes {
                timestamp, // tie timestamp together **THIS HAS TO BE SECONDS**
                prev_randao: parent.header.mix_hash,
                suggested_fee_recipient: config.node_config.reward_address,
                withdrawals: None, // these should ALWAYS be none
                parent_beacon_block_root: Some(prev_block_header.block_hash.into()),
            };
            let payload = reth_node_adapter
                .build_submit_payload_irys(prev_block_header.evm_block_hash, payload_attrs, system_txs)
                .await?;

            // trigger forkchoice update via engine api to commit the block to the blockchain
            reth_node_adapter
                .update_forkchoice_full(
                    payload.block().hash(),
                    // we mark this block as confirmed because we produced it
                    Some(payload.block().hash()),
                    // marking a block as finalized is handled by a different service
                    None,
                )
                .await?;

            let evm_block_hash =  payload.block().hash();

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
                reward_address: config.node_config.reward_address,
                reward_amount: reward_amount.amount,
                miner_address: solution.mining_address,
                signature: Signature::test_signature().into(), // temp value until block is signed with the mining singer
                timestamp: current_timestamp,
                system_ledgers,
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
                        tx_root: DataTransactionLedger::merklize_tx_root(&submit_txs.storage_tx).0,
                        tx_ids: H256List(submit_txids.clone()),
                        max_chunk_offset: submit_max_chunk_offset,
                        expires: Some(1622543200),
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
                oracle_irys_price: ema_irys_price.range_adjusted_oracle_price,
                ema_irys_price: ema_irys_price.ema,
            };

            // Now that all fields are initialized, Sign the block and initialize its block_hash
            let block_signer = config.irys_signer();
            block_signer.sign_block_header(&mut irys_block)?;

            let block = Arc::new(irys_block);
            match block_discovery_addr.send(BlockDiscoveredMessage(block.clone())).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(res)) => {
                    error!("Newly produced block {} ({}) failed pre-validation: {:?}", &block.block_hash.0.to_base58(), &block.height, res);
                    Err(eyre!("Newly produced block {} ({}) failed pre-validation: {:?}", &block.block_hash.0.to_base58(), &block.height, res))
                },
                Err(e) => {
                    error!("Could not deliver BlockDiscoveredMessage for block {} ({}) : {:?}", &block.block_hash.0.to_base58(), &block.height, e);
                    Err(eyre!("Could not deliver BlockDiscoveredMessage for block {} ({}) : {:?}", &block.block_hash.0.to_base58(), &block.height, e))
                }
            }?;

            // we set the canon head here, as we produced this block, and this lets us build off of it
            RethServiceActor::from_registry().send(ForkChoiceUpdateMessage{
                head_hash: BlockHashType::Evm(evm_block_hash),
                confirmed_hash: None,
                finalized_hash: None,
            }).await??;


            if is_difficulty_updated {
                mining_broadcaster_addr.do_send(BroadcastDifficultyUpdate(block.clone()));
            }

            info!("Finished producing block {}, ({})", &block.block_hash.0.to_base58(),&block_height);

            Ok(Some((block.clone(), payload)))
        }
        .into_actor(self)
        .map(|result, actor, _ctx| {
            // Only decrement blocks_remaining_for_test when a block is successfully produced
            if let Ok(Some(_)) = &result {
                // If blocks_remaining_for_test is Some, decrement it by 1
                if let Some(remaining) = actor.blocks_remaining_for_test {
                    actor.blocks_remaining_for_test = Some(remaining.saturating_sub(1));
                }
            }
            result
        })
        .map_err(|e: eyre::Error, _, _| {
            error!("Error producing a block: {}", &e);
            std::process::abort();
        })
        ))
    }
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
