use crate::chunk_ingress_service::ChunkIngressMessage;
use crate::mining_bus::{MiningBroadcastEvent, MiningBus};
use crate::{
    block_discovery::BlockDiscoveryMessage,
    block_producer::BlockProducerCommand,
    block_tree_service::{BlockStateUpdated, BlockTreeServiceMessage, ReorgEvent},
    cache_service::CacheServiceAction,
    chunk_migration_service::ChunkMigrationServiceMessage,
    mempool_service::MempoolServiceMessage,
    packing_service::{PackingRequest, PackingSender, PackingService},
    reth_service::RethServiceMessage,
    validation_service::ValidationServiceMessage,
    DataSyncServiceMessage, StorageModuleServiceMessage,
};
use core::ops::Deref;
use irys_domain::PeerEvent;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{PeerNetworkSender, PeerNetworkServiceMessage, Traced};
use irys_vdf::VdfStep;
use std::sync::Arc;
use tokio::sync::{
    broadcast,
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};

#[derive(Debug, Clone)]
pub struct ServiceSenders(pub Arc<ServiceSendersInner>);

impl Deref for ServiceSenders {
    type Target = Arc<ServiceSendersInner>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ServiceSenders {
    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.0.subscribe_reorgs()
    }

    pub fn subscribe_block_state_updates(&self) -> broadcast::Receiver<BlockStateUpdated> {
        self.0.block_state_events.subscribe()
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.0.peer_events.subscribe()
    }

    pub fn new() -> (Self, ServiceReceivers) {
        let (senders, receivers) = ServiceSendersInner::init();
        (Self(Arc::new(senders)), receivers)
    }

    pub fn packing_sender(&self) -> PackingSender {
        self.0.packing_sender.clone()
    }

    pub fn mining_bus(&self) -> MiningBus {
        self.0.mining_bus.clone()
    }

    pub fn subscribe_mining_broadcast(&self) -> UnboundedReceiver<Arc<MiningBroadcastEvent>> {
        self.0.subscribe_mining_broadcast()
    }

    pub fn send_mining_seed(
        &self,
        seed: irys_types::block_production::Seed,
        checkpoints: irys_types::H256List,
        global_step: u64,
    ) {
        let _ = self.0.mining_bus.send_seed(seed, checkpoints, global_step);
    }

    pub fn send_mining_difficulty(&self, msg: crate::mining_bus::BroadcastDifficultyUpdate) {
        let _ = self.0.mining_bus.send_difficulty(msg);
    }

    pub fn send_partitions_expiration(
        &self,
        msg: crate::mining_bus::BroadcastPartitionsExpiration,
    ) {
        let _ = self.0.mining_bus.send_partitions_expiration(msg);
    }
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: UnboundedReceiver<Traced<CacheServiceAction>>,
    pub chunk_ingress: UnboundedReceiver<Traced<ChunkIngressMessage>>,
    pub chunk_migration: UnboundedReceiver<Traced<ChunkMigrationServiceMessage>>,
    pub mempool: UnboundedReceiver<Traced<MempoolServiceMessage>>,
    pub vdf_fast_forward: UnboundedReceiver<Traced<VdfStep>>,
    pub storage_modules: UnboundedReceiver<Traced<StorageModuleServiceMessage>>,
    pub data_sync: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
    pub gossip_broadcast: UnboundedReceiver<Traced<GossipBroadcastMessageV2>>,
    pub block_tree: UnboundedReceiver<Traced<BlockTreeServiceMessage>>,
    pub validation_service: UnboundedReceiver<Traced<ValidationServiceMessage>>,
    pub block_producer: UnboundedReceiver<Traced<BlockProducerCommand>>,
    pub reth_service: UnboundedReceiver<Traced<RethServiceMessage>>,
    pub reorg_events: broadcast::Receiver<ReorgEvent>,
    pub block_state_events: broadcast::Receiver<BlockStateUpdated>,
    pub peer_events: broadcast::Receiver<PeerEvent>,
    pub peer_network: UnboundedReceiver<PeerNetworkServiceMessage>,
    pub block_discovery: UnboundedReceiver<Traced<BlockDiscoveryMessage>>,
    pub packing: tokio::sync::mpsc::Receiver<PackingRequest>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<Traced<CacheServiceAction>>,
    pub chunk_ingress: UnboundedSender<Traced<ChunkIngressMessage>>,
    pub chunk_migration: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
    pub mempool: UnboundedSender<Traced<MempoolServiceMessage>>,
    pub vdf_fast_forward: UnboundedSender<Traced<VdfStep>>,
    pub storage_modules: UnboundedSender<Traced<StorageModuleServiceMessage>>,
    pub data_sync: UnboundedSender<Traced<DataSyncServiceMessage>>,
    pub gossip_broadcast: UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    pub block_tree: UnboundedSender<Traced<BlockTreeServiceMessage>>,
    pub validation_service: UnboundedSender<Traced<ValidationServiceMessage>>,
    pub block_producer: UnboundedSender<Traced<BlockProducerCommand>>,
    pub reth_service: UnboundedSender<Traced<RethServiceMessage>>,
    pub reorg_events: broadcast::Sender<ReorgEvent>,
    pub block_state_events: broadcast::Sender<BlockStateUpdated>,
    pub peer_events: broadcast::Sender<PeerEvent>,
    pub peer_network: PeerNetworkSender,
    pub block_discovery: UnboundedSender<Traced<BlockDiscoveryMessage>>,
    pub mining_bus: MiningBus,
    pub packing_sender: PackingSender,
}

impl ServiceSendersInner {
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) =
            unbounded_channel::<Traced<CacheServiceAction>>();
        let (chunk_ingress_sender, chunk_ingress_receiver) =
            unbounded_channel::<Traced<ChunkIngressMessage>>();
        let (chunk_migration_sender, chunk_migration_receiver) =
            unbounded_channel::<Traced<ChunkMigrationServiceMessage>>();
        let (mempool_sender, mempool_receiver) =
            unbounded_channel::<Traced<MempoolServiceMessage>>();
        let (vdf_fast_forward_sender, vdf_fast_forward_receiver) =
            unbounded_channel::<Traced<VdfStep>>();
        let (sm_sender, sm_receiver) = unbounded_channel::<Traced<StorageModuleServiceMessage>>();
        let (ds_sender, ds_receiver) = unbounded_channel::<Traced<DataSyncServiceMessage>>();
        let (gossip_broadcast_sender, gossip_broadcast_receiver) =
            unbounded_channel::<Traced<GossipBroadcastMessageV2>>();
        let (block_tree_sender, block_tree_receiver) =
            unbounded_channel::<Traced<BlockTreeServiceMessage>>();
        let (validation_sender, validation_receiver) =
            unbounded_channel::<Traced<ValidationServiceMessage>>();
        let (block_producer_sender, block_producer_receiver) =
            unbounded_channel::<Traced<BlockProducerCommand>>();
        let (reth_service_sender, reth_service_receiver) =
            unbounded_channel::<Traced<RethServiceMessage>>();
        let (reorg_sender, reorg_receiver) = broadcast::channel::<ReorgEvent>(100);
        let (block_state_sender, block_state_receiver) =
            broadcast::channel::<BlockStateUpdated>(100);
        let (peer_events_sender, peer_events_receiver) = broadcast::channel::<PeerEvent>(100);
        let (peer_network_sender, peer_network_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (block_discovery_sender, block_discovery_receiver) =
            unbounded_channel::<Traced<BlockDiscoveryMessage>>();
        let (packing_sender, packing_receiver) = PackingService::channel(5_000);

        let mining_bus = MiningBus::new();
        let senders = Self {
            chunk_cache: chunk_cache_sender,
            chunk_ingress: chunk_ingress_sender,
            chunk_migration: chunk_migration_sender,
            mempool: mempool_sender,
            vdf_fast_forward: vdf_fast_forward_sender,
            storage_modules: sm_sender,
            data_sync: ds_sender,
            gossip_broadcast: gossip_broadcast_sender,
            block_tree: block_tree_sender,
            validation_service: validation_sender,
            block_producer: block_producer_sender,
            reth_service: reth_service_sender,
            reorg_events: reorg_sender,
            block_state_events: block_state_sender,
            peer_events: peer_events_sender,
            peer_network: PeerNetworkSender::new(peer_network_sender),
            block_discovery: block_discovery_sender,
            mining_bus,
            packing_sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            chunk_ingress: chunk_ingress_receiver,
            chunk_migration: chunk_migration_receiver,
            mempool: mempool_receiver,
            vdf_fast_forward: vdf_fast_forward_receiver,
            storage_modules: sm_receiver,
            data_sync: ds_receiver,
            gossip_broadcast: gossip_broadcast_receiver,
            block_tree: block_tree_receiver,
            validation_service: validation_receiver,
            block_producer: block_producer_receiver,
            reth_service: reth_service_receiver,
            reorg_events: reorg_receiver,
            block_state_events: block_state_receiver,
            peer_events: peer_events_receiver,
            peer_network: peer_network_receiver,
            block_discovery: block_discovery_receiver,
            packing: packing_receiver,
        };
        (senders, receivers)
    }

    /// Subscribe to reorg events
    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.reorg_events.subscribe()
    }

    pub fn subscribe_mining_broadcast(&self) -> UnboundedReceiver<Arc<MiningBroadcastEvent>> {
        self.mining_bus.subscribe()
    }
}
