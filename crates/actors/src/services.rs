use crate::{
    block_discovery::BlockDiscoveryMessage,
    block_index_service::BlockIndexServiceMessage,
    block_producer::BlockProducerCommand,
    block_tree_service::{
        BlockMigratedEvent, BlockStateUpdated, BlockTreeServiceMessage, ReorgEvent,
    },
    cache_service::CacheServiceAction,
    chunk_migration_service::ChunkMigrationServiceMessage,
    mempool_service::MempoolServiceMessage,
    packing::PackingHandle,
    reth_service::RethServiceMessage,
    validation_service::ValidationServiceMessage,
    DataSyncServiceMessage, StorageModuleServiceMessage,
};
use actix::Message;
use core::ops::Deref;
use irys_domain::PeerEvent;
use irys_types::{GossipBroadcastMessage, PeerNetworkSender, PeerNetworkServiceMessage};
use irys_vdf::VdfStep;
use std::sync::{Arc, OnceLock};
use tokio::sync::{
    broadcast,
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};

// Only contains senders, thread-safe to clone and share
#[derive(Debug, Clone)]
pub struct ServiceSenders(pub Arc<ServiceSendersInner>);

impl Deref for ServiceSenders {
    type Target = Arc<ServiceSendersInner>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ServiceSenders {
    // Create both the sender and receiver sides
    #[must_use]
    pub fn new() -> (Self, ServiceReceivers) {
        let (senders, receivers) = ServiceSendersInner::init();
        (Self(Arc::new(senders)), receivers)
    }

    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.0.subscribe_reorgs()
    }

    pub fn subscribe_block_migrated(&self) -> broadcast::Receiver<BlockMigratedEvent> {
        self.0.subscribe_block_migrated()
    }

    pub fn subscribe_block_state_updates(&self) -> broadcast::Receiver<BlockStateUpdated> {
        self.0.block_state_events.subscribe()
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.0.peer_events.subscribe()
    }

    pub fn new_with_packing_handle(handle: PackingHandle) -> (Self, ServiceReceivers) {
        let (senders, receivers) = ServiceSendersInner::init();
        senders
            .packing_handle
            .set(handle)
            .expect("packing_handle already set");
        (Self(Arc::new(senders)), receivers)
    }

    /// Late-initialize the packing handle for cases where ServiceSenders is constructed
    /// before the packing service exists. Subsequent calls are ignored.
    pub fn set_packing_handle(&self, handle: PackingHandle) {
        let _ = self.0.packing_handle.set(handle);
    }

    pub fn packing_handle(&self) -> PackingHandle {
        self.0
            .packing_handle
            .get()
            .expect("packing_handle not set")
            .clone()
    }
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: UnboundedReceiver<CacheServiceAction>,
    pub chunk_migration: UnboundedReceiver<ChunkMigrationServiceMessage>,
    pub mempool: UnboundedReceiver<MempoolServiceMessage>,
    pub vdf_fast_forward: UnboundedReceiver<VdfStep>,
    pub storage_modules: UnboundedReceiver<StorageModuleServiceMessage>,
    pub data_sync: UnboundedReceiver<DataSyncServiceMessage>,
    pub gossip_broadcast: UnboundedReceiver<GossipBroadcastMessage>,
    pub block_tree: UnboundedReceiver<BlockTreeServiceMessage>,
    pub block_index: UnboundedReceiver<BlockIndexServiceMessage>,
    pub validation_service: UnboundedReceiver<ValidationServiceMessage>,
    pub block_producer: UnboundedReceiver<BlockProducerCommand>,
    pub reth_service: UnboundedReceiver<RethServiceMessage>,
    pub reorg_events: broadcast::Receiver<ReorgEvent>,
    pub block_migrated_events: broadcast::Receiver<BlockMigratedEvent>,
    pub block_state_events: broadcast::Receiver<BlockStateUpdated>,
    pub peer_events: broadcast::Receiver<PeerEvent>,
    pub peer_network: UnboundedReceiver<PeerNetworkServiceMessage>,
    pub block_discovery: UnboundedReceiver<BlockDiscoveryMessage>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
    pub chunk_migration: UnboundedSender<ChunkMigrationServiceMessage>,
    pub mempool: UnboundedSender<MempoolServiceMessage>,
    pub vdf_fast_forward: UnboundedSender<VdfStep>,
    pub storage_modules: UnboundedSender<StorageModuleServiceMessage>,
    pub data_sync: UnboundedSender<DataSyncServiceMessage>,
    pub gossip_broadcast: UnboundedSender<GossipBroadcastMessage>,
    pub block_tree: UnboundedSender<BlockTreeServiceMessage>,
    pub block_index: UnboundedSender<BlockIndexServiceMessage>,
    pub validation_service: UnboundedSender<ValidationServiceMessage>,
    pub block_producer: UnboundedSender<BlockProducerCommand>,
    pub reth_service: UnboundedSender<RethServiceMessage>,
    pub reorg_events: broadcast::Sender<ReorgEvent>,
    pub block_migrated_events: broadcast::Sender<BlockMigratedEvent>,
    pub block_state_events: broadcast::Sender<BlockStateUpdated>,
    pub peer_events: broadcast::Sender<PeerEvent>,
    pub peer_network: PeerNetworkSender,
    pub block_discovery: UnboundedSender<BlockDiscoveryMessage>,
    pub packing_handle: OnceLock<PackingHandle>,
}

impl ServiceSendersInner {
    #[must_use]
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) = unbounded_channel::<CacheServiceAction>();
        let (chunk_migration_sender, chunk_migration_receiver) =
            unbounded_channel::<ChunkMigrationServiceMessage>();
        let (mempool_sender, mempool_receiver) = unbounded_channel::<MempoolServiceMessage>();
        // vdf channel for fast forwarding steps during node sync
        let (vdf_fast_forward_sender, vdf_fast_forward_receiver) = unbounded_channel::<VdfStep>();
        let (sm_sender, sm_receiver) = unbounded_channel::<StorageModuleServiceMessage>();
        let (ds_sender, ds_receiver) = unbounded_channel::<DataSyncServiceMessage>();
        let (gossip_broadcast_sender, gossip_broadcast_receiver) =
            unbounded_channel::<GossipBroadcastMessage>();
        let (block_tree_sender, block_tree_receiver) =
            unbounded_channel::<BlockTreeServiceMessage>();
        let (block_index_sender, block_index_receiver) =
            unbounded_channel::<BlockIndexServiceMessage>();
        let (validation_sender, validation_receiver) =
            unbounded_channel::<ValidationServiceMessage>();
        let (block_producer_sender, block_producer_receiver) =
            unbounded_channel::<BlockProducerCommand>();
        let (reth_service_sender, reth_service_receiver) =
            unbounded_channel::<RethServiceMessage>();
        // Create broadcast channel for reorg events
        let (reorg_sender, reorg_receiver) = broadcast::channel::<ReorgEvent>(100);
        let (block_migrated_sender, block_migrated_receiver) =
            broadcast::channel::<BlockMigratedEvent>(100);
        let (block_state_sender, block_state_receiver) =
            broadcast::channel::<BlockStateUpdated>(100);
        let (peer_events_sender, peer_events_receiver) = broadcast::channel::<PeerEvent>(100);
        let (peer_network_sender, peer_network_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (block_discovery_sender, block_discovery_receiver) =
            unbounded_channel::<BlockDiscoveryMessage>();

        let senders = Self {
            chunk_cache: chunk_cache_sender,
            chunk_migration: chunk_migration_sender,
            mempool: mempool_sender,
            vdf_fast_forward: vdf_fast_forward_sender,
            storage_modules: sm_sender,
            data_sync: ds_sender,
            gossip_broadcast: gossip_broadcast_sender,
            block_tree: block_tree_sender,
            block_index: block_index_sender,
            validation_service: validation_sender,
            block_producer: block_producer_sender,
            reth_service: reth_service_sender,
            reorg_events: reorg_sender,
            block_migrated_events: block_migrated_sender,
            block_state_events: block_state_sender,
            peer_events: peer_events_sender,
            peer_network: PeerNetworkSender::new(peer_network_sender),
            block_discovery: block_discovery_sender,
            packing_handle: OnceLock::new(),
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            chunk_migration: chunk_migration_receiver,
            mempool: mempool_receiver,
            vdf_fast_forward: vdf_fast_forward_receiver,
            storage_modules: sm_receiver,
            data_sync: ds_receiver,
            gossip_broadcast: gossip_broadcast_receiver,
            block_tree: block_tree_receiver,
            block_index: block_index_receiver,
            validation_service: validation_receiver,
            block_producer: block_producer_receiver,
            reth_service: reth_service_receiver,
            reorg_events: reorg_receiver,
            block_migrated_events: block_migrated_receiver,
            block_state_events: block_state_receiver,
            peer_events: peer_events_receiver,
            peer_network: peer_network_receiver,
            block_discovery: block_discovery_receiver,
        };
        (senders, receivers)
    }

    /// Subscribe to reorg events - can be called multiple times
    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.reorg_events.subscribe()
    }

    pub fn subscribe_block_migrated(&self) -> broadcast::Receiver<BlockMigratedEvent> {
        self.block_migrated_events.subscribe()
    }
}

/// Stop the actor
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Stop;
