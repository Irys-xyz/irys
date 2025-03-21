use actix::Message;
use core::ops::Deref;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{cache_service::CacheServiceAction, ema_service::EmaServiceMessage};

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
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
    pub ema: UnboundedSender<EmaServiceMessage>,
}

impl ServiceSendersInner {
    #[must_use]
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) = unbounded_channel::<CacheServiceAction>();
        let (ema_sender, ema_receiver) = unbounded_channel::<EmaServiceMessage>();

        let senders = Self {
            chunk_cache: chunk_cache_sender,
            ema: ema_sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            ema: ema_receiver,
        };
        (senders, receivers)
    }
}

/// Stop the actor
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Stop;
