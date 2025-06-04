use crate::{
    broadcast_mining_service::BroadcastMiningSeed, cache_service::CacheServiceAction,
    ema_service::EmaServiceMessage, mempool_service::MempoolServiceMessage, CommitmentCacheMessage,
    StorageModuleServiceMessage,
};
use actix::Message;
use core::ops::Deref;
use irys_types::GossipData;
use std::sync::Arc;
use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
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
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: UnboundedReceiver<CacheServiceAction>,
    pub ema: UnboundedReceiver<EmaServiceMessage>,
    pub commitments_cache: UnboundedReceiver<CommitmentCacheMessage>,
    pub mempool: UnboundedReceiver<MempoolServiceMessage>,
    pub vdf_mining: Receiver<bool>,
    pub vdf_seed: Receiver<BroadcastMiningSeed>,
    pub storage_modules: UnboundedReceiver<StorageModuleServiceMessage>,
    pub gossip_broadcast: UnboundedReceiver<GossipData>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
    pub ema: UnboundedSender<EmaServiceMessage>,
    pub commitment_cache: UnboundedSender<CommitmentCacheMessage>,
    pub mempool: UnboundedSender<MempoolServiceMessage>,
    pub vdf_mining: Sender<bool>,
    pub vdf_seed: Sender<BroadcastMiningSeed>,
    pub storage_modules: UnboundedSender<StorageModuleServiceMessage>,
    pub gossip_broadcast: UnboundedSender<GossipData>,
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
        let (vdf_seed_sender, vdf_seed_receiver) = channel::<BroadcastMiningSeed>(1);
        let (sm_sender, sm_receiver) = unbounded_channel::<StorageModuleServiceMessage>();
        let (gossip_broadcast_sender, gossip_broadcast_receiver) =
            unbounded_channel::<GossipData>();

        let senders = Self {
            chunk_cache: chunk_cache_sender,
            ema: ema_sender,
            commitment_cache: commitments_cache_sender,
            mempool: mempool_sender,
            vdf_mining: vdf_mining_sender,
            vdf_seed: vdf_seed_sender,
            storage_modules: sm_sender,
            gossip_broadcast: gossip_broadcast_sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            ema: ema_receiver,
            commitments_cache: commitments_cached_receiver,
            mempool: mempool_receiver,
            vdf_mining: vdf_mining_receiver,
            vdf_seed: vdf_seed_receiver,
            storage_modules: sm_receiver,
            gossip_broadcast: gossip_broadcast_receiver,
        };
        (senders, receivers)
    }
}

/// Stop the actor
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Stop;
