use actix::Message;
use core::ops::Deref;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{
    cache_service::CacheServiceAction, ema_service::EmaServiceMessage, CommitmentCacheMessage,
    StorageModuleServiceMessage, vdf_service::VdfServiceMessage, 
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
    pub vdf: UnboundedReceiver<VdfServiceMessage>,
    pub storage_modules: UnboundedReceiver<StorageModuleServiceMessage>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
    pub ema: UnboundedSender<EmaServiceMessage>,
    pub commitment_cache: UnboundedSender<CommitmentCacheMessage>,
    pub vdf: UnboundedSender<VdfServiceMessage>,
    pub storage_modules: UnboundedSender<StorageModuleServiceMessage>,
}

impl ServiceSendersInner {
    #[must_use]
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) = unbounded_channel::<CacheServiceAction>();
        let (ema_sender, ema_receiver) = unbounded_channel::<EmaServiceMessage>();
        let (commitments_cache_sender, commitments_cached_receiver) =
            unbounded_channel::<CommitmentCacheMessage>();
        let (vdf_sender, vdf_receiver) = unbounded_channel::<VdfServiceMessage>();
        let (sm_sender, sm_receiver) = unbounded_channel::<StorageModuleServiceMessage>();

        let senders = Self {
            chunk_cache: chunk_cache_sender,
            ema: ema_sender,
            commitment_cache: commitments_cache_sender,
            vdf: vdf_sender,
            storage_modules: sm_sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            ema: ema_receiver,
            commitments_cache: commitments_cached_receiver,
            vdf: vdf_receiver,
            storage_modules: sm_receiver,
        };
        (senders, receivers)
    }
}

/// Stop the actor
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Stop;
