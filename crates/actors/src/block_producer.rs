use std::{
    sync::{atomic::AtomicU64, Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use actix::{Actor, Addr, Context, Handler, ResponseFuture};
use irys_types::{block_production::SolutionContext, IrysBlockHeader};
use reth_db::DatabaseEnv;

use crate::mempool::{GetBestMempoolTxs, MempoolActor};

pub struct BlockProducerActor {
    pub db: Arc<DatabaseEnv>,
    pub mempool_addr: Addr<MempoolActor>,
    pub last_height: Arc<RwLock<u64>>,
}

impl BlockProducerActor {
    pub fn new(db: Arc<DatabaseEnv>, mempool_addr: Addr<MempoolActor>) -> Self {
        Self {
            last_height: Arc::new(RwLock::new(get_latest_height_from_db(&db))),
            db,
            mempool_addr,
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
    type Result = ResponseFuture<()>;

    fn handle(&mut self, msg: SolutionContext, ctx: &mut Self::Context) -> Self::Result {
        let mempool_addr = self.mempool_addr.clone();
        let current_height = *self.last_height.read().unwrap();
        let arc_rwlock = self.last_height.clone();
        Box::pin(async move {
            // Acquire lock and check that the height hasn't changed identifying a race condition
            let mut write_current_height = arc_rwlock.write().unwrap();
            if current_height != *write_current_height {
                return ();
            };

            let r = mempool_addr.send(GetBestMempoolTxs).await.unwrap();

            let final_block = IrysBlockHeader::new();
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

            *write_current_height += 1;
            ()
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
