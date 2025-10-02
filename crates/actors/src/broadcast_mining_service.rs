use crate::mining::PartitionMiningActor;
use actix::prelude::*;
use irys_types::{block_production::Seed, H256List, IrysBlockHeader};
use irys_vdf::MiningBroadcaster;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, Span};

/// Subscribes a `PartitionMiningActor` so the broadcaster can send broadcast messages
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Subscribe(pub Addr<PartitionMiningActor>);

/// Unsubscribes a `PartitionMiningActor` from the broadcaster
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Unsubscribe(pub Addr<PartitionMiningActor>);

/// Send the most recent mining step to all subscribers
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastMiningSeed {
    pub seed: Seed,
    pub checkpoints: H256List,
    pub global_step: u64,
}

/// Send the latest difficulty update to all subscribers
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastDifficultyUpdate(pub Arc<IrysBlockHeader>);

/// Send Partition expiration list to miners
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastPartitionsExpiration(pub H256List);

/// Tokio-side broadcast envelope so non-Actix subscribers can receive events
#[derive(Debug, Clone)]
pub enum MiningBroadcastEvent {
    Seed(BroadcastMiningSeed),
    Difficulty(BroadcastDifficultyUpdate),
    PartitionsExpiration(BroadcastPartitionsExpiration),
}

/// Subscribe a Tokio channel to receive broadcast messages
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct SubscribeTokio(pub UnboundedSender<MiningBroadcastEvent>);

/// Unsubscribe a Tokio channel from broadcast messages
///
/// Note: Unsubscribe is best-effort; we primarily prune closed channels on send failure.
/// This is provided for symmetry with the Actix API.
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct UnsubscribeTokio(pub UnboundedSender<MiningBroadcastEvent>);

/// Broadcaster actor
#[derive(Debug, Default)]
pub struct BroadcastMiningService {
    pub subscribers: Vec<Addr<PartitionMiningActor>>,
    pub tokio_subscribers: Vec<UnboundedSender<MiningBroadcastEvent>>,
    pub span: Option<Span>,
}

// Actor Definition
impl Actor for BroadcastMiningService {
    type Context = Context<Self>;
}

/// Adds this actor the the local service registry
impl Supervised for BroadcastMiningService {}

impl SystemService for BroadcastMiningService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        debug!("service started: broadcast_mining (Default)");
    }
}

impl BroadcastMiningService {
    /// Initialize a new `MiningBroadcaster`
    pub fn new(span: Option<Span>) -> Self {
        Self {
            subscribers: Vec::new(),
            tokio_subscribers: Vec::new(),
            span: Some(span.unwrap_or(Span::current())),
        }
    }

    fn with_span<F: FnOnce()>(span: &Option<Span>, f: F) {
        if let Some(span) = span {
            let span = span.clone();
            let _entered = span.enter();
            f();
        } else {
            f();
        }
    }

    fn broadcast_to_actix(
        &mut self,
        seed: Option<&BroadcastMiningSeed>,
        diff: Option<&BroadcastDifficultyUpdate>,
        exp: Option<&BroadcastPartitionsExpiration>,
    ) {
        // prune disconnected actix subscribers
        self.subscribers.retain(actix::Addr::connected);

        for sub in &self.subscribers {
            if let Some(msg) = seed {
                sub.do_send(msg.clone());
            }
            if let Some(msg) = diff {
                sub.do_send(msg.clone());
            }
            if let Some(msg) = exp {
                sub.do_send(msg.clone());
            }
        }
    }

    fn broadcast_to_tokio(&mut self, evt: MiningBroadcastEvent) {
        // retain only channels that can receive
        self.tokio_subscribers
            .retain(|tx| tx.send(evt.clone()).is_ok());
    }
}

// Handle Actix subscriptions
impl Handler<Subscribe> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: Subscribe, _: &mut Context<Self>) {
        Self::with_span(&self.span, || {
            debug!("PartitionMiningActor subscribed");
        });
        self.subscribers.push(msg.0);
    }
}

// Handle Actix unsubscribe
impl Handler<Unsubscribe> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: Unsubscribe, _: &mut Context<Self>) {
        self.subscribers.retain(|addr| addr != &msg.0);
    }
}

// Handle Tokio subscriptions
impl Handler<SubscribeTokio> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: SubscribeTokio, _: &mut Context<Self>) {
        Self::with_span(&self.span, || {
            debug!("Tokio subscriber registered");
        });
        self.tokio_subscribers.push(msg.0);
    }
}

// Handle Tokio unsubscribe (best-effort; primarily pruned on failed sends)
impl Handler<UnsubscribeTokio> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, _msg: UnsubscribeTokio, _: &mut Context<Self>) {
        // No-op: we cannot reliably compare UnboundedSender instances for removal.
        // Subscribers are removed automatically if sending fails.
    }
}

// Handle seed broadcasts
impl Handler<BroadcastMiningSeed> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMiningSeed, _: &mut Context<Self>) {
        let total = self.subscribers.len() + self.tokio_subscribers.len();
        Self::with_span(&self.span, || {
            info!("Broadcast Mining: {:?} subs: {}", msg.seed, total);
        });

        // Actix subscribers
        self.broadcast_to_actix(Some(&msg), None, None);

        // Tokio subscribers
        self.broadcast_to_tokio(MiningBroadcastEvent::Seed(msg));
    }
}

// Handle difficulty updates
impl Handler<BroadcastDifficultyUpdate> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: BroadcastDifficultyUpdate, _: &mut Context<Self>) {
        // Actix subscribers
        self.broadcast_to_actix(None, Some(&msg), None);

        // Tokio subscribers
        self.broadcast_to_tokio(MiningBroadcastEvent::Difficulty(msg));
    }
}

// Handle partition expiration broadcasts
impl Handler<BroadcastPartitionsExpiration> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: BroadcastPartitionsExpiration, _: &mut Context<Self>) {
        Self::with_span(&self.span, || {
            debug!(msg = ?msg.0, "Broadcasting expiration, expired partition hashes");
        });

        // Actix subscribers
        self.broadcast_to_actix(None, None, Some(&msg));

        // Tokio subscribers
        self.broadcast_to_tokio(MiningBroadcastEvent::PartitionsExpiration(msg));
    }
}

#[derive(Debug, Clone)]
pub struct MiningServiceBroadcaster(Addr<BroadcastMiningService>);

impl MiningBroadcaster for MiningServiceBroadcaster {
    fn broadcast(&self, seed: Seed, checkpoints: H256List, global_step: u64) {
        self.0.do_send(BroadcastMiningSeed {
            seed,
            checkpoints,
            global_step,
        })
    }
}

impl From<Addr<BroadcastMiningService>> for MiningServiceBroadcaster {
    fn from(addr: Addr<BroadcastMiningService>) -> Self {
        Self(addr)
    }
}
