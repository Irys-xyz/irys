use std::{ops::Deref, sync::Arc};

// should we be using crossbeam?
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::cache_service::CacheServiceAction;

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
    pub fn init() -> (Self, ServiceReceivers) {
        let (senders, receivers) = ServiceSendersInner::init();
        (Self(Arc::new(senders)), receivers)
    }
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: Option<UnboundedReceiver<CacheServiceAction>>,
}
// todo: make a macro to autogenerate these?
#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<CacheServiceAction>,
}

impl ServiceSendersInner {
    pub fn init() -> (Self, ServiceReceivers) {
        let (sender, receiver) = unbounded_channel::<CacheServiceAction>();
        let senders = Self {
            chunk_cache: sender,
        };
        let receivers = ServiceReceivers {
            chunk_cache: Some(receiver),
        };
        (senders, receivers)
    }
}
