use std::{
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix::prelude::*;
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV1Irys, PayloadAttributes};
use irys_database::{block_header_by_hash, tx_header_by_txid, Ledger};
use irys_primitives::{DataShadow, IrysTxId, ShadowTx, ShadowTxType, Shadows};
use irys_reth_node_bridge::{adapter::node::RethNodeContext, node::RethNodeProvider};
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, Address, Base64, H256List,
    IrysBlockHeader, IrysSignature, IrysTransactionHeader, PoaData, Signature, TransactionLedger,
    H256, U256,
};
use openssl::sha;
use reth::revm::primitives::B256;
use reth_db::Database;
use tracing::{error, info};

use crate::{
    block_index::{BlockIndexActor, GetLatestBlockIndexMessage},
    chunk_storage::ChunkStorageActor,
    epoch_service::{EpochServiceActor, GetPartitionAssignmentMessage},
    mempool::{GetBestMempoolTxs, MempoolActor},
};

/// BlockProducerActor creates blocks from mining solutions
#[derive(Debug)]
pub struct BlockProducerActor {
    /// Reference to the global database
    pub db: DatabaseProvider,
    /// Address of the mempool actor
    pub mempool_addr: Addr<MempoolActor>,
    /// Address of the chunk storage actor
    pub chunk_storage_addr: Addr<ChunkStorageActor>,
    /// Address of the bock_index actor
    pub block_index_addr: Addr<BlockIndexActor>,
    /// Tracks the global state of partition assignments on the protocol
    pub epoch_service: Addr<EpochServiceActor>,
    /// Reference to the VM node
    pub reth_provider: RethNodeProvider,
}

impl BlockProducerActor {
    /// Initializes a new BlockProducerActor
    pub fn new(
        db: DatabaseProvider,
        mempool_addr: Addr<MempoolActor>,
        chunk_storage_addr: Addr<ChunkStorageActor>,
        block_index_addr: Addr<BlockIndexActor>,
        epoch_service: Addr<EpochServiceActor>,
        reth_provider: RethNodeProvider,
    ) -> Self {
        Self {
            db,
            mempool_addr,
            chunk_storage_addr,
            block_index_addr,
            epoch_service,
            reth_provider,
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

    fn handle(&mut self, msg: SolutionFoundMessage, ctx: &mut Self::Context) -> Self::Result {
        let solution = msg.0;
        // info!("BlockProducerActor solution received {:?}", &solution);

        let mempool_addr = self.mempool_addr.clone();
        let block_index_addr = self.block_index_addr.clone();

        let epoch_service_addr = self.epoch_service.clone();

        let chunk_storage_addr = self.chunk_storage_addr.clone();
        info!("After");

        let reth = self.reth_provider.clone();
        let db = self.db.clone();
        let self_addr = ctx.address();

        AtomicResponse::new(Box::pin(
            async move {
                // Acquire lock and check that the height hasn't changed identifying a race condition
                // TEMP: This demonstrates how to get the block height from the block_index_actor
                let latest_block = block_index_addr
                    .send(GetLatestBlockIndexMessage {})
                    .await
                    .unwrap();

                let prev_block_header: Option<IrysBlockHeader>;
                let prev_block_hash: H256;

                if let Some(block_item) = latest_block {
                    prev_block_hash = block_item.block_hash;
                    prev_block_header = db
                        .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash))
                        .unwrap();
                } else {
                    prev_block_hash = H256::default();
                    prev_block_header = None;
                }

                // Retrieve all the transaction headers for the previous block
                let prev_block_header = match prev_block_header {
                    Some(header) => header,
                    None => {
                        error!("No previous block header found");
                        return None;
                    }
                };

                // Translate partition hash, chunk offset -> ledger, ledger chunk offset
                let ledger_num = epoch_service_addr
                    .send(GetPartitionAssignmentMessage(solution.partition_hash))
                    .await
                    .unwrap()
                    .map(|pa| pa.ledger_num)
                    .flatten();

                let data_txs: Vec<IrysTransactionHeader> =
                    mempool_addr.send(GetBestMempoolTxs).await.unwrap();

                let data_tx_ids = data_txs.iter().map(|h| h.id.clone()).collect::<Vec<H256>>();
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

                // TODO: Hash the block signature to create a block_hash
                // Generate a very stupid block_hash right now which is just
                // the hash of the timestamp
                let timestamp = now.as_millis() as u64;
                let block_hash = hash_sha256(&timestamp.to_le_bytes());

                let mut irys_block = IrysBlockHeader {
                    block_hash,
                    height: prev_block_header.height + 1,
                    diff: prev_block_header.diff,
                    cumulative_diff: U256::from(5000),
                    last_retarget: 1622543200,
                    solution_hash: H256::zero(),
                    previous_solution_hash: H256::zero(),
                    last_epoch_hash: H256::random(),
                    chunk_hash: H256::zero(),
                    previous_block_hash: prev_block_hash,
                    previous_cumulative_diff: U256::from(4000),
                    poa: PoaData {
                        tx_path: solution.tx_path.map(|tx_path| Base64(tx_path)),
                        data_path: solution.data_path.map(|data_path| Base64(data_path)),
                        chunk: Base64(solution.chunk),
                        ledger_num,
                        partition_chunk_offset: solution.chunk_offset as u64,
                        partition_hash: solution.partition_hash,
                    },
                    reward_address: Address::ZERO,
                    reward_key: Base64::from_str("").unwrap(),
                    signature: IrysSignature {
                        reth_signature: Signature::test_signature(),
                    },
                    timestamp,
                    ledgers: vec![
                        // Permanent Publish Ledger
                        TransactionLedger {
                            tx_root: H256::zero(),
                            txids: H256List::new(),
                            max_chunk_offset: 0,
                            expires: None,
                        },
                        // Term Submit Ledger
                        TransactionLedger {
                            tx_root: TransactionLedger::merklize_tx_root(&data_txs).0,
                            txids: H256List(data_tx_ids.clone()),
                            max_chunk_offset: 0,
                            expires: Some(1622543200),
                        },
                    ],
                    evm_block_hash: B256::ZERO,
                };

                // RethNodeContext is a type-aware wrapper that lets us interact with the reth node
                let context = RethNodeContext::new(reth.into()).await.unwrap();

                let shadows = Shadows::new(
                    data_txs
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
                let txs = Arc::new(data_txs);
                let block_confirm_message =
                    BlockConfirmedMessage(Arc::clone(&block), Arc::clone(&txs));

                // Broadcast BLockConfirmedMessage
                self_addr.do_send(block_confirm_message.clone());
                block_index_addr.do_send(block_confirm_message.clone());
                mempool_addr.do_send(block_confirm_message.clone());

                // Get all the transactions for the previous block, error if not found
                let txs = match prev_block_header.ledgers[Ledger::Submit]
                    .txids
                    .iter()
                    .map(|txid| {
                        db.view_eyre(|tx| tx_header_by_txid(tx, txid))
                            .and_then(|opt| {
                                opt.ok_or_else(|| {
                                    eyre::eyre!("No tx header found for txid {}", txid)
                                })
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(txs) => txs,
                    Err(e) => {
                        error!("Failed to collect tx headers: {}", e);
                        return None;
                    }
                };

                chunk_storage_addr.do_send(BlockFinalizedMessage {
                    block_header: Arc::new(prev_block_header),
                    txs: Arc::new(txs),
                });

                Some((block.clone(), exec_payload))
            }
            .into_actor(self),
        ))
    }
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
        let data_tx = &msg.1;

        // Do something with the block
        info!("Block height: {} num tx: {}", block.height, data_tx.len());
    }
}

/// Similar to [BlockConfirmedMessage] (but takes ownership of parameters) and
/// acts as a placeholder for when the node will maintain a block tree of
/// confirmed blocks and produce finalized blocks for the canonical chain when
///  enough confirmations have occurred. Chunks are moved from the in-memory
/// index to the storage modules when a block is finalized.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), ()>")]
pub struct BlockFinalizedMessage {
    pub block_header: Arc<IrysBlockHeader>,
    pub txs: Arc<Vec<IrysTransactionHeader>>,
}

/// SHA256 hash the message parameter
fn hash_sha256(message: &[u8]) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    H256::from(hasher.finish())
}
