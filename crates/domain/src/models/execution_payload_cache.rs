use crate::PeerList;
use alloy_primitives::B256;
use alloy_rpc_types::engine::ExecutionData;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use lru::LruCache;
use reth::builder::{BeaconOnNewPayloadError, Block as _};
use reth::consensus::ConsensusError;
use reth::consensus_common::validation::validate_body_against_header;
use reth_ethereum_primitives::Block;
use reth_primitives_traits::{InMemorySize as _, SealedBlock};
#[cfg(feature = "test-utils")]
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, watch};
use tracing::{debug, error, instrument};

const PAYLOAD_CACHE_CAPACITY: usize = 1000;
const PAYLOAD_RECEIVERS_CAPACITY: usize = 1000;
const PAYLOAD_REQUESTS_CACHE_CAPACITY: usize = 1000;
/// Bounds for payloads that arrive before a local request or waiter names
/// their hash (see [`UnsolicitedPayloads`]). Normal operation needs only a
/// few: an EVM payload is usually gossiped shortly before its Irys block.
const UNSOLICITED_PAYLOADS_MAX_COUNT: usize = 64;
const UNSOLICITED_PAYLOADS_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum RethBlockProvider {
    IrysRethAdapter(IrysRethNodeAdapter),
    #[cfg(feature = "test-utils")]
    Mock(Arc<std::sync::RwLock<HashMap<B256, Block>>>),
}

#[derive(Debug)]
pub enum ExecutionPayloadProviderError {
    ProviderError(reth_provider::ProviderError),
    ProviderNotSet,
    PayloadNotFound(B256),
    BeaconOnNewPayloadError(BeaconOnNewPayloadError),
    UpdateForkchoiceError(eyre::Report),
    PayloadValidationError(eyre::Report),
}

/// Error returned when a `wait_for_payload` / `wait_for_sealed_block`
/// call cannot deliver the payload to the caller.
///
/// This does NOT indicate that the execution layer deterministically
/// failed — it indicates a local-cache disruption (`ReceiverDisrupted`)
/// or a bounded wait elapsing without the payload arriving over gossip
/// (`WaitTimeout`). The latter exists because the request-side retry
/// loop in `request_payload_from_the_network` is best-effort: a peer
/// that advertises a block header but never serves the EVM payload
/// would otherwise stall the wait until the `payload_senders` LRU
/// (capacity 1000) eventually evicted the slot — essentially unbounded
/// under low load.
///
/// Callers MUST route both variants as soft local failures (retry via
/// fresh gossip / cache re-entry) and MUST NOT classify them as node
/// faults: the node is healthy, the cache or the peer-network response
/// is just slow.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone, Copy)]
pub enum ExecutionPayloadWaitError {
    /// The `watch::Sender` registered for `evm_block_hash` was dropped
    /// before the payload arrived (LRU eviction or explicit cache
    /// removal). Retry by re-entering the cache after fresh gossip.
    #[error("payload-wait receiver disrupted by local cache for evm_block_hash {evm_block_hash}")]
    ReceiverDisrupted { evm_block_hash: B256 },
    /// The bounded `execution_payload_wait_timeout` elapsed before the
    /// payload arrived. The `watch::Sender` entry is intentionally left
    /// in `payload_senders` — sibling waiters may still be subscribed
    /// to it, and the LRU bounds the orphan-set naturally. Retry by
    /// re-entering the cache; if a fresh subscriber races, it
    /// re-subscribes to the same sender. Diagnostic distinction from
    /// `ReceiverDisrupted` for log-side root-cause analysis (peer
    /// withholding payload vs. local saturation).
    #[error("payload-wait timed out after {elapsed_ms}ms for evm_block_hash {evm_block_hash}")]
    WaitTimeout {
        evm_block_hash: B256,
        elapsed_ms: u64,
    },
}

/// The payload body does not match the commitments in its header
/// (transactions root, ommers hash, withdrawals root). The cache key is the
/// header hash, so such a body must not be stored under it.
#[derive(Debug, thiserror::Error)]
#[error("execution payload {evm_block_hash} body does not match its header: {source}")]
pub struct PayloadBodyMismatchError {
    pub evm_block_hash: B256,
    #[source]
    pub source: ConsensusError,
}

impl From<BeaconOnNewPayloadError> for ExecutionPayloadProviderError {
    fn from(err: BeaconOnNewPayloadError) -> Self {
        Self::BeaconOnNewPayloadError(err)
    }
}

impl RethBlockProvider {
    pub fn new(irys_reth_node_adapter: IrysRethNodeAdapter) -> Self {
        Self::IrysRethAdapter(irys_reth_node_adapter)
    }

    pub fn as_irys_reth_adapter(&self) -> Option<&IrysRethNodeAdapter> {
        match self {
            Self::IrysRethAdapter(adapter) => Some(adapter),
            #[cfg(feature = "test-utils")]
            Self::Mock(_) => None,
        }
    }

    #[cfg(feature = "test-utils")]
    pub fn new_mock() -> Self {
        Self::Mock(Arc::new(std::sync::RwLock::new(HashMap::new())))
    }

    /// Fetches the execution payload for a given EVM block hash. You can get the EVM block hash
    /// from the Irys block header like this:
    /// ```rust
    /// use irys_types::IrysBlockHeader;
    /// let irys_block: IrysBlockHeader = IrysBlockHeader::new_mock_header(); // Obtain the Irys block header
    /// let evm_block_hash = irys_block.evm_block_hash; // Get the EVM block hash
    /// ```
    pub fn evm_block(&self, evm_block_hash: B256) -> Option<Block> {
        match self {
            Self::IrysRethAdapter(adapter) => adapter.evm_block(evm_block_hash),
            #[cfg(feature = "test-utils")]
            Self::Mock(_) => self.evm_block_mock(evm_block_hash),
        }
    }

    #[cfg(feature = "test-utils")]
    pub fn evm_block_mock(&self, evm_block_hash: B256) -> Option<Block> {
        if let Self::Mock(payloads) = self {
            let payloads = payloads.read().expect("can always read");
            payloads.get(&evm_block_hash).cloned()
        } else {
            panic!("Tried to get payload from the mock provider, but it is not a mock provider");
        }
    }
}

impl From<IrysRethNodeAdapter> for RethBlockProvider {
    fn from(irys_adapter: IrysRethNodeAdapter) -> Self {
        Self::new(irys_adapter)
    }
}

/// Per-hash payload notifier kept in [`ExecutionPayloadCache::payload_senders`].
///
/// Was previously `Vec<oneshot::Sender<SealedBlock<Block>>>` to support
/// multiple concurrent waiters for the same EVM payload hash (two
/// validation tasks racing on competing chain extensions that share an
/// EVM ancestor). The Vec backing collided with per-waiter timeout /
/// cancel semantics: any single waiter's path tripping a cleanup would
/// `pop` the entire Vec, dropping every sibling's oneshot sender and
/// surfacing `ReceiverDisrupted` to victims whose own wait was still
/// healthy.
///
/// `watch::Sender<Option<SealedBlock<Block>>>` collapses the fan-out
/// onto a single broadcast slot:
///   * Each waiter calls [`watch::Sender::subscribe`] to get an
///     independent `Receiver`. Dropping a `Receiver` (per-waiter
///     timeout, outer cancel, dropped future) is a no-op for siblings —
///     no cross-contamination.
///   * Wake-side calls [`watch::Sender::send`] exactly once with
///     `Some(payload)`; every currently-subscribed `Receiver` sees the
///     value via `borrow_and_update`. Late-arriving receivers that
///     subscribe AFTER the send still see the value via `borrow`
///     because `watch` retains the most recent value for the lifetime
///     of any `Receiver`.
///   * `Sender` drop (explicit `remove_payload_from_cache` or LRU
///     eviction) surfaces to live receivers as `RecvError` ↦
///     `ExecutionPayloadWaitError::ReceiverDisrupted`, preserving the
///     existing typed-error contract.
type PayloadNotifier = watch::Sender<Option<SealedBlock<Block>>>;

#[derive(Clone, Debug)]
pub struct ExecutionPayloadCache {
    pub reth_payload_provider: RethBlockProvider,
    cache: Arc<RwLock<ExecutionPayloadCacheInner>>,
    /// Per-hash payload-arrival notifiers. See [`PayloadNotifier`] for
    /// the rationale behind the `watch::Sender` choice over the prior
    /// `Vec<oneshot::Sender>` fan-out.
    payload_senders: Arc<RwLock<LruCache<B256, PayloadNotifier>>>,
    peer_list: PeerList,
    /// Bounded wait used by `wait_for_sealed_block` to cap how long the
    /// caller blocks on a payload that may never arrive (peer advertises
    /// the block header but never serves the EVM payload). Sized to
    /// exceed the 50s budget of `request_payload_from_the_network`'s
    /// 10×5s retry loop so the request side completes first under
    /// normal conditions.
    wait_timeout: Duration,
}

impl ExecutionPayloadCache {
    pub fn new(
        peer_list: PeerList,
        reth_payload_provider: RethBlockProvider,
        wait_timeout: Duration,
    ) -> Self {
        Self {
            cache: Arc::new(RwLock::new(ExecutionPayloadCacheInner {
                payloads: LruCache::new(NonZeroUsize::new(PAYLOAD_CACHE_CAPACITY).expect("payload capacity is not a non-zero usize")),
                unsolicited_payloads: UnsolicitedPayloads::new(),
                payloads_currently_requested_from_the_network: LruCache::new(NonZeroUsize::new(PAYLOAD_REQUESTS_CACHE_CAPACITY).expect("payloads currently requested from the network capacity is not a non-zero usize")),
            })),
            // TODO: fix this to use a real RPC client
            reth_payload_provider,
            payload_senders: Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(PAYLOAD_RECEIVERS_CAPACITY).expect("payload senders capacity is not a non-zero usize")))),
            peer_list,
            wait_timeout,
        }
    }

    /// Adds a payload to the cache and wakes the waiters for its hash.
    ///
    /// The payload is keyed by its header hash, which commits to the body
    /// only through the header's transactions root, ommers hash and
    /// withdrawals root. A body that does not match these is rejected and
    /// nothing is stored, so a cached payload is always the exact EVM block
    /// its hash names and a later correct copy is still accepted.
    ///
    /// A payload whose hash a local waiter or network request names goes to
    /// the requested tier. Any other payload goes to the bounded
    /// [`UnsolicitedPayloads`] tier, so payloads that nothing references
    /// cannot grow memory without bound or push out requested payloads.
    pub async fn add_payload_to_cache(
        &self,
        sealed_block: SealedBlock<Block>,
    ) -> Result<(), PayloadBodyMismatchError> {
        let evm_block_hash = sealed_block.hash();
        validate_body_against_header(sealed_block.body(), sealed_block.header()).map_err(
            |source| PayloadBodyMismatchError {
                evm_block_hash,
                source,
            },
        )?;
        let has_waiter = self.payload_senders.read().await.contains(&evm_block_hash);
        {
            debug!("Adding execution payload to cache: {:?}", evm_block_hash);
            let mut cache = self.cache.write().await;
            let was_requested = cache
                .payloads_currently_requested_from_the_network
                .pop(&evm_block_hash)
                .is_some();
            if has_waiter || was_requested || cache.payloads.contains(&evm_block_hash) {
                cache.unsolicited_payloads.remove(&evm_block_hash);
                cache.payloads.put(evm_block_hash, sealed_block.clone());
            } else {
                cache
                    .unsolicited_payloads
                    .insert(evm_block_hash, sealed_block.clone());
            }
        }
        // Wake-side fan-out. Single broadcast onto the watch slot reaches
        // every currently-subscribed waiter; the Sender is then dropped
        // (via `pop`). Late-subscribing waiters that race the pop still
        // see the value through `borrow` on their `Receiver` because the
        // watch channel retains the most-recent value for the lifetime
        // of any `Receiver` reference. Send-error here only when no
        // `Receiver` exists at the moment of send AND no receiver
        // subscribed before drop — which is the normal "payload arrived
        // before anyone asked" path; not an anomaly, hence no warn.
        if let Some(sender) = self.payload_senders.write().await.pop(&evm_block_hash) {
            let _ = sender.send(Some(sealed_block.clone()));
        }
        Ok(())
    }

    /// Records that this node asked the network for `evm_block_hash`. The
    /// payload then goes to the requested tier when it arrives, and a copy
    /// already marked as seen by gossip is still accepted.
    pub async fn mark_payload_as_requested(&self, evm_block_hash: B256) {
        self.cache
            .write()
            .await
            .payloads_currently_requested_from_the_network
            .put(evm_block_hash, ());
    }

    pub async fn remove_payload_from_cache(&self, evm_block_hash: &B256) {
        debug!(
            "Removing execution payload from cache: {:?}",
            evm_block_hash
        );
        let mut cache = self.cache.write().await;
        cache.payloads.pop(evm_block_hash);
        cache.unsolicited_payloads.remove(evm_block_hash);
        cache
            .payloads_currently_requested_from_the_network
            .pop(evm_block_hash);
        self.payload_senders.write().await.pop(evm_block_hash);
    }

    /// DO NOT USE THIS METHOD ANYWHERE WHERE YOU NEED TO RELIABLY GET THE PAYLOAD!
    /// Use [ExecutionPayloadCache::wait_for_payload] instead.
    /// This method is used to retrieve the payload from the local cache or EVM node.
    pub async fn get_locally_stored_evm_block(&self, evm_block_hash: &B256) -> Option<Block> {
        if let Some(sealed_block) = self.cached_payload(evm_block_hash, false).await {
            Some(sealed_block.into_block())
        } else {
            self.reth_payload_provider.evm_block(*evm_block_hash)
        }
    }

    /// Tries to get the sealed block from the local cache first, and if not found, fetches it from
    /// the EVM node. This method does not request the payload from the network if it is not found
    /// locally, use [ExecutionPayloadCache::wait_for_sealed_block] instead if you want to
    /// request the payload from the network.
    pub async fn get_sealed_block_from_cache(
        &self,
        evm_block_hash: &B256,
    ) -> Option<SealedBlock<Block>> {
        self.get_sealed_block(evm_block_hash, false).await
    }

    /// See [`Self::get_sealed_block_from_cache`]. With `referenced`, the
    /// caller holds an Irys block that names this hash, so a hit in the
    /// unsolicited tier moves to the requested tier.
    async fn get_sealed_block(
        &self,
        evm_block_hash: &B256,
        referenced: bool,
    ) -> Option<SealedBlock<Block>> {
        let maybe_sealed = self.cached_payload(evm_block_hash, referenced).await;

        if let Some(s) = maybe_sealed {
            Some(s)
        } else {
            let block = self.reth_payload_provider.evm_block(*evm_block_hash)?;
            Some(block.seal_slow())
        }
    }

    /// Looks the payload up in both tiers. With `promote`, a hit in the
    /// unsolicited tier moves to the requested tier.
    async fn cached_payload(
        &self,
        evm_block_hash: &B256,
        promote: bool,
    ) -> Option<SealedBlock<Block>> {
        let mut cache = self.cache.write().await;
        if let Some(sealed_block) = cache.payloads.get(evm_block_hash) {
            return Some(sealed_block.clone());
        }
        if !promote {
            return cache.unsolicited_payloads.get(evm_block_hash).cloned();
        }
        let sealed_block = cache.unsolicited_payloads.remove(evm_block_hash)?;
        cache.payloads.put(*evm_block_hash, sealed_block.clone());
        Some(sealed_block)
    }

    /// Checks if the execution payload is stored in the EVM node.
    pub fn is_stored_in_reth(&self, evm_block_hash: &B256) -> bool {
        self.reth_payload_provider
            .evm_block(*evm_block_hash)
            .is_some()
    }

    /// Waits for the execution payload to arrive over gossip. This method will first check the local
    /// cache, then try to retrieve the payload from the network if it is not found locally.
    ///
    /// Returns `Err(ExecutionPayloadWaitError::ReceiverDisrupted { .. })` if
    /// the in-process oneshot wiring is torn down before the payload arrives
    /// (LRU eviction from `payload_senders` under heavy catch-up sync, or an
    /// explicit `remove_payload_from_cache` call for the same hash).
    ///
    /// Returns `Err(ExecutionPayloadWaitError::WaitTimeout { .. })` if the
    /// configured `execution_payload_wait_timeout` elapses before the
    /// payload arrives. This caps the previously-unbounded wait against a
    /// peer that advertises a header but never serves the EVM payload.
    ///
    /// Both variants are local-cache or local-wait disruption signals, NOT
    /// an execution-layer fault — see the doc on
    /// [`ExecutionPayloadWaitError`] for the caller contract.
    ///
    /// You can get the EVM block hash from the Irys block header like this:
    /// ```rust
    /// use irys_types::IrysBlockHeader;
    /// let irys_block = IrysBlockHeader::new_mock_header();
    /// let evm_block_hash = irys_block.evm_block_hash;
    /// ```
    pub async fn wait_for_payload(
        &self,
        evm_block_hash: &B256,
    ) -> Result<ExecutionData, ExecutionPayloadWaitError> {
        let sealed_block = self.wait_for_sealed_block(evm_block_hash, false).await?;
        Ok(<<irys_reth_node_bridge::irys_reth::IrysEthereumNode as reth::api::NodeTypes>::Payload as reth::api::PayloadTypes>::block_to_payload(sealed_block))
    }

    /// Same as [ExecutionPayloadCache::wait_for_payload], but returns the sealed block instead
    /// of the execution data. See [`Self::wait_for_payload`] for the error contract.
    pub async fn wait_for_sealed_block(
        &self,
        evm_block_hash: &B256,
        request_only_from_trusted_peers: bool,
    ) -> Result<SealedBlock<Block>, ExecutionPayloadWaitError> {
        // The caller waits for this hash on behalf of an Irys block that
        // names it, so a payload that arrived early is promoted out of the
        // unsolicited tier.
        if let Some(sealed_block) = self.get_sealed_block(evm_block_hash, true).await {
            return Ok(sealed_block);
        }

        let mut receiver = self.block_receiver(*evm_block_hash).await;

        // Close the lost-notify window: a payload may have arrived between
        // the initial cache check above and `block_receiver` subscribing to
        // the notifier. If that happened, the wake-side already fired (and
        // dropped) any sender that existed pre-subscribe; our just-created
        // (or freshly-subscribed) receiver would otherwise block until LRU
        // eviction of `payload_senders` (up to `PAYLOAD_RECEIVERS_CAPACITY`
        // distinct hashes later under sync load) — recoverable, but adds
        // large unnecessary tail latency. The second cache check is cheap,
        // idempotent, and turns a worst-case multi-second stall into an
        // immediate hit.
        //
        // Unlike the pre-watch implementation, we do NOT pop the
        // `payload_senders` entry on this fast-return path. Popping would
        // drop the `watch::Sender` and surface a spurious `ReceiverDisrupted`
        // to any sibling waiter currently parked on `changed().await` for
        // the same hash. Leaving the sender alone is safe: dropping our own
        // `Receiver` here is a no-op for siblings, and the orphaned sender
        // is bounded by the LRU capacity (1000) and natural eviction.
        if let Some(sealed_block) = self.get_sealed_block(evm_block_hash, true).await {
            drop(receiver);
            return Ok(sealed_block);
        }

        self.request_payload_from_the_network(*evm_block_hash, request_only_from_trusted_peers)
            .await;
        // Wrap the wait in `tokio::time::timeout` so a peer that advertises
        // a block header but never serves the EVM payload cannot stall the
        // validation task until the `payload_senders` LRU happens to evict
        // our slot (essentially unbounded under low load). With the
        // `watch::Sender` notifier, `changed().await` resolves either when
        // the sender broadcasts `Some(payload)` (wake-side fan-out) or when
        // the sender is dropped (LRU eviction / explicit cache removal),
        // the latter surfaced as `ReceiverDisrupted`. A `timeout`-arm trip
        // becomes the distinct `WaitTimeout` so operators can root-cause
        // from logs. Both error variants map to
        // `ValidationError::ExecutionPayloadCacheEvicted` downstream — see
        // `ExecutionPayloadWaitError` doc for the caller contract.
        //
        // Crucially, the timeout arm here does NOT touch `payload_senders`.
        // The prior implementation popped the entire fan-out `Vec` on
        // timeout, dropping every sibling waiter's oneshot sender and
        // surfacing spurious `ReceiverDisrupted` to victims whose own wait
        // was still healthy. With the `watch` design, dropping THIS
        // receiver is isolated to this waiter; sibling receivers continue
        // to observe the sender unaffected. The orphaned sender entry (if
        // we were the last subscriber) is bounded by the LRU and will be
        // evicted naturally — well below the cost of breaking sibling
        // waiters.
        //
        // DEFERRED: wrapping `request_payload_from_the_network` inside this
        // same timeout would bound the worst-case latency from
        // `request_budget + wait_timeout` (50s+60s=110s in prod) to
        // `wait_timeout` (60s). It is deferred because (a) the larger
        // worst-case only fires in already-degraded peer-network states,
        // and (b) several integration tests rely on the wider effective
        // budget to feed payloads after validation starts — the request
        // budget gives them a ~55s window even with the 5s test
        // `wait_timeout`. A real fix needs to retune
        // `NodeConfig::testing()`'s `execution_payload_wait_timeout_millis`
        // alongside the move.
        let started = std::time::Instant::now();
        match tokio::time::timeout(self.wait_timeout, async {
            loop {
                receiver.changed().await.map_err(|_| {
                    ExecutionPayloadWaitError::ReceiverDisrupted {
                        evm_block_hash: *evm_block_hash,
                    }
                })?;
                if let Some(sealed_block) = receiver.borrow_and_update().clone() {
                    return Ok(sealed_block);
                }
                // `None` is the channel's initial value; only a `Some(_)`
                // send constitutes payload arrival. Defensive — current
                // wake-side never sends `None`, but the loop guards
                // against a future regression.
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                // We hit the documented worst case: peer advertised the
                // header (or the request-retry budget elapsed without anyone
                // ACKing) but the payload never arrived within `wait_timeout`.
                // The layered budget (request_budget ~50s + wait_timeout
                // ~60s) is intentional — see the DEFERRED block above — but
                // a tripped wait_timeout in prod is interesting enough that
                // operators want it surfaced above the default log level.
                tracing::warn!(
                    evm_block_hash = ?evm_block_hash,
                    elapsed_ms,
                    wait_timeout_ms = u64::try_from(self.wait_timeout.as_millis()).unwrap_or(u64::MAX),
                    "execution_payload_cache: wait_for_sealed_block hit WaitTimeout — payload-withholding lane (block parks for retry, not a node fault)"
                );
                Err(ExecutionPayloadWaitError::WaitTimeout {
                    evm_block_hash: *evm_block_hash,
                    elapsed_ms,
                })
            }
        }
    }

    pub async fn is_payload_in_cache(&self, evm_block_hash: &B256) -> bool {
        let cache = self.cache.read().await;
        cache.payloads.contains(evm_block_hash)
            || cache.unsolicited_payloads.contains(evm_block_hash)
    }

    #[instrument(skip_all)]
    pub async fn request_payload_from_the_network(
        &self,
        evm_block_hash: B256,
        use_trusted_peers_only: bool,
    ) {
        let mut max_iterations = 10;
        loop {
            if max_iterations == 0 {
                break;
            }
            self.cache
                .write()
                .await
                .payloads_currently_requested_from_the_network
                .put(evm_block_hash, ());
            if let Err(peer_list_error) = self
                .peer_list
                .request_payload_from_the_network(evm_block_hash, use_trusted_peers_only)
                .await
            {
                self.cache
                    .write()
                    .await
                    .payloads_currently_requested_from_the_network
                    .pop(&evm_block_hash);
                error!(
                    "Failed to request execution payload from the network: {:?}",
                    peer_list_error
                );
                // try re-requesting from the network after a short while
                tokio::time::sleep(Duration::from_secs(5)).await;
                max_iterations -= 1;
                continue;
            }
            break;
        }
    }

    pub async fn is_waiting_for_payload(&self, evm_block_hash: &B256) -> bool {
        self.cache
            .read()
            .await
            .payloads_currently_requested_from_the_network
            .contains(evm_block_hash)
    }

    /// Subscribe to the per-hash payload-arrival notifier, creating it
    /// if absent. Returns a `watch::Receiver` whose `changed().await`
    /// resolves when `add_payload_to_cache` fires the notifier (or
    /// `RecvError` if the sender is dropped by `remove_payload_from_cache`
    /// / LRU eviction).
    async fn block_receiver(
        &self,
        evm_block_hash: B256,
    ) -> watch::Receiver<Option<SealedBlock<Block>>> {
        let mut senders = self.payload_senders.write().await;
        if let Some(sender) = senders.get(&evm_block_hash) {
            return sender.subscribe();
        }
        let (sender, receiver) = watch::channel(None);
        senders.push(evm_block_hash, sender);
        receiver
    }

    #[cfg(feature = "test-utils")]
    pub async fn test_observe_sealed_block_arrival(
        &self,
        evm_block_hash: B256,
        timeout: std::time::Duration,
    ) -> eyre::Result<()> {
        if self
            .get_sealed_block_from_cache(&evm_block_hash)
            .await
            .is_none()
        {
            let mut receiver = self.block_receiver(evm_block_hash).await;

            // Close the lost-notify window — mirrors `wait_for_sealed_block`
            // (see the comment block there for the full rationale): a
            // payload may have arrived between the initial cache check
            // above and `block_receiver` subscribing to the notifier. If
            // it did, the wake-side already popped + dropped the sender;
            // this freshly-subscribed `Receiver` would otherwise block on
            // `changed().await` until the timeout fires even though the
            // payload is already sitting in `cache.payloads`. Re-checking
            // the cache here turns that timeout into an immediate hit.
            if self
                .get_sealed_block_from_cache(&evm_block_hash)
                .await
                .is_some()
            {
                drop(receiver);
                return Ok(());
            }

            tokio::time::timeout(timeout, async {
                loop {
                    receiver.changed().await.map_err(|err| {
                        eyre::eyre!(
                            "sealed block observer dropped before {evm_block_hash:?} arrived: {err}"
                        )
                    })?;
                    if receiver.borrow_and_update().is_some() {
                        return Ok::<_, eyre::Report>(());
                    }
                }
            })
            .await
            .map_err(|_| eyre::eyre!("timed out waiting for sealed block {evm_block_hash:?}"))??;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ExecutionPayloadCacheInner {
    /// Payloads whose hash a local waiter or network request named.
    payloads: LruCache<B256, SealedBlock<Block>>,
    unsolicited_payloads: UnsolicitedPayloads,
    payloads_currently_requested_from_the_network: LruCache<B256, ()>,
}

/// Payloads that arrived before a local waiter or network request named
/// their hash: usually an EVM payload gossiped before its Irys block. Any
/// peer can send these, so this tier is bounded by count and by total
/// in-memory size, and it is separate from the requested tier so that it
/// cannot push out payloads that validation waits for.
///
/// An evicted payload is not lost for good: validation of the Irys block
/// that names it requests it from the network again.
#[derive(Debug)]
struct UnsolicitedPayloads {
    /// Each entry keeps its in-memory size for the byte accounting.
    entries: LruCache<B256, (SealedBlock<Block>, usize)>,
    total_bytes: usize,
}

impl UnsolicitedPayloads {
    fn new() -> Self {
        Self {
            entries: LruCache::new(
                NonZeroUsize::new(UNSOLICITED_PAYLOADS_MAX_COUNT)
                    .expect("unsolicited payloads capacity is not a non-zero usize"),
            ),
            total_bytes: 0,
        }
    }

    fn insert(&mut self, evm_block_hash: B256, sealed_block: SealedBlock<Block>) {
        let size = sealed_block.size();
        // `push` returns the replaced entry for the same hash, or the entry
        // evicted by the count bound.
        if let Some((_, (_, replaced_size))) =
            self.entries.push(evm_block_hash, (sealed_block, size))
        {
            self.total_bytes -= replaced_size;
        }
        self.total_bytes += size;
        // The newest entry always stays, so a single payload larger than
        // the byte bound is still kept.
        while self.total_bytes > UNSOLICITED_PAYLOADS_MAX_BYTES && self.entries.len() > 1 {
            if let Some((_, (_, evicted_size))) = self.entries.pop_lru() {
                self.total_bytes -= evicted_size;
            }
        }
    }

    fn get(&self, evm_block_hash: &B256) -> Option<&SealedBlock<Block>> {
        self.entries
            .peek(evm_block_hash)
            .map(|(sealed_block, _)| sealed_block)
    }

    fn remove(&mut self, evm_block_hash: &B256) -> Option<SealedBlock<Block>> {
        let (sealed_block, size) = self.entries.pop(evm_block_hash)?;
        self.total_bytes -= size;
        Some(sealed_block)
    }

    fn contains(&self, evm_block_hash: &B256) -> bool {
        self.entries.contains(evm_block_hash)
    }
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::PeerList;
    use alloy_primitives::U256;
    use reth_ethereum_primitives::BlockBody;
    use reth_primitives_traits::Header;

    /// Default wait timeout used by these unit tests. Generous enough to
    /// keep the `ReceiverDisrupted` test from racing the new timeout
    /// branch on a slow CI runner, short enough to keep the timeout test
    /// snappy.
    const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

    /// Shared helper that mirrors the production
    /// `wait_for_sealed_block` notify-loop against a raw
    /// `watch::Receiver`, so unit tests can pin the typed-error
    /// contract without going through `request_payload_from_the_network`'s
    /// retry-with-sleep loop against the no-op mock peer-network sender.
    async fn await_receiver(
        mut receiver: watch::Receiver<Option<SealedBlock<Block>>>,
        wait_timeout: Duration,
        evm_block_hash: B256,
    ) -> Result<SealedBlock<Block>, ExecutionPayloadWaitError> {
        let started = std::time::Instant::now();
        match tokio::time::timeout(wait_timeout, async {
            loop {
                receiver
                    .changed()
                    .await
                    .map_err(|_| ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash })?;
                if let Some(sealed_block) = receiver.borrow_and_update().clone() {
                    return Ok(sealed_block);
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ExecutionPayloadWaitError::WaitTimeout {
                evm_block_hash,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }),
        }
    }

    /// Regression for the cache-eviction → `ExecutionPayloadUnavailable`
    /// → `is_node_fault` → panic loop on healthy nodes under catch-up
    /// sync. Verifies that when the watch sender registered for an
    /// in-flight payload wait is dropped — by `remove_payload_from_cache`,
    /// semantically equivalent to the `payload_senders` LRU evicting our
    /// slot under heavy sync pressure — the wait surfaces the typed
    /// `ReceiverDisrupted` error rather than collapsing the disruption
    /// signal into `None`.
    #[tokio::test]
    async fn wait_for_payload_returns_receiver_disrupted_on_eviction() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        let evm_block_hash = B256::repeat_byte(0x42);

        // Subscribe to the per-hash notifier — same path
        // `wait_for_sealed_block` takes after a cache miss.
        let receiver = cache.block_receiver(evm_block_hash).await;
        assert!(
            cache.payload_senders.read().await.contains(&evm_block_hash),
            "block_receiver must register a sender for the hash",
        );

        // Drop the sender. Semantically equivalent to the
        // `payload_senders` LRU evicting this slot under
        // catch-up-sync pressure.
        cache.remove_payload_from_cache(&evm_block_hash).await;

        let result = await_receiver(receiver, TEST_WAIT_TIMEOUT, evm_block_hash).await;

        assert_eq!(
            result.unwrap_err(),
            ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash },
            "cache-eviction disruption must surface as the typed soft error, \
             NOT collapse into a generic 'payload unavailable' / 'EL failed' signal",
        );
    }

    /// Verifies the bounded wait. When the configured `wait_timeout`
    /// elapses before the payload arrives (peer advertised the header
    /// but never served the EVM payload), the wait surfaces
    /// `WaitTimeout` distinct from `ReceiverDisrupted`. The sender
    /// entry is INTENTIONALLY left in `payload_senders` on this path —
    /// dropping it would invalidate sibling waiters that subscribed to
    /// the same notifier; LRU handles eviction. See the
    /// `per_waiter_timeout_does_not_disturb_siblings` test below for
    /// the isolation invariant this enables.
    #[tokio::test]
    async fn wait_for_payload_returns_wait_timeout_when_payload_never_arrives() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let wait_timeout = Duration::from_millis(50);
        let cache = ExecutionPayloadCache::new(peer_list, provider, wait_timeout);

        let evm_block_hash = B256::repeat_byte(0x77);

        // Subscribe to the per-hash notifier — same path
        // `wait_for_sealed_block` takes after a cache miss. Sender
        // stays alive (and never fires) so the wait must hit the
        // timeout arm.
        let receiver = cache.block_receiver(evm_block_hash).await;
        assert!(
            cache.payload_senders.read().await.contains(&evm_block_hash),
            "block_receiver must register a sender for the hash",
        );

        let result = await_receiver(receiver, wait_timeout, evm_block_hash).await;

        match result.unwrap_err() {
            ExecutionPayloadWaitError::WaitTimeout {
                evm_block_hash: hash,
                elapsed_ms,
            } => {
                assert_eq!(hash, evm_block_hash, "WaitTimeout must carry the hash");
                assert!(
                    elapsed_ms >= wait_timeout.as_millis() as u64,
                    "elapsed_ms ({elapsed_ms}) must be >= wait_timeout ({}ms)",
                    wait_timeout.as_millis()
                );
            }
            other => panic!("expected WaitTimeout, got {other:?}"),
        }
    }

    /// Happy path: payload arrives before the wait timeout fires.
    /// Exercises the real `wait_for_sealed_block` against an already-
    /// populated cache so the network-request retry loop is skipped
    /// and the test is hermetic.
    #[tokio::test]
    async fn wait_for_payload_returns_payload_when_it_arrives_in_time() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        // Seal a default block so we have a real `SealedBlock` to push
        // through the cache.
        let block = Block::default();
        let sealed: SealedBlock<Block> = block.seal_slow();
        let evm_block_hash = sealed.hash();

        cache
            .add_payload_to_cache(sealed.clone())
            .await
            .expect("payload body matches its header");

        let started = std::time::Instant::now();
        let got = cache
            .wait_for_sealed_block(&evm_block_hash, false)
            .await
            .expect("payload is already in cache; wait must succeed");
        assert!(
            started.elapsed() < TEST_WAIT_TIMEOUT,
            "happy path must return well before the wait timeout fires",
        );
        assert_eq!(
            got.hash(),
            evm_block_hash,
            "wait must return the payload we just inserted",
        );
    }

    /// Sibling-waiter isolation regression: two concurrent waiters
    /// subscribe to the same hash. One waiter's `Receiver` is dropped
    /// (simulates per-waiter timeout or outer-cancel of a
    /// `shadow_tx_task`). The surviving sibling must remain valid — its
    /// wait must still resolve when the payload arrives, NOT spuriously
    /// surface `ReceiverDisrupted`.
    ///
    /// Under the prior `Vec<oneshot::Sender>` design, any single
    /// waiter's cleanup path popped the whole Vec, dropping every
    /// sibling's sender. This test pins that invariant under the
    /// `watch::Sender` design.
    #[tokio::test]
    async fn per_waiter_timeout_does_not_disturb_siblings() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        let block = Block::default();
        let sealed: SealedBlock<Block> = block.seal_slow();
        let evm_block_hash = sealed.hash();

        // Two independent subscribers for the same hash, simulating
        // two validation tasks racing on competing chain extensions
        // that share an EVM ancestor.
        let waiter_a = cache.block_receiver(evm_block_hash).await;
        let waiter_b = cache.block_receiver(evm_block_hash).await;

        // Simulate waiter_a being cancelled (outer cancel or
        // per-waiter timeout — both manifest as `Receiver` drop).
        drop(waiter_a);

        // The sender entry MUST still be present for waiter_b.
        assert!(
            cache.payload_senders.read().await.contains(&evm_block_hash),
            "dropping one Receiver must not evict the sender — sibling waiters depend on it",
        );

        // Deliver the payload. waiter_b must observe it.
        cache
            .add_payload_to_cache(sealed.clone())
            .await
            .expect("payload body matches its header");

        let result = await_receiver(waiter_b, TEST_WAIT_TIMEOUT, evm_block_hash).await;
        let delivered = result.expect("sibling waiter must resolve when payload arrives");
        assert_eq!(
            delivered.hash(),
            evm_block_hash,
            "sibling waiter must receive the delivered payload, not a disruption error",
        );
    }

    /// Sender-orphan invariant: dropping a `Receiver` (outer-cancel of
    /// an in-flight `shadow_tx_task` mid-`wait_for_payload`) leaves no
    /// orphaned state that would invalidate a subsequently-spawned
    /// waiter on the same hash. The sender entry persists in the LRU
    /// (cleared by `add_payload_to_cache` on payload arrival), and a
    /// fresh subscriber sees the eventual payload normally.
    #[tokio::test]
    async fn dropped_receiver_does_not_orphan_state_for_future_waiters() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        let block = Block::default();
        let sealed: SealedBlock<Block> = block.seal_slow();
        let evm_block_hash = sealed.hash();

        // First waiter subscribes, then is cancelled.
        let waiter_a = cache.block_receiver(evm_block_hash).await;
        drop(waiter_a);

        // Orphan invariant: dropping `waiter_a` must NOT evict the sender
        // entry. This is the load-bearing assertion for the test's name —
        // if a future regression makes `Receiver::drop` also drop the
        // sender, `waiter_b` below would silently subscribe to a freshly
        // created notifier (still passing the rest of this test, but
        // racing the real `wait_for_sealed_block` path against a sender
        // that may have already fired + been popped). Pin it explicitly
        // so that class of regression surfaces here rather than as a
        // flaky `ReceiverDisrupted` in production.
        assert!(
            cache.payload_senders.read().await.contains(&evm_block_hash),
            "sender entry must persist after Receiver drop — future waiters depend on subscribing to the SAME notifier, not a fresh one",
        );

        // Sender entry persists; a second waiter must be able to
        // subscribe to the same notifier (rather than racing a fresh
        // one against a sender that was prematurely dropped).
        let waiter_b = cache.block_receiver(evm_block_hash).await;

        // Deliver the payload. waiter_b must observe it normally.
        cache
            .add_payload_to_cache(sealed.clone())
            .await
            .expect("payload body matches its header");
        let result = await_receiver(waiter_b, TEST_WAIT_TIMEOUT, evm_block_hash).await;
        let delivered = result.expect("second waiter must resolve after first waiter cancelled");
        assert_eq!(delivered.hash(), evm_block_hash);

        // Wake-side popped the sender on delivery — future cache-miss
        // waiters fall through to the normal `get_or_insert_with` path.
        assert!(
            !cache.payload_senders.read().await.contains(&evm_block_hash),
            "delivery must pop the sender entry so the LRU does not leak",
        );
    }

    /// The watch channel retains the broadcast value across `Sender`
    /// drop. Wake-side flow: pop sender out of LRU → `send(Some(payload))`
    /// → sender goes out of scope and drops. A `Receiver` that
    /// subscribed before the pop must still observe the value (it
    /// shares the underlying `Arc<Shared>` independent of the
    /// `Sender`'s lifetime). Without this retention guarantee the
    /// wake-side fan-out would race `Receiver::changed().await`
    /// against `Sender` drop and surface spurious `RecvError`.
    #[tokio::test]
    async fn watch_retention_survives_sender_drop_after_send() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        let block = Block::default();
        let sealed: SealedBlock<Block> = block.seal_slow();
        let evm_block_hash = sealed.hash();

        // A waiter subscribes BEFORE delivery.
        let mut waiter = cache.block_receiver(evm_block_hash).await;

        // Deliver. The wake-side pops the sender from the LRU after
        // broadcasting `Some(payload)`. `waiter` still holds a
        // `Receiver` clone of the underlying `Shared`, so it sees the
        // value even after the `Sender` is dropped.
        cache
            .add_payload_to_cache(sealed.clone())
            .await
            .expect("payload body matches its header");

        // First `changed()` returns Ok (version bumped by send).
        waiter
            .changed()
            .await
            .expect("send + sender-drop sequence must surface the value, not RecvError");
        let observed = waiter.borrow_and_update().clone();
        assert_eq!(
            observed.map(|s| s.hash()),
            Some(evm_block_hash),
            "watch retention must hand the late waiter the delivered payload",
        );
    }

    fn test_cache() -> ExecutionPayloadCache {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        ExecutionPayloadCache::new(peer_list, RethBlockProvider::new_mock(), TEST_WAIT_TIMEOUT)
    }

    fn block_with_header(header: Header) -> Block {
        Block {
            header,
            body: BlockBody::default(),
        }
    }

    fn block_with_number(number: u64) -> SealedBlock<Block> {
        block_with_header(Header {
            number,
            ..Default::default()
        })
        .seal_slow()
    }

    async fn in_requested_tier(cache: &ExecutionPayloadCache, evm_block_hash: &B256) -> bool {
        cache.cache.read().await.payloads.contains(evm_block_hash)
    }

    async fn add_unsolicited_payloads(
        cache: &ExecutionPayloadCache,
        numbers: std::ops::Range<u64>,
    ) {
        for number in numbers {
            cache
                .add_payload_to_cache(block_with_number(number))
                .await
                .expect("payload body matches its header");
        }
    }

    #[tokio::test]
    async fn payload_with_mismatched_transactions_root_is_not_cached() {
        let cache = test_cache();
        let sealed = block_with_header(Header {
            transactions_root: B256::repeat_byte(0x11),
            ..Default::default()
        })
        .seal_slow();
        let evm_block_hash = sealed.hash();

        let err = cache
            .add_payload_to_cache(sealed)
            .await
            .expect_err("empty body does not match a non-empty transactions root");
        assert_eq!(err.evm_block_hash, evm_block_hash);
        assert!(matches!(
            err.source,
            ConsensusError::BodyTransactionRootDiff(_)
        ));
        assert!(!cache.is_payload_in_cache(&evm_block_hash).await);
    }

    /// A body that does not match the header shares the header hash with the
    /// correct body. Rejecting it must leave the hash free for the correct copy.
    #[tokio::test]
    async fn matching_payload_is_cached_after_a_mismatched_copy() {
        let cache = test_cache();
        let ommers = vec![Header::default()];
        let header = Header {
            ommers_hash: reth_primitives_traits::proofs::calculate_ommers_root(&ommers),
            ..Default::default()
        };
        let matching = Block {
            header: header.clone(),
            body: BlockBody {
                ommers,
                ..Default::default()
            },
        }
        .seal_slow();
        let mismatched = block_with_header(header).seal_slow();
        let evm_block_hash = matching.hash();
        assert_eq!(
            mismatched.hash(),
            evm_block_hash,
            "both copies share the header"
        );

        let err = cache
            .add_payload_to_cache(mismatched)
            .await
            .expect_err("empty ommers do not match the header ommers hash");
        assert!(matches!(err.source, ConsensusError::BodyOmmersHashDiff(_)));
        assert!(!cache.is_payload_in_cache(&evm_block_hash).await);

        cache
            .add_payload_to_cache(matching.clone())
            .await
            .expect("matching body is accepted");
        let cached = cache
            .get_sealed_block_from_cache(&evm_block_hash)
            .await
            .expect("matching payload is cached");
        assert_eq!(cached, matching);
    }

    #[tokio::test]
    async fn unsolicited_payloads_do_not_evict_requested_payloads() {
        let cache = test_cache();
        let requested = block_with_number(1);
        let waited_for = block_with_number(2);

        cache.mark_payload_as_requested(requested.hash()).await;
        let _waiter = cache.block_receiver(waited_for.hash()).await;
        cache
            .add_payload_to_cache(requested.clone())
            .await
            .expect("payload body matches its header");
        cache
            .add_payload_to_cache(waited_for.clone())
            .await
            .expect("payload body matches its header");
        assert!(in_requested_tier(&cache, &requested.hash()).await);
        assert!(in_requested_tier(&cache, &waited_for.hash()).await);

        let unsolicited_count = 2 * UNSOLICITED_PAYLOADS_MAX_COUNT as u64;
        add_unsolicited_payloads(&cache, 1000..1000 + unsolicited_count).await;

        assert!(cache.is_payload_in_cache(&requested.hash()).await);
        assert!(cache.is_payload_in_cache(&waited_for.hash()).await);
        assert_eq!(
            cache.cache.read().await.unsolicited_payloads.entries.len(),
            UNSOLICITED_PAYLOADS_MAX_COUNT,
        );
        assert!(
            !cache
                .is_payload_in_cache(&block_with_number(1000).hash())
                .await,
            "the oldest unsolicited payload is evicted by the count bound",
        );
        assert!(
            cache
                .is_payload_in_cache(&block_with_number(1000 + unsolicited_count - 1).hash())
                .await,
            "the newest unsolicited payload is kept",
        );
    }

    #[tokio::test]
    async fn unsolicited_payloads_are_bounded_by_total_size() {
        let cache = test_cache();
        // `new_unchecked` with a distinct hash skips hashing the large
        // extra data; the bound depends only on the size.
        let large_block = |number: u64, extra_data_len: usize| {
            let block = block_with_header(Header {
                number,
                extra_data: vec![0_u8; extra_data_len].into(),
                ..Default::default()
            });
            SealedBlock::new_unchecked(block, B256::from(U256::from(number)))
        };
        let quarter_of_bound = UNSOLICITED_PAYLOADS_MAX_BYTES / 4;
        for number in 0..8 {
            cache
                .add_payload_to_cache(large_block(number, quarter_of_bound))
                .await
                .expect("payload body matches its header");
            assert!(
                cache.cache.read().await.unsolicited_payloads.total_bytes
                    <= UNSOLICITED_PAYLOADS_MAX_BYTES
            );
        }
        let unsolicited_count = cache.cache.read().await.unsolicited_payloads.entries.len();
        assert!(unsolicited_count < 4, "byte bound limits the entry count");

        // A single payload above the byte bound is still kept.
        let oversized = large_block(100, UNSOLICITED_PAYLOADS_MAX_BYTES + 1);
        cache
            .add_payload_to_cache(oversized.clone())
            .await
            .expect("payload body matches its header");
        let inner = cache.cache.read().await;
        assert_eq!(inner.unsolicited_payloads.entries.len(), 1);
        assert!(inner.unsolicited_payloads.contains(&oversized.hash()));
        assert_eq!(inner.unsolicited_payloads.total_bytes, oversized.size());
    }

    /// An EVM payload often arrives before the Irys block that names it. It
    /// must be served when that block is validated, and from then on it must
    /// not be evicted by unsolicited payloads.
    #[tokio::test]
    async fn early_payload_is_served_and_promoted_when_its_block_arrives() {
        let cache = test_cache();
        let early = block_with_number(1);
        cache
            .add_payload_to_cache(early.clone())
            .await
            .expect("payload body matches its header");
        assert!(!in_requested_tier(&cache, &early.hash()).await);

        let served = cache
            .wait_for_sealed_block(&early.hash(), false)
            .await
            .expect("early payload is served from the cache");
        assert_eq!(served, early);
        assert!(in_requested_tier(&cache, &early.hash()).await);

        add_unsolicited_payloads(
            &cache,
            1000..1000 + 2 * UNSOLICITED_PAYLOADS_MAX_COUNT as u64,
        )
        .await;
        assert!(cache.is_payload_in_cache(&early.hash()).await);
    }
}
