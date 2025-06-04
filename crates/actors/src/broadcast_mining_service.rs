use crate::mining::PartitionMiningActor;
use actix::prelude::*;
use irys_types::{block_production::Seed, H256List, IrysBlockHeader};
use irys_vdf::MiningBroadcaster;
use std::sync::Arc;
use tracing::{debug, info, Span};
// Message types

/// Subscribes a `PartitionMiningActor` so the broadcaster to receive broadcast messages
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Subscribe(pub Addr<PartitionMiningActor>);

/// Unsubscribes a `PartitionMiningActor` so from the broadcaster
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Unsubscribe(pub Addr<PartitionMiningActor>);

/// Send the most recent mining step to all the `PartitionMiningActors`
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastMiningSeed {
    pub seed: Seed,
    pub checkpoints: H256List,
    pub global_step: u64,
}

/// Send the latest difficulty update to all the `PartitionMiningActors`
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastDifficultyUpdate(pub Arc<IrysBlockHeader>);

/// Send Partition expiration list to miners
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastPartitionsExpiration(pub H256List);

/// Broadcaster actor
#[derive(Debug, Default)]
pub struct BroadcastMiningService {
    pub subscribers: Vec<Addr<PartitionMiningActor>>,
    pub span: Option<Span>,
}
// Actor Definition

impl BroadcastMiningService {
    /// Initialize a new `MiningBroadcaster`
    pub fn new(span: Option<Span>) -> Self {
        Self {
            subscribers: Vec::new(),
            span: Some(span.unwrap_or(Span::current())),
        }
    }
}

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

// Handle subscriptions
impl Handler<Subscribe> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: Subscribe, _: &mut Context<Self>) {
        if self.span.is_some() {
            let span = self.span.clone().unwrap();
            let _span = span.enter();
        }

        debug!("PartitionMiningActor subscribed");

        self.subscribers.push(msg.0);
    }
}

// Handle unsubscribe
impl Handler<Unsubscribe> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: Unsubscribe, _: &mut Context<Self>) {
        self.subscribers.retain(|addr| addr != &msg.0);
    }
}

// Handle broadcasts
impl Handler<BroadcastMiningSeed> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMiningSeed, _: &mut Context<Self>) {
        let span = self.span.clone().unwrap();
        let _span = span.enter();
        info!(
            "Broadcast Mining: {:?} subs: {}",
            msg.seed,
            &self.subscribers.len()
        );
        self.subscribers.retain(|addr| addr.connected());
        for subscriber in &self.subscribers {
            subscriber.do_send(msg.clone());
        }
    }
}

impl Handler<BroadcastDifficultyUpdate> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: BroadcastDifficultyUpdate, _: &mut Context<Self>) {
        self.subscribers.retain(|addr| addr.connected());
        for subscriber in &self.subscribers {
            subscriber.do_send(msg.clone());
        }
    }
}

impl Handler<BroadcastPartitionsExpiration> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: BroadcastPartitionsExpiration, _: &mut Context<Self>) {
        self.subscribers.retain(|addr| addr.connected());
        debug!(msg = ?msg.0, "Broadcasting expiration, expired partition hashes");
        for subscriber in &self.subscribers {
            subscriber.do_send(msg.clone());
        }
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
