use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix::prelude::*;
use actors::mocker::Mocker;
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV1Irys, PayloadAttributes};
use base58::ToBase58;
use eyre::eyre;
use irys_database::{
    block_header_by_hash, cached_data_root_by_data_root, tables::IngressProofs, tx_header_by_txid,
    Ledger,
};
use irys_primitives::{DataShadow, IrysTxId, ShadowTx, ShadowTxType, Shadows};
use irys_reth_node_bridge::{adapter::node::RethNodeContext, node::RethNodeProvider};
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, calculate_difficulty,
    next_cumulative_diff, storage_config::StorageConfig, vdf_config::VDFStepsConfig, Address,
    Base64, DifficultyAdjustmentConfig, H256List, IngressProofsList, IrysBlockHeader,
    IrysTransactionHeader, PoaData, Signature, TransactionLedger, TxIngressProof, VDFLimiterInfo,
    H256, U256,
};
use nodit::interval::ii;
use openssl::sha;
use reth::revm::primitives::B256;
use reth_db::cursor::*;
use reth_db::Database;
use tracing::{error, info, warn};

use crate::{
    block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor},
    block_tree_service::BlockTreeReadGuard,
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    epoch_service::{EpochServiceActor, GetPartitionAssignmentMessage},
    mempool_service::{GetBestMempoolTxs, MempoolService},
    vdf_service::VdfStepsReadGuard,
};

/// Used to mock up a `BlockProducerActor`
pub type BlockProducerMockActor = Mocker<BlockProducerActor>;

/// A mocked [`BlockProducerActor`] only needs to implement [`SolutionFoundMessage`]
#[derive(Debug)]
pub struct MockedBlockProducerAddr(pub Recipient<SolutionFoundMessage>);

/// `BlockProducerActor` creates blocks from mining solutions
#[derive(Debug)]
pub struct BlockProducerActor {
    /// Reference to the global database
    pub db: DatabaseProvider,
    /// Address of the mempool actor
    pub mempool_addr: Addr<MempoolService>,
    /// Message the block discovery actor when a block is produced locally
    pub block_discovery_addr: Addr<BlockDiscoveryActor>,
    /// Tracks the global state of partition assignments on the protocol
    pub epoch_service: Addr<EpochServiceActor>,
    /// Reference to the VM node
    pub reth_provider: RethNodeProvider,
    /// Storage config
    pub storage_config: StorageConfig,
    /// Difficulty adjustment parameters for the Irys Protocol
    pub difficulty_config: DifficultyAdjustmentConfig,
    /// VDF configuration parameters
    pub vdf_config: VDFStepsConfig,
    /// Store last VDF Steps
    pub vdf_steps_guard: VdfStepsReadGuard,
    /// Get the head of the chain
    pub block_tree_guard: BlockTreeReadGuard,
}

/// Actors can handle this message to learn about the `block_producer` actor at startup
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct RegisterBlockProducerMessage(pub Addr<BlockProducerActor>);

impl BlockProducerActor {
    /// Initializes a new `BlockProducerActor`
    pub const fn new(
        db: DatabaseProvider,
        mempool_addr: Addr<MempoolService>,
        block_discover_addr: Addr<BlockDiscoveryActor>,
        epoch_service: Addr<EpochServiceActor>,
        reth_provider: RethNodeProvider,
        storage_config: StorageConfig,
        difficulty_config: DifficultyAdjustmentConfig,
        vdf_config: VDFStepsConfig,
        vdf_steps_guard: VdfStepsReadGuard,
        block_tree_guard: BlockTreeReadGuard,
    ) -> Self {
        Self {
            db,
            mempool_addr,
            block_discovery_addr: block_discover_addr,
            epoch_service,
            reth_provider,
            storage_config,
            difficulty_config,
            vdf_config,
            vdf_steps_guard,
            block_tree_guard,
        }
    }
}

impl Actor for BlockProducerActor {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        println!("block_producer actor started!");
        // Or do any initialization you need
    }
}

#[derive(Message, Debug)]
#[rtype(result = "eyre::Result<Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>>")]
/// Announce to the node a mining solution has been found.
pub struct SolutionFoundMessage(pub SolutionContext);

impl Handler<SolutionFoundMessage> for BlockProducerActor {
    type Result = AtomicResponse<
        Self,
        eyre::Result<Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>>,
    >;

    fn handle(&mut self, msg: SolutionFoundMessage, _ctx: &mut Self::Context) -> Self::Result {
        let solution = msg.0;
        info!(
            "BlockProducerActor solution received miner:{:?} partition_hash:{:?} offset:{} capacity:{} chunk_length:{}",
            solution.mining_address,
            solution.partition_hash,
            solution.chunk_offset,
            solution.tx_path.is_none(),
            solution.chunk.len(),
        );

        let mempool_addr = self.mempool_addr.clone();
        let block_discovery_addr = self.block_discovery_addr.clone();
        let epoch_service_addr = self.epoch_service.clone();
        let mining_broadcaster_addr = BroadcastMiningService::from_registry();

        let reth = self.reth_provider.clone();
        let db = self.db.clone();
        let difficulty_config = self.difficulty_config;
        let chunk_size = self.storage_config.chunk_size;
        let block_tree_guard = self.block_tree_guard.clone();

        // let self_addr = ctx.address();
        // let storage_config = self.storage_config.clone();
        // let vdf_config = self.vdf_config.clone();
        let vdf_steps = self.vdf_steps_guard.clone();

        AtomicResponse::new(Box::pin( async move {
            // Get the current head of the longest chain, from the block_tree, to build off of
            let (canonical_blocks, _not_onchain_count) = block_tree_guard.read().get_canonical_chain();
            let (latest_block_hash, prev_block_height, _publish_tx, _submit_tx) = canonical_blocks.last().unwrap();
            info!("Starting block production, previous block: {} ({})", &latest_block_hash, &prev_block_height);

            let block_item = match db.view_eyre(|tx| block_header_by_hash(tx, latest_block_hash)) {
                Ok(Some(header)) => Ok(header),
                    Ok(None) =>
                    Err(eyre!("No block header found for hash {}", latest_block_hash)),
                Err(e) =>  Err(eyre!("Failed to get previous block header: {}", e))
            }?;

            // Retrieve the previous block header and hash

            let prev_block_hash = block_item.block_hash;
            let prev_block_header: IrysBlockHeader = match db.view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash)) {
                Ok(Some(header)) => Ok(header),
                Ok(None) =>
                    Err(eyre!("No block header found for block {} ({}) ", prev_block_hash.0.to_base58(), prev_block_height)),
                Err(e) =>
                    Err(eyre!("Failed to get previous block {} ({}) header: {}", prev_block_hash.0.to_base58(), prev_block_height,  e)) 
            }?;

            if solution.vdf_step <= prev_block_header.vdf_limiter_info.global_step_number {
                warn!("Solution for old step number {}, previous block step number {}", solution.vdf_step, prev_block_header.vdf_limiter_info.global_step_number);
            }

            // Get all the ingress proofs for data promotion
            let mut publish_txs: Vec<IrysTransactionHeader> = Vec::new();
            let mut publish_txids: Vec<H256> = Vec::new();
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

                // Loop tough all the data_roots with ingress proofs and find corresponding transaction ids
                for data_root in ingress_proofs.keys() {
                    let cached_data_root = cached_data_root_by_data_root(&read_tx, *data_root).unwrap();
                    if let Some(cached_data_root) = cached_data_root {
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

            // Publish Ledger Transactions
            let publish_chunks_added = calculate_chunks_added(&publish_txs, chunk_size);
            let publish_max_chunk_offset =  prev_block_header.ledgers[Ledger::Publish].max_chunk_offset + publish_chunks_added;
            let opt_proofs = if !proofs.is_empty() {
                 Some(IngressProofsList::from(proofs))
            } else {
                None
            };

            // Submit Ledger Transactions    
            let submit_txs: Vec<IrysTransactionHeader> =
                mempool_addr.send(GetBestMempoolTxs).await.unwrap();

            let submit_chunks_added = calculate_chunks_added(&submit_txs, chunk_size);
            let submit_max_chunk_offset = prev_block_header.ledgers[Ledger::Submit].max_chunk_offset + submit_chunks_added;

            let submit_txids = submit_txs.iter().map(|h| h.id).collect::<Vec<H256>>();
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

            // Difficulty adjustment logic
            let current_timestamp = now.as_millis();
            let mut last_diff_timestamp = prev_block_header.last_diff_timestamp;
            let current_difficulty = prev_block_header.diff;
            let mut is_difficulty_updated = false;
            let block_height = prev_block_header.height + 1;
            let (diff, stats) = calculate_difficulty(block_height, last_diff_timestamp, current_timestamp, current_difficulty, &difficulty_config);

            // Did an adjustment happen?
            if let Some(stats) = stats {
                if stats.is_adjusted {
                    println!("🧊 block_time: {:?} is {}% off the target block_time of {:?} and above the minimum threshold of {:?}%, adjusting difficulty. ", stats.actual_block_time, stats.percent_different, stats.target_block_time, stats.min_threshold);
                    println!(" max: {}\nlast: {}\nnext: {}", U256::MAX, current_difficulty, diff);
                    is_difficulty_updated = true;
                } else {
                    println!("🧊 block_time: {:?} is {}% off the target block_time of {:?} and below the minimum threshold of {:?}%. No difficulty adjustment.", stats.actual_block_time, stats.percent_different, stats.target_block_time, stats.min_threshold);
                }
                last_diff_timestamp = current_timestamp;
            }

            let cumulative_difficulty = next_cumulative_diff(prev_block_header.cumulative_diff, diff);

            // TODO: Hash the block signature to create a block_hash
            // Generate a very stupid block_hash right now which is just
            // the hash of the timestamp
            let block_hash = hash_sha256(&current_timestamp.to_le_bytes());

            // Use the partition hash to figure out what ledger it belongs to
            let ledger_id = epoch_service_addr
                .send(GetPartitionAssignmentMessage(solution.partition_hash))
                .await
                .unwrap()
                .and_then(|pa| pa.ledger_id);


            let poa = PoaData {
                tx_path: solution.tx_path.map(Base64),
                data_path: solution.data_path.map(Base64),
                chunk: Base64(solution.chunk),
                recall_chunk_index: solution.recall_chunk_index,
                ledger_id,
                partition_chunk_offset: solution.chunk_offset,
                partition_hash: solution.partition_hash,
            };

            let mut steps = if prev_block_header.vdf_limiter_info.global_step_number + 1 > solution.vdf_step - 1 {
                H256List::new()
            } else {
                vdf_steps.get_steps(ii(prev_block_header.vdf_limiter_info.global_step_number + 1, solution.vdf_step - 1)).await
                .map_err(|e| eyre!("VDF step range {} unavailable while producing block {}, reason: {:?}, aborting", solution.vdf_step, &block_height, e))?
            };
            steps.push(solution.seed.0);

            let mut irys_block = IrysBlockHeader {
                block_hash,
                height: block_height,
                diff,
                cumulative_diff: cumulative_difficulty,
                last_diff_timestamp,
                solution_hash: solution.solution_hash,
                previous_solution_hash: H256::zero(),
                last_epoch_hash: H256::random(),
                chunk_hash: H256(sha::sha256(&poa.chunk.0)),
                previous_block_hash: prev_block_hash,
                previous_cumulative_diff: prev_block_header.cumulative_diff,
                poa,
                reward_address: Address::ZERO ,
                miner_address: solution.mining_address,
                signature: Signature::test_signature().into(),
                timestamp: current_timestamp,
                ledgers: vec![
                    // Permanent Publish Ledger
                    TransactionLedger {
                        ledger_id: Ledger::Publish.into(),
                        tx_root: TransactionLedger::merklize_tx_root(&publish_txs).0,
                        tx_ids: H256List(publish_txids.clone()),
                        max_chunk_offset: publish_max_chunk_offset,
                        expires: None,
                        proofs: opt_proofs,
                    },
                    // Term Submit Ledger
                    TransactionLedger {
                        ledger_id: Ledger::Submit.into(),
                        tx_root: TransactionLedger::merklize_tx_root(&submit_txs).0,
                        tx_ids: H256List(submit_txids.clone()),
                        max_chunk_offset: submit_max_chunk_offset,
                        expires: Some(1622543200),
                        proofs: None,
                    },
                ],
                evm_block_hash: B256::ZERO,
                vdf_limiter_info: VDFLimiterInfo {
                    global_step_number: solution.vdf_step,
                    output: solution.seed.into_inner(),
                    last_step_checkpoints: solution.checkpoints,
                    prev_output: prev_block_header.vdf_limiter_info.output,
                    seed: prev_block_header.vdf_limiter_info.seed,
                    steps,
                    ..Default::default()
                },
            };

            // RethNodeContext is a type-aware wrapper that lets us interact with the reth node
            let context =  RethNodeContext::new(reth.into()).await.map_err(|e| eyre!("Error connecting to Reth - {}", e))?;

            let shadows = Shadows::new(
                submit_txs
                    .iter()
                    .map(|header| ShadowTx {
                        tx_id: IrysTxId::from_slice(header.id.as_bytes()),
                        fee: irys_primitives::U256::from(
                            header.total_fee(),
                        ),
                        address: header.signer,
                        tx: ShadowTxType::Data(DataShadow {
                            fee: irys_primitives::U256::from(
                                header.total_fee(),
                            ),
                        }),
                    })
                    .collect(),
            );

            // create a new reth payload

            // generate payload attributes
            // TODO: we need previous block metadata to fill in parent & prev_randao
            let payload_attrs = PayloadAttributes {
                timestamp: now.as_secs(), // tie timestamp together **THIS HAS TO BE SECONDS**
                prev_randao: B256::ZERO,
                suggested_fee_recipient: irys_block.reward_address,
                withdrawals: None,
                parent_beacon_block_root: None,
                shadows: Some(shadows),
            };

            let exec_payload = context
                .engine_api
                .build_payload_v1_irys(B256::ZERO, payload_attrs)
                .await
                .unwrap();

            // we can examine the execution status of generated shadow txs
            // let shadow_receipts = exec_payload.shadow_receipts;

            let v1_payload = exec_payload
                .clone()
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner;
            // TODO @JesseTheRobot create a deref(?) trait so this isn't as bad
            let block_hash = v1_payload.block_hash;

            irys_block.evm_block_hash = block_hash;

            // TODO: Irys block header building logic

            // TODO: Commit block to DB and send to networking layer

            // make the built evm payload canonical
            context
                .engine_api
                .update_forkchoice(v1_payload.parent_hash, v1_payload.block_hash)
                .await
                .unwrap();

            let block = Arc::new(irys_block);
            match block_discovery_addr.send(BlockDiscoveredMessage(block.clone())).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(res)) => {
                    error!("Newly produced block {} ({}) failed pre-validation - {:?}", &block.block_hash.0.to_base58(), &block.height, res);
                    Err(eyre!("Newly produced block {} ({}) failed pre-validation - {:?}", &block.block_hash.0.to_base58(), &block.height, res))
                },
                Err(e) => {
                    error!("Could not deliver BlockDiscoveredMessage for block {} ({}) : {:?}", &block.block_hash.0.to_base58(), &block.height, e);
                    Err(eyre!("Could not deliver BlockDiscoveredMessage for block {} ({}) : {:?}", &block.block_hash.0.to_base58(), &block.height, e))
                }
            }?;

            if is_difficulty_updated {
                mining_broadcaster_addr.do_send(BroadcastDifficultyUpdate(block.clone()));
            }

            info!("Finished producing block {}, ({})", &block_hash.0.to_base58(),&block_height);

            Ok(Some((block.clone(), exec_payload)))
        }
        .into_actor(self),
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
#[rtype(result = "()")]
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
#[rtype(result = "Result<(), ()>")]
pub struct BlockFinalizedMessage {
    /// Block being finalized
    pub block_header: Arc<IrysBlockHeader>,
    /// Include all the blocks transaction headers [Submit, Publish]
    pub all_txs: Arc<Vec<IrysTransactionHeader>>,
}

/// SHA256 hash the message parameter
fn hash_sha256(message: &[u8]) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    H256::from(hasher.finish())
}
