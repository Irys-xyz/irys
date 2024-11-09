use std::{
    sync::{atomic::AtomicU64, Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use actix::{Actor, Addr, AsyncContext, Context, Handler, Message, ResponseFuture};
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV1Irys, PayloadAttributes};
use irys_primitives::{ShadowTx, Shadows};
// use irys_primitives::PayloadAttributes;
use irys_reth_node_bridge::{adapter::node::RethNodeContext, node::RethNodeProvider};
use irys_types::{
    app_state::DatabaseProvider, block_production::SolutionContext, Address, IrysBlockHeader,
};
use reth::{payload::EthPayloadBuilderAttributes, primitives::SealedBlock, revm::primitives::B256};
use reth_db::DatabaseEnv;

use crate::mempool::{GetBestMempoolTxs, MempoolActor, RemoveConfirmedTxs};
use irys_primitives::*;

pub struct BlockProducerActor {
    pub db: DatabaseProvider,
    pub mempool_addr: Addr<MempoolActor>,
    pub last_height: Arc<RwLock<u64>>,
    pub reth_provider: RethNodeProvider,
}

impl BlockProducerActor {
    pub fn new(
        db: DatabaseProvider,
        mempool_addr: Addr<MempoolActor>,
        reth_provider: RethNodeProvider,
    ) -> Self {
        Self {
            last_height: Arc::new(RwLock::new(get_latest_height_from_db(&db))),
            db,
            mempool_addr,
            reth_provider,
        }
    }
}

// TODO: Implement real query
fn get_latest_height_from_db(db: &DatabaseEnv) -> u64 {
    0
}

impl Actor for BlockProducerActor {
    type Context = Context<Self>;
}

impl Handler<SolutionContext> for BlockProducerActor {
    type Result = ResponseFuture<Option<(IrysBlockHeader, ExecutionPayloadEnvelopeV1Irys)>>;

    fn handle(&mut self, msg: SolutionContext, ctx: &mut Self::Context) -> Self::Result {
        let mempool_addr = self.mempool_addr.clone();
        let current_height = *self.last_height.read().unwrap();
        let arc_rwlock = self.last_height.clone();
        let reth = self.reth_provider.clone();
        let db = self.db.clone();
        let self_addr = ctx.address();

        Box::pin(async move {
            // Acquire lock and check that the height hasn't changed identifying a race condition
            let mut write_current_height = arc_rwlock.write().unwrap();
            if current_height != *write_current_height {
                return None;
            };

            let data_txs: Vec<irys_types::IrysTransactionHeader> =
                mempool_addr.send(GetBestMempoolTxs).await.unwrap();

            let mut irys_block = IrysBlockHeader::new();

            // RethNodeContext is a type-aware wrapper that lets us interact with the reth node
            let context = RethNodeContext::new(reth.into()).await.unwrap();

            let data_tx_ids = data_txs.iter().map(|h| h.id.clone()).collect();

            let shadows = Shadows::new(
                data_txs
                    .iter()
                    .map(|header| ShadowTx {
                        tx_id: IrysTxId::from_slice(header.id.as_bytes()),
                        fee: U256::from(header.term_fee + header.perm_fee.unwrap_or(0)),
                        address: header.signer,
                        tx: ShadowTxType::Data(DataShadow {
                            fee: U256::from(header.term_fee + header.perm_fee.unwrap_or(0)),
                        }),
                    })
                    .collect(),
            );

            // create a new reth payload

            // generate payload attributes
            // TODO: we need previous block metadata to fill in parent & prev_randao
            let payload_attrs = PayloadAttributes {
                timestamp: irys_block.timestamp, // tie timestamp together
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

            // TODO: Commit block

            let _ = mempool_addr
                .send(RemoveConfirmedTxs(data_tx_ids))
                .await
                .unwrap();

            // let final_block = IrysBlockHeader {
            //     block_hash: todo!(),
            //     diff: todo!(),
            //     cumulative_diff: todo!(),
            //     last_retarget: todo!(),
            //     solution_hash: todo!(),
            //     previous_solution_hash: todo!(),
            //     chunk_hash: todo!(),
            //     height: get_current_block_height() + 1,
            //     previous_block_hash: todo!(),
            //     previous_cumulative_diff: todo!(),
            //     poa: todo!(),
            //     reward_address: todo!(),
            //     reward_key: todo!(),
            //     signature: todo!(),
            //     timestamp: get_current_timestamp(),
            //     ledgers: todo!(),
            // };

            // TODO: Commit block to DB and send to networking layer

            // make the built evm payload canonical
            context
                .engine_api
                .update_forkchoice(v1_payload.parent_hash, v1_payload.block_hash)
                .await
                .unwrap();

            database::insert_block(&db, &irys_block).unwrap();

            self_addr.do_send(BlockConfirmed());

            *write_current_height += 1;
            Some((irys_block, exec_payload))
        })
    }
}

fn get_current_timestamp() -> u64 {
    let start = SystemTime::now();
    start.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn get_current_block_height() -> u64 {
    0
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct BlockConfirmed();

impl Handler<BlockConfirmed> for BlockProducerActor {
    type Result = ();

    fn handle(&mut self, msg: BlockConfirmed, ctx: &mut Self::Context) -> Self::Result {
        // TODO: Likely want to do several actions upon a block being confirmed such as update indexes
        ()
    }
}
