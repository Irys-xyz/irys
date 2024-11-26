use std::{
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix::prelude::*;
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV1Irys, PayloadAttributes};
use irys_primitives::{DataShadow, IrysTxId, ShadowTx, ShadowTxType, Shadows};
use irys_reth_node_bridge::{adapter::node::RethNodeContext, node::RethNodeProvider};
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, Address, Base64, H256List,
    IrysBlockHeader, IrysSignature, IrysTransactionHeader, PoaData, Signature, TransactionLedger,
    H256, U256,
};
use reth::revm::primitives::B256;
use tracing::info;

use crate::{
    block_index::{BlockIndexActor, GetBlockHeightMessage},
    mempool::{GetBestMempoolTxs, MempoolActor},
};

pub struct BlockProducerActor {
    pub db: DatabaseProvider,
    pub mempool_addr: Addr<MempoolActor>,
    pub block_index_addr: Addr<BlockIndexActor>,
    pub reth_provider: RethNodeProvider,
}

impl BlockProducerActor {
    pub fn new(
        db: DatabaseProvider,
        mempool_addr: Addr<MempoolActor>,
        block_index_addr: Addr<BlockIndexActor>,
        reth_provider: RethNodeProvider,
    ) -> Self {
        Self {
            db,
            mempool_addr,
            block_index_addr,
            reth_provider,
        }
    }
}

impl Actor for BlockProducerActor {
    type Context = Context<Self>;
}

impl Handler<SolutionContext> for BlockProducerActor {
    type Result =
        AtomicResponse<Self, Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>>;

    fn handle(&mut self, msg: SolutionContext, ctx: &mut Self::Context) -> Self::Result {
        info!("BlockProducerActor solution received {:?}", &msg);

        let mempool_addr = self.mempool_addr.clone();
        let block_index_addr = self.block_index_addr.clone();
        info!("After");
        let current_height = 0;
        let reth = self.reth_provider.clone();
        let db = self.db.clone();
        let self_addr = ctx.address();

        return AtomicResponse::new(Box::pin(
            async move {
                // Acquire lock and check that the height hasn't changed identifying a race condition
                // TEMP: This demonstrates how to get the block height from the block_index_actor
                let bh = block_index_addr
                    .send(GetBlockHeightMessage {})
                    .await
                    .unwrap();
                info!("block_height: {:?}", bh);

                let data_txs: Vec<IrysTransactionHeader> =
                    mempool_addr.send(GetBestMempoolTxs).await.unwrap();

                let data_tx_ids = data_txs.iter().map(|h| h.id.clone()).collect::<Vec<H256>>();
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

                let mut irys_block = IrysBlockHeader {
                    diff: U256::from(1000),
                    cumulative_diff: U256::from(5000),
                    last_retarget: 1622543200,
                    solution_hash: H256::zero(),
                    previous_solution_hash: H256::zero(),
                    last_epoch_hash: H256::random(),
                    chunk_hash: H256::zero(),
                    height: bh,
                    block_hash: H256::zero(),
                    previous_block_hash: H256::zero(),
                    previous_cumulative_diff: U256::from(4000),
                    poa: PoaData {
                        tx_path: Base64::from_str("").unwrap(),
                        data_path: Base64::from_str("").unwrap(),
                        chunk: Base64::from_str("").unwrap(),
                    },
                    reward_address: Address::ZERO,
                    reward_key: Base64::from_str("").unwrap(),
                    signature: IrysSignature {
                        reth_signature: Signature::test_signature(),
                    },
                    timestamp: now.as_millis() as u64,
                    ledgers: vec![
                        // Permanent Publish Ledger
                        TransactionLedger {
                            tx_root: TransactionLedger::merklize_tx_root(&data_txs).0,
                            txids: H256List(data_tx_ids.clone()),
                            max_chunk_offset: 0,
                            expires: None,
                        },
                        // Term Submit Ledger
                        TransactionLedger {
                            tx_root: TransactionLedger::merklize_tx_root(&vec![]).0,
                            txids: H256List::new(),
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

                irys_database::insert_block(&db, &irys_block).unwrap();

                let block = Arc::new(irys_block);
                let txs = Arc::new(data_txs);
                let block_confirm_message =
                    BlockConfirmedMessage(Arc::clone(&block), Arc::clone(&txs));

                // We can clone messages because it only contains references to the data
                self_addr.do_send(block_confirm_message.clone());
                block_index_addr.do_send(block_confirm_message.clone());
                mempool_addr.do_send(block_confirm_message.clone());

                Some((block.clone(), exec_payload))
            }
            .into_actor(self),
        ));
    }
}

fn get_current_block_height() -> u64 {
    0
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

/// Currently identical to [BlockConfirmedMessage] and acts as a placeholder
/// for when the node will maintain a block tree of confirmed blocks and
/// produce finalized blocks for the canonical chain when enough confirmations
/// have occurred. Chunks are moved from the in-memory index to the storage
/// modules when a block is finalized.
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BlockFinalizedMessage(
    pub Arc<IrysBlockHeader>,
    pub Arc<Vec<IrysTransactionHeader>>,
);
