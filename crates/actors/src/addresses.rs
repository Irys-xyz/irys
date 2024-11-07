use actix::Addr;

use crate::{
    block_producer::BlockProducerActor, mempool::MempoolActor, mining::PartitionMiningActor,
    packing::PackingActor,
};

/// Serves as a kind of app state that can be passed into actix web to allow
/// the webserver to interact with actors in the node context.
#[derive(Debug, Clone)]
pub struct ActorAddresses {
    pub partitions: Vec<Addr<PartitionMiningActor>>,
    pub block_producer: Addr<BlockProducerActor>,
    pub packing: Addr<PackingActor>,
    pub mempool: Addr<MempoolActor>,
}
