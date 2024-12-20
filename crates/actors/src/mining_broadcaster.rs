use crate::mining::{PartitionMiningActor, Seed};
use actix::prelude::*;
use irys_types::IrysBlockHeader;
use std::sync::Arc;
use tracing::info;

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
pub struct BroadcastMiningSeed(pub Seed);

/// Send the latest difficulty update to all the `PartitionMiningActors`
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastDifficultyUpdate(pub Arc<IrysBlockHeader>);

/// Broadcaster actor
#[derive(Debug)]
pub struct MiningBroadcaster {
    subscribers: Vec<Addr<PartitionMiningActor>>,
}
// Actor Definition

impl Default for MiningBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl MiningBroadcaster {
    /// Initialize a new `MiningBroadcaster`
    pub const fn new() -> Self {
        Self {
            subscribers: Vec::new(),
        }
    }
}

impl Actor for MiningBroadcaster {
    type Context = Context<Self>;
}

// Handle subscriptions
impl Handler<Subscribe> for MiningBroadcaster {
    type Result = ();

    fn handle(&mut self, msg: Subscribe, _: &mut Context<Self>) {
        self.subscribers.push(msg.0);
    }
}

// Handle unsubscribe
impl Handler<Unsubscribe> for MiningBroadcaster {
    type Result = ();

    fn handle(&mut self, msg: Unsubscribe, _: &mut Context<Self>) {
        self.subscribers.retain(|addr| addr != &msg.0);
    }
}

// Handle broadcasts
impl Handler<BroadcastMiningSeed> for MiningBroadcaster {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMiningSeed, _: &mut Context<Self>) {
        info!("Mining: {:?}", msg.0);
        self.subscribers.retain(|addr| addr.connected());
        for subscriber in &self.subscribers {
            subscriber.do_send(msg.clone());
        }
    }
}

impl Handler<BroadcastDifficultyUpdate> for MiningBroadcaster {
    type Result = ();

    fn handle(&mut self, msg: BroadcastDifficultyUpdate, _: &mut Context<Self>) {
        self.subscribers.retain(|addr| addr.connected());
        for subscriber in &self.subscribers {
            subscriber.do_send(msg.clone());
        }
    }
}
