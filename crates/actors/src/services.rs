use crate::chunk_ingress_service::ChunkIngressMessage;
use crate::mining_bus::{MiningBroadcastEvent, MiningBus};
use crate::{
    DataSyncServiceMessage, StorageModuleServiceMessage,
    block_discovery::BlockDiscoveryMessage,
    block_producer::BlockProducerCommand,
    block_tree_service::{BlockStateUpdated, BlockTreeServiceMessage, ReorgEvent},
    cache_service::CacheServiceAction,
    chunk_migration_service::ChunkMigrationServiceMessage,
    mempool_service::MempoolServiceMessage,
    packing_service::{PackingRequest, PackingSender, PackingService},
    reth_service::RethServiceMessage,
    validation_service::ValidationServiceMessage,
};
use core::ops::Deref;
use irys_domain::PeerEvent;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{BlockHash, PeerNetworkSender, PeerNetworkServiceMessage, Traced};
use irys_vdf::VdfStep;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};
use tokio::sync::{
    broadcast,
    mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender, channel, unbounded_channel},
};

use crate::block_tree_service::ValidationResult;

const VDF_FAST_FORWARD_CHANNEL_CAPACITY: usize = 4_096;

/// Bound for the recent-validation-results store on `ServiceSendersInner`.
///
/// The store seeds new `block_state_events` subscribers with the parent's
/// last validation result so the broadcast race (parent's event fires before
/// child subscribes) cannot misattribute a child's cancellation reason.
///
/// 1024 is well above the realistic concurrent-validation window — the
/// in-flight set is bounded by `block_tree_depth` (defaults 50–100) plus
/// any out-of-tree blocks still being routed. Pick a fixed power-of-two for
/// determinism; the store entries are tiny (`BlockHash` + a small
/// `ValidationResult` enum).
const RECENT_VALIDATION_RESULTS_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
pub struct ServiceSenders(pub Arc<ServiceSendersInner>);

impl Deref for ServiceSenders {
    type Target = Arc<ServiceSendersInner>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ServiceSenders {
    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.0.subscribe_reorgs()
    }

    pub fn subscribe_block_state_updates(&self) -> broadcast::Receiver<BlockStateUpdated> {
        self.0.block_state_events.subscribe()
    }

    /// Look up the most-recently-broadcast `ValidationResult` for `block_hash`.
    ///
    /// Returns `Some(result)` if an entry is still cached, `None` otherwise.
    /// Used by validation tasks to seed their `parent_last_validation_result`
    /// on entry to a wait loop, covering the case where the parent's
    /// `BlockStateUpdated` was broadcast BEFORE the child subscribed
    /// (`tokio::sync::broadcast` does not replay past events).
    ///
    /// Ordering invariant: every site that broadcasts a `BlockStateUpdated`
    /// in `block_tree_service.rs` MUST call `record_validation_result`
    /// BEFORE `block_state_events.send(..)`. Otherwise a subscriber woken by
    /// `recv()` could query this store and see stale `None`, reintroducing
    /// the race in the other direction.
    pub fn recent_validation_result(&self, block_hash: &BlockHash) -> Option<ValidationResult> {
        self.0.recent_validation_result(block_hash)
    }

    /// Record the validation result for `block_hash` into the recent-results
    /// store. MUST be called before sending the corresponding
    /// `BlockStateUpdated` broadcast — see `recent_validation_result`.
    pub fn record_validation_result(&self, block_hash: BlockHash, result: ValidationResult) {
        self.0.record_validation_result(block_hash, result);
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.0.peer_events.subscribe()
    }

    pub fn new() -> (Self, ServiceReceivers) {
        let (senders, receivers) = ServiceSendersInner::init();
        (Self(Arc::new(senders)), receivers)
    }

    pub fn packing_sender(&self) -> PackingSender {
        self.0.packing_sender.clone()
    }

    pub fn mining_bus(&self) -> MiningBus {
        self.0.mining_bus.clone()
    }

    pub fn subscribe_mining_broadcast(&self) -> UnboundedReceiver<Arc<MiningBroadcastEvent>> {
        self.0.subscribe_mining_broadcast()
    }

    pub fn send_mining_seed(
        &self,
        seed: irys_types::block_production::Seed,
        checkpoints: irys_types::H256List,
        global_step: u64,
    ) {
        let _ = self.0.mining_bus.send_seed(seed, checkpoints, global_step);
    }

    pub fn send_mining_difficulty(&self, msg: crate::mining_bus::BroadcastDifficultyUpdate) {
        let _ = self.0.mining_bus.send_difficulty(msg);
    }

    pub fn send_partitions_expiration(
        &self,
        msg: crate::mining_bus::BroadcastPartitionsExpiration,
    ) {
        let _ = self.0.mining_bus.send_partitions_expiration(msg);
    }
}

#[derive(Debug)]
pub struct ServiceReceivers {
    pub chunk_cache: UnboundedReceiver<Traced<CacheServiceAction>>,
    pub chunk_ingress: UnboundedReceiver<Traced<ChunkIngressMessage>>,
    pub chunk_migration: UnboundedReceiver<Traced<ChunkMigrationServiceMessage>>,
    pub mempool: UnboundedReceiver<Traced<MempoolServiceMessage>>,
    pub vdf_fast_forward: Receiver<Traced<VdfStep>>,
    pub storage_modules: UnboundedReceiver<Traced<StorageModuleServiceMessage>>,
    pub data_sync: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
    pub gossip_broadcast: UnboundedReceiver<Traced<GossipBroadcastMessageV2>>,
    pub block_tree: UnboundedReceiver<Traced<BlockTreeServiceMessage>>,
    pub validation_service: UnboundedReceiver<Traced<ValidationServiceMessage>>,
    pub block_producer: UnboundedReceiver<Traced<BlockProducerCommand>>,
    pub reth_service: UnboundedReceiver<Traced<RethServiceMessage>>,
    pub reorg_events: broadcast::Receiver<ReorgEvent>,
    pub block_state_events: broadcast::Receiver<BlockStateUpdated>,
    pub peer_events: broadcast::Receiver<PeerEvent>,
    pub peer_network: UnboundedReceiver<PeerNetworkServiceMessage>,
    pub block_discovery: UnboundedReceiver<Traced<BlockDiscoveryMessage>>,
    pub packing: tokio::sync::mpsc::Receiver<PackingRequest>,
}

#[derive(Debug)]
pub struct ServiceSendersInner {
    pub chunk_cache: UnboundedSender<Traced<CacheServiceAction>>,
    pub chunk_ingress: UnboundedSender<Traced<ChunkIngressMessage>>,
    pub chunk_migration: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
    pub mempool: UnboundedSender<Traced<MempoolServiceMessage>>,
    pub vdf_fast_forward: Sender<Traced<VdfStep>>,
    pub storage_modules: UnboundedSender<Traced<StorageModuleServiceMessage>>,
    pub data_sync: UnboundedSender<Traced<DataSyncServiceMessage>>,
    pub gossip_broadcast: UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    pub block_tree: UnboundedSender<Traced<BlockTreeServiceMessage>>,
    pub validation_service: UnboundedSender<Traced<ValidationServiceMessage>>,
    pub block_producer: UnboundedSender<Traced<BlockProducerCommand>>,
    pub reth_service: UnboundedSender<Traced<RethServiceMessage>>,
    pub reorg_events: broadcast::Sender<ReorgEvent>,
    pub block_state_events: broadcast::Sender<BlockStateUpdated>,
    pub peer_events: broadcast::Sender<PeerEvent>,
    pub peer_network: PeerNetworkSender,
    pub block_discovery: UnboundedSender<Traced<BlockDiscoveryMessage>>,
    pub mining_bus: MiningBus,
    pub packing_sender: PackingSender,
    /// Bounded store of the most-recently-broadcast `ValidationResult` per
    /// block hash. Lives alongside the `block_state_events` broadcast channel
    /// so subscribers that joined AFTER an event was fired can still observe
    /// it (broadcast channels don't replay). See `recent_validation_result`
    /// on `ServiceSenders` for the ordering invariant.
    ///
    /// `std::sync::RwLock` (not `tokio::sync::RwLock`): writers are sync
    /// code inside the `BlockTreeService` message handlers, readers do
    /// quick hash-keyed lookups. No `.await` is ever held across this lock.
    pub recent_validation_results: Arc<RwLock<LruCache<BlockHash, ValidationResult>>>,
}

impl ServiceSendersInner {
    pub fn init() -> (Self, ServiceReceivers) {
        let (chunk_cache_sender, chunk_cache_receiver) =
            unbounded_channel::<Traced<CacheServiceAction>>();
        let (chunk_ingress_sender, chunk_ingress_receiver) =
            unbounded_channel::<Traced<ChunkIngressMessage>>();
        let (chunk_migration_sender, chunk_migration_receiver) =
            unbounded_channel::<Traced<ChunkMigrationServiceMessage>>();
        let (mempool_sender, mempool_receiver) =
            unbounded_channel::<Traced<MempoolServiceMessage>>();
        let (vdf_fast_forward_sender, vdf_fast_forward_receiver) =
            channel::<Traced<VdfStep>>(VDF_FAST_FORWARD_CHANNEL_CAPACITY);
        let (sm_sender, sm_receiver) = unbounded_channel::<Traced<StorageModuleServiceMessage>>();
        let (ds_sender, ds_receiver) = unbounded_channel::<Traced<DataSyncServiceMessage>>();
        let (gossip_broadcast_sender, gossip_broadcast_receiver) =
            unbounded_channel::<Traced<GossipBroadcastMessageV2>>();
        let (block_tree_sender, block_tree_receiver) =
            unbounded_channel::<Traced<BlockTreeServiceMessage>>();
        let (validation_sender, validation_receiver) =
            unbounded_channel::<Traced<ValidationServiceMessage>>();
        let (block_producer_sender, block_producer_receiver) =
            unbounded_channel::<Traced<BlockProducerCommand>>();
        let (reth_service_sender, reth_service_receiver) =
            unbounded_channel::<Traced<RethServiceMessage>>();
        let (reorg_sender, reorg_receiver) = broadcast::channel::<ReorgEvent>(100);
        let (block_state_sender, block_state_receiver) =
            broadcast::channel::<BlockStateUpdated>(100);
        let (peer_events_sender, peer_events_receiver) = broadcast::channel::<PeerEvent>(100);
        let (peer_network_sender, peer_network_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (block_discovery_sender, block_discovery_receiver) =
            unbounded_channel::<Traced<BlockDiscoveryMessage>>();
        let (packing_sender, packing_receiver) = PackingService::channel(5_000);

        let mining_bus = MiningBus::new();
        let recent_validation_results = Arc::new(RwLock::new(LruCache::new(
            NonZeroUsize::new(RECENT_VALIDATION_RESULTS_CAPACITY)
                .expect("RECENT_VALIDATION_RESULTS_CAPACITY must be > 0"),
        )));
        let senders = Self {
            chunk_cache: chunk_cache_sender,
            chunk_ingress: chunk_ingress_sender,
            chunk_migration: chunk_migration_sender,
            mempool: mempool_sender,
            vdf_fast_forward: vdf_fast_forward_sender,
            storage_modules: sm_sender,
            data_sync: ds_sender,
            gossip_broadcast: gossip_broadcast_sender,
            block_tree: block_tree_sender,
            validation_service: validation_sender,
            block_producer: block_producer_sender,
            reth_service: reth_service_sender,
            reorg_events: reorg_sender,
            block_state_events: block_state_sender,
            peer_events: peer_events_sender,
            peer_network: PeerNetworkSender::new(peer_network_sender),
            block_discovery: block_discovery_sender,
            mining_bus,
            packing_sender,
            recent_validation_results,
        };
        let receivers = ServiceReceivers {
            chunk_cache: chunk_cache_receiver,
            chunk_ingress: chunk_ingress_receiver,
            chunk_migration: chunk_migration_receiver,
            mempool: mempool_receiver,
            vdf_fast_forward: vdf_fast_forward_receiver,
            storage_modules: sm_receiver,
            data_sync: ds_receiver,
            gossip_broadcast: gossip_broadcast_receiver,
            block_tree: block_tree_receiver,
            validation_service: validation_receiver,
            block_producer: block_producer_receiver,
            reth_service: reth_service_receiver,
            reorg_events: reorg_receiver,
            block_state_events: block_state_receiver,
            peer_events: peer_events_receiver,
            peer_network: peer_network_receiver,
            block_discovery: block_discovery_receiver,
            packing: packing_receiver,
        };
        (senders, receivers)
    }

    /// Subscribe to reorg events
    pub fn subscribe_reorgs(&self) -> broadcast::Receiver<ReorgEvent> {
        self.reorg_events.subscribe()
    }

    pub fn subscribe_mining_broadcast(&self) -> UnboundedReceiver<Arc<MiningBroadcastEvent>> {
        self.mining_bus.subscribe()
    }

    /// See `ServiceSenders::recent_validation_result`.
    pub fn recent_validation_result(&self, block_hash: &BlockHash) -> Option<ValidationResult> {
        match self.recent_validation_results.read() {
            // `LruCache::peek` does not promote on read, which is what we
            // want — promotion on the *read* path would let a hot reader
            // pin a stale entry and starve eviction of newer entries; the
            // write path (`record_validation_result`) is what bumps
            // freshness here.
            Ok(guard) => guard.peek(block_hash).cloned(),
            Err(poisoned) => {
                tracing::error!(
                    "recent_validation_results lock poisoned in recent_validation_result; \
                     returning None — fix the panic in the writer"
                );
                poisoned.into_inner().peek(block_hash).cloned()
            }
        }
    }

    /// See `ServiceSenders::record_validation_result`.
    ///
    /// Inserts (or overwrites) the entry under a brief write lock. Must be
    /// called BEFORE broadcasting the corresponding `BlockStateUpdated`
    /// event so a subscriber waking from `recv()` cannot read a stale
    /// `None`. Lock ordering: this lock is leaf — no other locks
    /// (block-tree cache, mempool, etc.) are acquired while holding it.
    pub fn record_validation_result(&self, block_hash: BlockHash, result: ValidationResult) {
        match self.recent_validation_results.write() {
            Ok(mut guard) => {
                guard.put(block_hash, result);
            }
            Err(poisoned) => {
                tracing::error!(
                    "recent_validation_results lock poisoned in record_validation_result; \
                     recovering"
                );
                poisoned.into_inner().put(block_hash, result);
            }
        }
    }
}

/// Waits until no events arrive on `rx` for `idle` duration, bounded by `deadline`.
/// Treats `RecvError::Lagged` as activity (continues waiting).
/// Returns when idle timeout elapses, deadline is reached, or channel closes.
pub async fn wait_until_broadcast_idle<T: Clone>(
    rx: &mut tokio::sync::broadcast::Receiver<T>,
    idle: std::time::Duration,
    deadline: tokio::time::Instant,
) {
    loop {
        match tokio::time::timeout_at(deadline, tokio::time::timeout(idle, rx.recv())).await {
            Ok(Ok(Ok(_))) => continue,
            Ok(Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_)))) => continue,
            Ok(Ok(Err(tokio::sync::broadcast::error::RecvError::Closed))) => break,
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod recent_validation_results_tests {
    //! Tests for the recent-validation-results store on `ServiceSenders`.
    //!
    //! The store seeds new `block_state_events` subscribers with the
    //! parent's last-broadcast `ValidationResult`, closing the race where
    //! the parent's `BlockStateUpdated` fires BEFORE the child subscribes
    //! and is then never delivered (`tokio::sync::broadcast` does not
    //! replay past events).
    use super::*;
    use crate::block_validation::ValidationError;
    use irys_types::H256;

    fn internal_failure_result() -> ValidationResult {
        ValidationError::ParentBlockMissing {
            block_hash: H256::zero(),
        }
        .into()
    }

    /// Direct round-trip: insert then read returns the same variant.
    #[test]
    fn record_then_read_round_trips() {
        let (senders, _receivers) = ServiceSenders::new();
        let hash = H256::random();
        assert!(senders.recent_validation_result(&hash).is_none());

        senders.record_validation_result(hash, ValidationResult::Valid);
        assert!(matches!(
            senders.recent_validation_result(&hash),
            Some(ValidationResult::Valid)
        ));
    }

    /// Race-test for the bug this fix closes: parent fires its event
    /// BEFORE a child subscribes. The broadcast itself doesn't replay,
    /// but the recent-results store still has the entry, so a fresh
    /// subscriber that consults it on entry sees the result.
    ///
    /// Without the fix (no store-write before broadcast send), the
    /// `recent_validation_result` read below would return `None` and the
    /// child would lose the cascade signal.
    #[test]
    fn parent_event_before_child_subscribe_still_observable() {
        let (senders, _receivers) = ServiceSenders::new();
        let parent_hash = H256::random();

        // Simulate the BlockTreeService publish ordering: write store
        // BEFORE broadcast send. We don't actually need to subscribe
        // first to demonstrate the bug — the point is that the broadcast
        // is lossy across subscribe boundaries, but the store is not.
        senders.record_validation_result(parent_hash, internal_failure_result());
        let _ = senders
            .0
            .block_state_events
            .send(crate::block_tree_service::BlockStateUpdated {
                block_hash: parent_hash,
                height: 0,
                state: irys_domain::ChainState::NotOnchain(irys_domain::BlockState::Unknown),
                discarded: false,
                validation_result: internal_failure_result(),
            });

        // A fresh subscriber created AFTER the broadcast can never see
        // the parent's event via `recv()`. But it can see it via the
        // store. This is the seed read that `exit_if_block_is_too_old`
        // performs.
        let _late_rx = senders.subscribe_block_state_updates();
        match senders.recent_validation_result(&parent_hash) {
            Some(ValidationResult::InternalFailure(_)) => {}
            other => panic!(
                "expected Some(InternalFailure(..)) seeded from the store, got {:?}",
                other
            ),
        }
    }

    /// LRU bound: once capacity is exceeded, the oldest entry falls out.
    ///
    /// Use a small explicit cache here (the production capacity of 1024
    /// is too large to stress in a unit test). We can't reach into the
    /// inner `LruCache` directly, so this test asserts the contract by
    /// inserting `RECENT_VALIDATION_RESULTS_CAPACITY + N` distinct
    /// entries and checking the first N have been evicted.
    #[test]
    fn lru_evicts_oldest_when_over_capacity() {
        let (senders, _receivers) = ServiceSenders::new();
        let mut hashes = Vec::with_capacity(RECENT_VALIDATION_RESULTS_CAPACITY + 8);
        for _ in 0..RECENT_VALIDATION_RESULTS_CAPACITY + 8 {
            let h = H256::random();
            hashes.push(h);
            senders.record_validation_result(h, ValidationResult::Valid);
        }
        // The first 8 entries (oldest) must have been evicted to make
        // room for the last 8.
        for h in &hashes[0..8] {
            assert!(
                senders.recent_validation_result(h).is_none(),
                "expected oldest entry {:?} to have been evicted",
                h
            );
        }
        // The last RECENT_VALIDATION_RESULTS_CAPACITY entries must still
        // be present.
        for h in &hashes[8..] {
            assert!(
                senders.recent_validation_result(h).is_some(),
                "expected entry {:?} to still be present",
                h
            );
        }
    }

    /// Overwrite: re-recording the same hash replaces the prior value
    /// (parents move through Scheduled → InternalFailure → eventually
    /// Valid on retry, and the store must track the latest).
    #[test]
    fn record_overwrites_prior_entry() {
        let (senders, _receivers) = ServiceSenders::new();
        let hash = H256::random();

        senders.record_validation_result(hash, internal_failure_result());
        assert!(matches!(
            senders.recent_validation_result(&hash),
            Some(ValidationResult::InternalFailure(_))
        ));

        senders.record_validation_result(hash, ValidationResult::Valid);
        assert!(matches!(
            senders.recent_validation_result(&hash),
            Some(ValidationResult::Valid)
        ));
    }
}
