use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix::prelude::*;
use actors::mocker::Mocker;
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV1Irys, PayloadAttributes};
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
    block_index::{BlockIndexActor, GetLatestBlockIndexMessage},
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    chunk_migration::ChunkMigrationActor,
    epoch_service::{EpochServiceActor, GetPartitionAssignmentMessage},
    mempool::{GetBestMempoolTxs, MempoolActor},
    vdf::VdfStepsReadGuard,
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
    pub mempool_addr: Addr<MempoolActor>,
    /// Address of the chunk migration actor
    pub chunk_migration_addr: Addr<ChunkMigrationActor>,
    /// Address of the `bock_index` actor
    pub block_index_addr: Addr<BlockIndexActor>,
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
}

/// Actors can handle this message to learn about the `block_producer` actor at startup
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct RegisterBlockProducerMessage(pub Addr<BlockProducerActor>);

impl BlockProducerActor {
    /// Initializes a new `BlockProducerActor`
    pub const fn new(
        db: DatabaseProvider,
        mempool_addr: Addr<MempoolActor>,
        chunk_migration_addr: Addr<ChunkMigrationActor>,
        block_index_addr: Addr<BlockIndexActor>,
        block_discover_addr: Addr<BlockDiscoveryActor>,
        epoch_service: Addr<EpochServiceActor>,
        reth_provider: RethNodeProvider,
        storage_config: StorageConfig,
        difficulty_config: DifficultyAdjustmentConfig,
        vdf_config: VDFStepsConfig,
        vdf_steps_guard: VdfStepsReadGuard,
    ) -> Self {
        Self {
            db,
            mempool_addr,
            chunk_migration_addr,
            block_index_addr,
            block_discovery_addr: block_discover_addr,
            epoch_service,
            reth_provider,
            storage_config,
            difficulty_config,
            vdf_config,
            vdf_steps_guard,
        }
    }
}

impl Actor for BlockProducerActor {
    type Context = Context<Self>;
}

#[derive(Message, Debug)]
#[rtype(result = "Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>")]
/// Announce to the node a mining solution has been found.
pub struct SolutionFoundMessage(pub SolutionContext);

impl Handler<SolutionFoundMessage> for BlockProducerActor {
    type Result =
        AtomicResponse<Self, Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>>;

    fn handle(&mut self, msg: SolutionFoundMessage, _ctx: &mut Self::Context) -> Self::Result {
        let solution = msg.0;
        info!(
            "BlockProducerActor solution received partition_hash:{:?} offset:{} capacity:{} chunk_length:{}",
            solution.partition_hash,
            solution.chunk_offset,
            solution.tx_path.is_none(),
            solution.chunk.len(),
        );

        if solution.chunk.len() <= 32 {
            info!("Chunk:{:?}", solution.chunk)
        }

        let mempool_addr = self.mempool_addr.clone();
        let block_index_addr = self.block_index_addr.clone();
        let block_discovery_addr = self.block_discovery_addr.clone();
        let epoch_service_addr = self.epoch_service.clone();
        let chunk_migration_addr = self.chunk_migration_addr.clone();
        let mining_broadcaster_addr = BroadcastMiningService::from_registry();

        let reth = self.reth_provider.clone();
        let db = self.db.clone();
        let difficulty_config = self.difficulty_config.clone();
        let chunk_size = self.storage_config.chunk_size;

        // let self_addr = ctx.address();
        // let storage_config = self.storage_config.clone();
        // let vdf_config = self.vdf_config.clone();
        let vdf_steps = self.vdf_steps_guard.clone();

        AtomicResponse::new(Box::pin( async move {
            // Acquire lock and check that the height hasn't changed identifying a race condition
            // TEMP: This demonstrates how to get the block height from the block_index_actor
            let latest_block = block_index_addr
                .send(GetLatestBlockIndexMessage {})
                .await
                .unwrap();

            // Retrieve the previous block header and hash
            let prev_block_header: IrysBlockHeader;
            let prev_block_hash: H256;

            if let Some(block_item) = latest_block {
                prev_block_hash = block_item.block_hash;
                prev_block_header = match db.view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash)) {
                    Ok(Some(header)) => header,
                    Ok(None) => {
                        error!("No block header found for hash {}", prev_block_hash);
                        return None;
                    },
                    Err(e) => {
                        error!("Failed to get previous block header: {}", e);
                        return None;
                    }
                };
            } else {
                error!("No previous block header found");
                return None;
            }

            if solution.vdf_step <= prev_block_header.vdf_limiter_info.global_step_number {
                warn!("Solution for old step number {}, previous block step number {}", solution.vdf_step, prev_block_header.vdf_limiter_info.global_step_number);
                return None;
            }

            // Get all the ingress proofs for data promotion
            let mut publish_txs: Vec<IrysTransactionHeader> = Vec::new();
            let mut publish_txids: Vec<H256> = Vec::new();
            let mut proofs: Vec<TxIngressProof> = Vec::new();
            {
                let read_tx = db.tx().map_err(|e| {
                    error!("Failed to create transaction: {}", e);
                }).ok()?;

                let mut read_cursor = read_tx.new_cursor::<IngressProofs>().map_err(|e| {
                    error!("Failed to create cursor: {}", e);
                }).ok()?;

                let walker = read_cursor.walk(None).map_err(|e| {
                    error!("Failed to create walker: {}", e);
                }).ok()?;

                let ingress_proofs = walker.collect::<Result<HashMap<_, _>, _>>().map_err(|e| {
                    error!("Failed to collect results: {}", e);
                }).ok()?;

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
            let ledger_num = epoch_service_addr
                .send(GetPartitionAssignmentMessage(solution.partition_hash))
                .await
                .unwrap()
                .and_then(|pa| pa.ledger_num);


            let poa = PoaData {
                tx_path: solution.tx_path.map(Base64),
                data_path: solution.data_path.map(Base64),
                chunk: Base64(solution.chunk),
                recall_chunk_index: solution.recall_chunk_index,
                ledger_num,
                partition_chunk_offset: solution.chunk_offset,
                partition_hash: solution.partition_hash,
            };

            let mut checkpoints = if prev_block_header.vdf_limiter_info.global_step_number + 1 > solution.vdf_step - 1 {
                H256List::new()
            } else {
                    match vdf_steps.get_steps(ii(prev_block_header.vdf_limiter_info.global_step_number + 1, solution.vdf_step - 1)).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Not available vdf steps range while producing block in step:{} reason: {:?}, waiting ...", solution.vdf_step, e);
                            return None
                        }
                    }
            };
            checkpoints.push(solution.seed.0);

            let mut irys_block = IrysBlockHeader {
                block_hash,
                height: block_height,
                diff,
                cumulative_diff: cumulative_difficulty,
                last_diff_timestamp: last_diff_timestamp,
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
                        tx_root: TransactionLedger::merklize_tx_root(&publish_txs).0,
                        txids: H256List(publish_txids.clone()),
                        max_chunk_offset: publish_max_chunk_offset,
                        expires: None,
                        proofs: opt_proofs,
                    },
                    // Term Submit Ledger
                    TransactionLedger {
                        tx_root: TransactionLedger::merklize_tx_root(&submit_txs).0,
                        txids: H256List(submit_txids.clone()),
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
                    checkpoints,
                    ..Default::default()
                },
            };

            // RethNodeContext is a type-aware wrapper that lets us interact with the reth node
            let context =  match RethNodeContext::new(reth.into()).await {
                Ok(c) => c,
                Err(_) => {
                    error!("Reth node is unavailable!");
                    return None
                }
            };

            let shadows = Shadows::new(
                submit_txs
                    .iter()
                    .map(|header| ShadowTx {
                        tx_id: IrysTxId::from_slice(header.id.as_bytes()),
                        fee: irys_primitives::U256::from(
                            header.term_fee + header.perm_fee.unwrap_or(0),
                        ),
                        address: header.signer,
                        tx: ShadowTxType::Data(DataShadow {
                            fee: irys_primitives::U256::from(
                                header.term_fee + header.perm_fee.unwrap_or(0),
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

            db.update_eyre(|tx| irys_database::insert_block_header(tx, &irys_block))
                .unwrap();

            let block = Arc::new(irys_block);
            block_discovery_addr.do_send(BlockDiscoveredMessage(block.clone()));

            // Get all the transactions for the previous block, error if not found
            // TODO: Eventually abstract this for support of `n` ledgers
            let previous_submit_txs = get_ledger_tx_headers(&prev_block_header, Ledger::Submit, &db);
            let previous_publish_txs = get_ledger_tx_headers(&prev_block_header, Ledger::Publish, &db);

            let prev_all_txs = {
                let mut combined = previous_submit_txs.unwrap_or_default();
                combined.extend(previous_publish_txs.unwrap_or_default());
                combined
            };

            if is_difficulty_updated {
                mining_broadcaster_addr.do_send(BroadcastDifficultyUpdate(block.clone()));
            }

            let _ = chunk_migration_addr.send(BlockFinalizedMessage {
                block_header: Arc::new(prev_block_header),
                all_txs: Arc::new(prev_all_txs),
            }).await.unwrap();
            info!("Finished producing block height: {}, hash: {}", &block_height, &block_hash);

            Some((block.clone(), exec_payload))
        }
        .into_actor(self),
        ))
    }
}

fn get_ledger_tx_headers(
    block_header: &IrysBlockHeader,
    ledger: Ledger,
    db: &DatabaseProvider,
) -> Option<Vec<IrysTransactionHeader>> {
    let tx = db
        .tx()
        .map_err(|e| {
            error!("Failed to create transaction: {}", e);
        })
        .ok()?;

    match block_header.ledgers[ledger]
        .txids
        .iter()
        .map(|txid| {
            tx_header_by_txid(&tx, txid)
                .map_err(|e| eyre::eyre!("Failed to get tx header: {}", e))
                .and_then(|opt| {
                    opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                })
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(txs) => Some(txs),
        Err(e) => {
            error!(
                "Failed to collect tx headers for {:?} ledger: {}",
                ledger, e
            );
            None
        }
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

impl Handler<BlockConfirmedMessage> for BlockProducerActor {
    type Result = ();
    fn handle(&mut self, msg: BlockConfirmedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Access the block header through msg.0
        let block = &msg.0;
        let all_txs = &msg.1;

        // Do something with the block
        info!("Block height: {} num tx: {}", block.height, all_txs.len());
    }
}

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
