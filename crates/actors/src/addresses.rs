use actix::Addr;

use crate::{mining::PartitionMiningActor, packing::PackingActor, reth_service::RethServiceActor};

/// Serves as a kind of app state that can be passed into actix web to allow
/// the webserver to interact with actors in the node context.
#[derive(Debug, Clone)]
pub struct ActorAddresses {
    pub partitions: Vec<Addr<PartitionMiningActor>>,
    pub packing: Addr<PackingActor>,
    pub reth: Addr<RethServiceActor>,
}
