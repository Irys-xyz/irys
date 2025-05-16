use actix::Addr;

use crate::{
    block_discovery::BlockDiscoveryActor,
    block_index_service::BlockIndexService,
    block_producer::BlockProducerActor,
    mempool_service::MempoolService,
    mining::{MiningControl, PartitionMiningActor},
    packing::PackingActor,
    reth_service::RethServiceActor,
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
    pub mempool: Addr<MempoolService>,
    pub block_index: Addr<BlockIndexService>,
    pub epoch_service: Addr<EpochServiceActor>,
    pub reth: Addr<RethServiceActor>,
}

impl ActorAddresses {
    /// Send a message to all known partition actors to ignore any received VDF steps
    pub fn stop_mining(
        &self,
        vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
    ) -> eyre::Result<()> {
        // pause VDF thread mining
        // TODO: switch fn to async?
        tokio::spawn(async move {
            let _ = vdf_mining_state_sender.send(false).await;
        });
        self.set_mining(false)
    }
    /// Send a message to all known partition actors to begin mining when they receive a VDF step
    pub fn start_mining(
        &self,
        vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
    ) -> eyre::Result<()> {
        // start VDF thread mining
        // TODO: switch fn to async?
        tokio::spawn(async move {
            let _ = vdf_mining_state_sender.send(true).await;
        });
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
