use std::sync::{Arc, Mutex};

// broadcast message types are defined locally in this module
use irys_types::{block_production::Seed, H256List, IrysBlockHeader};
use irys_vdf::MiningBroadcaster;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, Span};

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
    subscribers: Mutex<Vec<UnboundedSender<MiningBroadcastEvent>>>,
    span: Option<Span>,
}

/// Tokio-native mining bus that supports fan-out to multiple subscribers.
///
/// - subscribe() returns an UnboundedReceiver that yields MiningBroadcastEvent items
/// - send_* helpers fan-out events to all current subscribers
///
/// This provides a central event hub for:
/// - VDF thread broadcasting new seeds/checkpoints
/// - Block tree broadcasting difficulty updates and partition expirations
/// - Partition mining services consuming the stream of events
#[derive(Debug, Clone)]
pub struct MiningBus(Arc<MiningBusInner>);

impl MiningBus {
    /// Create a new mining bus with an optional tracing Span applied to send operations.
    pub fn new(span: Option<Span>) -> Self {
        Self(Arc::new(MiningBusInner {
            subscribers: Mutex::new(Vec::new()),
            span,
        }))
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

    /// Subscribe to mining events. Returns a new UnboundedReceiver.
    /// The receiver will get all subsequent broadcast events.
    pub fn subscribe(&self) -> UnboundedReceiver<MiningBroadcastEvent> {
        let (tx, rx) = unbounded_channel();
        let mut subs = self.0.subscribers.lock().expect("mining bus poisoned");
        subs.push(tx);
        rx
    }

    /// Broadcast a raw event to all current subscribers.
    /// Returns the number of subscribers after pruning closed channels.
    pub fn send_event(&self, evt: MiningBroadcastEvent) -> usize {
        let mut subs = self.0.subscribers.lock().expect("mining bus poisoned");

        // Retain only senders that can receive this event
        subs.retain(|tx| tx.send(evt.clone()).is_ok());

        subs.len()
    }

    /// Send a seed/checkpoints/global step update to all subscribers.
    pub fn send_seed(&self, seed: Seed, checkpoints: H256List, global_step: u64) -> usize {
        Self::with_span(&self.0.span, || {
            let total = self.0.subscribers.lock().map(|s| s.len()).unwrap_or(0);
            info!("Broadcast Mining: seed {:?}, subs: {}", seed, total);
        });

        let msg = BroadcastMiningSeed {
            seed,
            checkpoints,
            global_step,
        };
        self.send_event(MiningBroadcastEvent::Seed(msg))
    }

    /// Send a difficulty update to all subscribers.
    pub fn send_difficulty(&self, msg: BroadcastDifficultyUpdate) -> usize {
        self.send_event(MiningBroadcastEvent::Difficulty(msg))
    }

    /// Send partition expiration notice to all subscribers.
    pub fn send_partitions_expiration(&self, msg: BroadcastPartitionsExpiration) -> usize {
        debug!(msg = ?msg.0, "Broadcasting expiration, expired partition hashes");
        self.send_event(MiningBroadcastEvent::PartitionsExpiration(msg))
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
