use std::sync::{Arc, Mutex};

// broadcast message types are defined locally in this module
use irys_types::{H256List, IrysBlockHeader, block_production::Seed};
use irys_vdf::MiningBroadcaster;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{debug, info};

/// Tokio-native broadcast envelope for mining events.
#[derive(Debug, Clone)]
pub struct BroadcastMiningSeed {
    pub seed: Seed,
    pub checkpoints: H256List,
    pub global_step: u64,
}

#[derive(Debug, Clone)]
pub struct BroadcastDifficultyUpdate(pub Arc<IrysBlockHeader>);

#[derive(Debug, Clone)]
pub struct BroadcastPartitionsExpiration(pub H256List);

#[derive(Debug, Clone)]
pub enum MiningBroadcastEvent {
    Seed(BroadcastMiningSeed),
    Difficulty(BroadcastDifficultyUpdate),
    PartitionsExpiration(BroadcastPartitionsExpiration),
}

#[derive(Debug)]
struct MiningBusInner {
    subscribers: Mutex<Vec<UnboundedSender<Arc<MiningBroadcastEvent>>>>,
}

/// Tokio-native mining bus that supports fan-out to multiple subscribers.
///
/// - subscribe() returns an UnboundedReceiver that yields MiningBroadcastEvent items
///   - Why not a tokio broadcast::channel? PartitionsExpiration may not like a bounded channel. Todo: Investigate
/// - send_* helpers fan-out events to all current subscribers
/// - Used by:
///   - VDF thread via `MiningBusBroadcaster` broadcasting new Seeds
///   - `BlockTreeService` broadcasting PartitionsExpiration events
///   - `BlockTreeService` and `BlockProducerService` broadcasting Difficulty updates
///   - `PartitionMiningService` consuming `Seed`, `Difficulty`, and `PartitionsExpiration` events
#[derive(Debug, Clone)]
pub struct MiningBus(Arc<MiningBusInner>);

impl Default for MiningBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MiningBus {
    /// Create a new mining bus.
    pub fn new() -> Self {
        Self(Arc::new(MiningBusInner {
            subscribers: Mutex::new(Vec::new()),
        }))
    }

    /// Subscribe to mining events. Returns a new UnboundedReceiver.
    /// The receiver will get all subsequent broadcast events.
    pub fn subscribe(&self) -> UnboundedReceiver<Arc<MiningBroadcastEvent>> {
        let (tx, rx) = unbounded_channel();
        let mut subs = self.0.subscribers.lock().expect("mining bus poisoned");
        subs.push(tx);
        rx
    }

    /// Broadcast a raw event to all current subscribers.
    /// Returns the number of subscribers after pruning closed channels.
    pub fn send_event(&self, evt: Arc<MiningBroadcastEvent>) -> usize {
        let mut subs = self.0.subscribers.lock().expect("mining bus poisoned");

        // Retain only senders that can receive this event
        subs.retain(|tx| tx.send(evt.clone()).is_ok());

        subs.len()
    }

    /// Send a seed/checkpoints/global step update to all subscribers.
    #[tracing::instrument(level = "trace", skip_all, fields(vdf.seed = ?seed, vdf.global_step = %global_step, vdf.subscriber_count))]
    pub fn send_seed(&self, seed: Seed, checkpoints: H256List, global_step: u64) -> usize {
        let total = self.0.subscribers.lock().map(|s| s.len()).unwrap_or(0);
        info!("Broadcast Mining: seed {:?}, subs: {}", seed, total);

        let msg = BroadcastMiningSeed {
            seed,
            checkpoints,
            global_step,
        };
        self.send_event(Arc::new(MiningBroadcastEvent::Seed(msg)))
    }

    /// Send a difficulty update to all subscribers.
    pub fn send_difficulty(&self, msg: BroadcastDifficultyUpdate) -> usize {
        self.send_event(Arc::new(MiningBroadcastEvent::Difficulty(msg)))
    }

    /// Send partition expiration notice to all subscribers.
    #[tracing::instrument(level = "trace", skip_all, fields(partition.expired_count = msg.0.len()))]
    pub fn send_partitions_expiration(&self, msg: BroadcastPartitionsExpiration) -> usize {
        debug!(custom.msg = ?msg.0, "Broadcasting expiration, expired partition hashes");
        self.send_event(Arc::new(MiningBroadcastEvent::PartitionsExpiration(msg)))
    }
}

/// A MiningBroadcaster implementation backed by the Tokio-native MiningBus.
///
/// This is used by the VDF thread to publish new seeds/checkpoints
#[derive(Debug, Clone)]
pub struct MiningBusBroadcaster {
    bus: MiningBus,
}

impl MiningBusBroadcaster {
    pub fn new(bus: MiningBus) -> Self {
        Self { bus }
    }

    pub fn bus(&self) -> MiningBus {
        self.bus.clone()
    }
}

impl From<MiningBus> for MiningBusBroadcaster {
    fn from(bus: MiningBus) -> Self {
        Self::new(bus)
    }
}

impl MiningBroadcaster for MiningBusBroadcaster {
    fn broadcast(&self, seed: Seed, checkpoints: H256List, global_step: u64) {
        let _ = self.bus.send_seed(seed, checkpoints, global_step);
    }
}
