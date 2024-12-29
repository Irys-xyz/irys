use crate::mining::PartitionMiningActor;
use actix::prelude::*;
use irys_types::{block_production::Seed, H256List, IrysBlockHeader, H256};
use std::sync::Arc;
use tracing::info;

// Message types

/// Subscribes a PartitionMiningActor so the broadcaster to receive broadcast messages
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Subscribe(pub Addr<PartitionMiningActor>);

/// Unsubscribes a PartitionMiningActor so from the broadcaster
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Unsubscribe(pub Addr<PartitionMiningActor>);

/// Send the most recent mining step to all the PartitionMiningActors
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastMiningSeed {
    pub seed: Seed,
    pub checkpoints: H256List,
    pub global_step: u64,
}

/// Send the latest difficulty update to all the PartitionMiningActors
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BroadcastDifficultyUpdate(pub Arc<IrysBlockHeader>);

/// Broadcaster actor
#[derive(Debug, Default)]
pub struct BroadcastMiningService {
    subscribers: Vec<Addr<PartitionMiningActor>>,
}
// Actor Definition

impl BroadcastMiningService {
    /// Initialize a new MiningBroadcaster
    pub fn new() -> Self {
        Self {
            subscribers: Vec::new(),
        }
    }
}

impl Actor for BroadcastMiningService {
    type Context = Context<Self>;
}

/// Adds this actor the the local service registry
impl Supervised for BroadcastMiningService {}

impl ArbiterService for BroadcastMiningService {
    fn service_started(&mut self, ctx: &mut Context<Self>) {
        println!("broadcast_mining service started");
    }
}

// Handle subscriptions
impl Handler<Subscribe> for BroadcastMiningService {
    type Result = ();

    fn handle(&mut self, msg: Subscribe, _: &mut Context<Self>) {
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
        info!("Mining: {:?}", msg.seed);
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
