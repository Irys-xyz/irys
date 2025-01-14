use actix::Addr;

use crate::{
    block_index_service::BlockIndexService,
    block_producer::BlockProducerActor,
    epoch_service::EpochServiceActor,
    mempool::MempoolActor,
    mining::{MiningControl, PartitionMiningActor},
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
    pub block_index: Addr<BlockIndexService>,
    pub epoch_service: Addr<EpochServiceActor>,
}

impl ActorAddresses {
    /// Send a message to all known partition actors to ignore any received VDF steps
    pub fn stop_mining(&self) -> eyre::Result<()> {
        self.set_mining(false)
    }
    /// Send a message to all known partition actors to begin mining when they receive a VDF step
    pub fn start_mining(&self) -> eyre::Result<()> {
        self.set_mining(true)
    }
    /// Send a custom control message to all known partition actors
    pub fn set_mining(&self, should_mine: bool) -> eyre::Result<()> {
        for part in &self.partitions {
            part.try_send(MiningControl(should_mine))?;
        }
        Ok(())
    }
}
