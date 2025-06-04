use crate::{
    cache_service::CacheServiceAction, ema_service::EmaServiceMessage,
    mempool_service::MempoolServiceMessage, CommitmentCacheMessage, StorageModuleServiceMessage,
};
use actix::Message;
use core::ops::Deref;
use irys_types::GossipData;
use irys_vdf::StepWithCheckpoints;
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
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: UnboundedReceiver<CacheServiceAction>,
    pub ema: UnboundedReceiver<EmaServiceMessage>,
    pub commitments_cache: UnboundedReceiver<CommitmentCacheMessage>,
    pub mempool: UnboundedReceiver<MempoolServiceMessage>,
    pub vdf_mining: Receiver<bool>,
    pub vdf_fast_forward: Receiver<StepWithCheckpoints>,
    pub storage_modules: UnboundedReceiver<StorageModuleServiceMessage>,
    pub gossip_broadcast: UnboundedReceiver<GossipData>,
    pub block_tree: UnboundedReceiver<BlockTreeServiceMessage>,
    pub reorg_events: broadcast::Receiver<ReorgEvent>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
    pub ema: UnboundedSender<EmaServiceMessage>,
    pub commitment_cache: UnboundedSender<CommitmentCacheMessage>,
    pub mempool: UnboundedSender<MempoolServiceMessage>,
    pub vdf_mining: Sender<bool>,
    pub vdf_fast_forward: Sender<StepWithCheckpoints>,
    pub storage_modules: UnboundedSender<StorageModuleServiceMessage>,
    pub gossip_broadcast: UnboundedSender<GossipData>,
    pub block_tree: UnboundedSender<BlockTreeServiceMessage>,
    pub reorg_events: broadcast::Sender<ReorgEvent>,
}

impl ServiceSendersInner {
    #[must_use]
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) = unbounded_channel::<CacheServiceAction>();
        let (ema_sender, ema_receiver) = unbounded_channel::<EmaServiceMessage>();
        let (commitments_cache_sender, commitments_cached_receiver) =
            unbounded_channel::<CommitmentCacheMessage>();

        let (mempool_sender, mempool_receiver) = unbounded_channel::<MempoolServiceMessage>();
        // enabling/disabling VDF mining thread
        let (vdf_mining_sender, vdf_mining_receiver) = channel::<bool>(1);
        // vdf channel for fast forwarding steps during node sync
        let (vdf_fast_forward_sender, vdf_fast_forward_receiver) =
            channel::<StepWithCheckpoints>(1);
        let (sm_sender, sm_receiver) = unbounded_channel::<StorageModuleServiceMessage>();
        let (gossip_broadcast_sender, gossip_broadcast_receiver) =
            unbounded_channel::<GossipData>();
        let (block_tree_sender, block_tree_receiver) =
            unbounded_channel::<BlockTreeServiceMessage>();
        // Create broadcast channel for reorg events
        let (reorg_sender, reorg_receiver) = broadcast::channel::<ReorgEvent>(100);

        let senders = Self {
            chunk_cache: chunk_cache_sender,
            ema: ema_sender,
            commitment_cache: commitments_cache_sender,
            mempool: mempool_sender,
            vdf_mining: vdf_mining_sender,
            vdf_fast_forward: vdf_fast_forward_sender,
            storage_modules: sm_sender,
            gossip_broadcast: gossip_broadcast_sender,
            block_tree: block_tree_sender,
            reorg_events: reorg_sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            ema: ema_receiver,
            commitments_cache: commitments_cached_receiver,
            mempool: mempool_receiver,
            vdf_mining: vdf_mining_receiver,
            vdf_fast_forward: vdf_fast_forward_receiver,
            storage_modules: sm_receiver,
            gossip_broadcast: gossip_broadcast_receiver,
            block_tree: block_tree_receiver,
            reorg_events: reorg_receiver,
        };
        (senders, receivers)
    }

    /// Subscribe to reorg events - can be called multiple times
    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.reorg_events.subscribe()
    }
}

/// Stop the actor
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Stop;
