use actix::Addr;

use crate::{
    block_discovery::BlockDiscoveryActor, block_index_service::BlockIndexService,
    block_producer::BlockProducerActor, mempool_service::MempoolService,
    mining::PartitionMiningActor, packing::PackingActor, reth_service::RethServiceActor,
    EpochServiceActor,
};

/// Serves as a kind of app state that can be passed into actix web to allow
/// the webserver to interact with actors in the node context.
#[derive(Debug, Clone)]
pub struct ActorAddresses {
    pub partitions: Vec<Addr<PartitionMiningActor>>,
    pub block_discovery_addr: Addr<BlockDiscoveryActor>,
    pub block_producer: Addr<BlockProducerActor>,
    pub packing: Addr<PackingActor>,
    pub block_index: Addr<BlockIndexService>,
    pub epoch_service: Addr<EpochServiceActor>,
    pub reth: Addr<RethServiceActor>,
}
