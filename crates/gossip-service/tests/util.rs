use actix::{Actor, Handler, Context};
use irys_actors::mempool_service::{ChunkIngressError, ChunkIngressMessage, TxIngressError, TxIngressMessage};

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
    type Result = Result<(), TxIngressError>;

    fn handle(&mut self, msg: TxIngressMessage, _: &mut Self::Context) -> Self::Result {
        self.txs.push(msg);

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolStub {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, msg: ChunkIngressMessage, _: &mut Self::Context) -> Self::Result {
        self.chunks.push(msg);

        Ok(())
    }
}