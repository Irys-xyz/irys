use std::sync::{Arc, RwLock};
use actix::{Actor, Handler, Context};
use irys_actors::mempool_service::{ChunkIngressError, ChunkIngressMessage, TxIngressError, TxIngressMessage};

#[derive(Debug)]
pub struct MempoolStub {
    pub txs: Arc<RwLock<Vec<TxIngressMessage>>>,
    pub chunks: Arc<RwLock<Vec<ChunkIngressMessage>>>
}

impl MempoolStub {
    pub fn new() -> Self {
        Self {
            txs: Default::default(),
            chunks: Default::default()
        }
    }
}

impl Actor for MempoolStub {
    type Context = Context<Self>;
}

impl Handler<TxIngressMessage> for MempoolStub {
    type Result = Result<(), TxIngressError>;

    fn handle(&mut self, msg: TxIngressMessage, _: &mut Self::Context) -> Self::Result {
        self.txs.write().unwrap().push(msg);

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolStub {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, msg: ChunkIngressMessage, _: &mut Self::Context) -> Self::Result {
        self.chunks.write().unwrap().push(msg);

        Ok(())
    }
}