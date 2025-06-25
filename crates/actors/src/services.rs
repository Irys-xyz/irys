use crate::{
    block_tree_service::{BlockMigratedEvent, BlockTreeServiceMessage, ReorgEvent},
    cache_service::CacheServiceAction,
    ema_service::EmaServiceMessage,
    mempool_service::MempoolServiceMessage,
    validation_service::ValidationServiceMessage,
    StorageModuleServiceMessage,
};
use actix::Message;
use core::ops::Deref;
use irys_types::GossipBroadcastMessage;
use irys_vdf::VdfStep;
use std::sync::Arc;
use tokio::sync::{
    broadcast,
    mpsc::{channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
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
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: UnboundedReceiver<CacheServiceAction>,
    pub ema: UnboundedReceiver<EmaServiceMessage>,
    pub mempool: UnboundedReceiver<MempoolServiceMessage>,
    pub vdf_mining: Receiver<bool>,
    pub vdf_fast_forward: UnboundedReceiver<VdfStep>,
    pub storage_modules: UnboundedReceiver<StorageModuleServiceMessage>,
    pub gossip_broadcast: UnboundedReceiver<GossipBroadcastMessage>,
    pub block_tree: UnboundedReceiver<BlockTreeServiceMessage>,
    pub validation_service: UnboundedReceiver<ValidationServiceMessage>,
    pub reorg_events: broadcast::Receiver<ReorgEvent>,
    pub block_migrated_events: broadcast::Receiver<BlockMigratedEvent>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
    pub ema: UnboundedSender<EmaServiceMessage>,
    pub mempool: UnboundedSender<MempoolServiceMessage>,
    pub vdf_mining: Sender<bool>,
    pub vdf_fast_forward: UnboundedSender<VdfStep>,
    pub storage_modules: UnboundedSender<StorageModuleServiceMessage>,
    pub gossip_broadcast: UnboundedSender<GossipBroadcastMessage>,
    pub block_tree: UnboundedSender<BlockTreeServiceMessage>,
    pub validation_service: UnboundedSender<ValidationServiceMessage>,
    pub reorg_events: broadcast::Sender<ReorgEvent>,
    pub block_migrated_events: broadcast::Sender<BlockMigratedEvent>,
}

impl ServiceSendersInner {
    #[must_use]
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) = unbounded_channel::<CacheServiceAction>();
        let (ema_sender, ema_receiver) = unbounded_channel::<EmaServiceMessage>();

        let (mempool_sender, mempool_receiver) = unbounded_channel::<MempoolServiceMessage>();
        // enabling/disabling VDF mining thread
        let (vdf_mining_sender, vdf_mining_receiver) = channel::<bool>(1);
        // vdf channel for fast forwarding steps during node sync
        let (vdf_fast_forward_sender, vdf_fast_forward_receiver) = unbounded_channel::<VdfStep>();
        let (sm_sender, sm_receiver) = unbounded_channel::<StorageModuleServiceMessage>();
        let (gossip_broadcast_sender, gossip_broadcast_receiver) =
            unbounded_channel::<GossipBroadcastMessage>();
        let (block_tree_sender, block_tree_receiver) =
            unbounded_channel::<BlockTreeServiceMessage>();
        let (validation_sender, validation_receiver) =
            unbounded_channel::<ValidationServiceMessage>();
        // Create broadcast channel for reorg events
        let (reorg_sender, reorg_receiver) = broadcast::channel::<ReorgEvent>(100);
        let (block_migrated_sender, block_migrated_receiver) =
            broadcast::channel::<BlockMigratedEvent>(100);

        let senders = Self {
            chunk_cache: chunk_cache_sender,
            ema: ema_sender,
            mempool: mempool_sender,
            vdf_mining: vdf_mining_sender,
            vdf_fast_forward: vdf_fast_forward_sender,
            storage_modules: sm_sender,
            gossip_broadcast: gossip_broadcast_sender,
            block_tree: block_tree_sender,
            validation_service: validation_sender,
            reorg_events: reorg_sender,
            block_migrated_events: block_migrated_sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            ema: ema_receiver,
            mempool: mempool_receiver,
            vdf_mining: vdf_mining_receiver,
            vdf_fast_forward: vdf_fast_forward_receiver,
            storage_modules: sm_receiver,
            gossip_broadcast: gossip_broadcast_receiver,
            block_tree: block_tree_receiver,
            validation_service: validation_receiver,
            reorg_events: reorg_receiver,
            block_migrated_events: block_migrated_receiver,
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
