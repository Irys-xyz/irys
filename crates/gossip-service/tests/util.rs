use actix::{Actor, Handler, Context};
use irys_actors::mempool_service::{ChunkIngressMessage, TxIngressMessage};

pub struct MempoolStub {
    pub txs: Vec<TxIngressMessage>,
    pub chunks: Vec<ChunkIngressMessage>
}

impl MempoolStub {
    pub fn new() -> Self {
        Self {
            txs: Vec::new(),
            chunks: Vec::new()
        }
    }
}

impl Actor for MempoolStub {
    type Context = Context<Self>;
}

impl Handler<TxIngressMessage> for MempoolStub {
    type Result = ();

    fn handle(&mut self, msg: TxIngressMessage, _: &mut Self::Context) {
        self.txs.push(msg);
    }
}

impl Handler<ChunkIngressMessage> for MempoolStub {
    type Result = ();

    fn handle(&mut self, msg: ChunkIngressMessage, _: &mut Self::Context) {
        self.chunks.push(msg);
    }
}