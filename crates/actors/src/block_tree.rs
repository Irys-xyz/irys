use crate::{
    block_discovery::BlockPreValidatedMessage,
    block_index::BlockIndexActor,
    block_producer::{BlockConfirmedMessage, BlockProducerActor, RegisterBlockProducerMessage},
    mempool::MempoolActor,
};
use actix::prelude::*;

/// BlockDiscoveryActor listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockTreeActor {
    /// Shared access to the block index used for validation.
    pub block_index: Addr<BlockIndexActor>,
    /// Reference to the mempool actor, which maintains the validity of pending transactions.
    pub mempool: Addr<MempoolActor>,
    /// Needs to know the current block to build on
    block_producer: Option<Addr<BlockProducerActor>>,
}

impl Actor for BlockTreeActor {
    type Context = Context<Self>;
}

impl BlockTreeActor {
    /// Initializes a BlockTreeActor without a block_producer address
    pub fn new(block_index: Addr<BlockIndexActor>, mempool: Addr<MempoolActor>) -> Self {
        Self {
            block_index,
            mempool,
            block_producer: None,
        }
    }
}

impl Handler<RegisterBlockProducerMessage> for BlockTreeActor {
    type Result = ();
    fn handle(
        &mut self,
        msg: RegisterBlockProducerMessage,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.block_producer = Some(msg.0);
    }
}

impl Handler<BlockPreValidatedMessage> for BlockTreeActor {
    type Result = ();
    fn handle(&mut self, msg: BlockPreValidatedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let pre_validated_block = msg.0;
        let txs = msg.1;
        // TODO: Check and see if this block represents a new head for the canonical chain
        // or if it should be added to a fork.

        // For now, because there are no forks, we'll just auto confirm the block
        let block_confirm_message = BlockConfirmedMessage(pre_validated_block, txs);

        self.block_index.do_send(block_confirm_message.clone());
        if let Some(block_producer) = &self.block_producer {
            block_producer.do_send(block_confirm_message.clone());
        }
        self.mempool.do_send(block_confirm_message.clone());

        // TODO: Kick off a full validation process on the confirmed block that checks the VDF steps
        // and other heavy validation tasks, so it can be fully validated before we risk producing
        // a block on top of it.
    }
}
