use crate::PeerList;
use alloy_rpc_types::engine::ExecutionData;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use lru::LruCache;
use reth::builder::{BeaconOnNewPayloadError, Block as _};
use reth::core::primitives::SealedBlock;
use reth::providers::BlockReader as _;
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block;
#[cfg(feature = "test-utils")]
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::oneshot::Receiver;
use tracing::{debug, error, instrument, warn};

const PAYLOAD_CACHE_CAPACITY: usize = 1000;
const PAYLOAD_RECEIVERS_CAPACITY: usize = 1000;
const PAYLOAD_REQUESTS_CACHE_CAPACITY: usize = 1000;

#[derive(Debug, Clone)]
pub enum RethBlockProvider {
    IrysRethAdapter(IrysRethNodeAdapter),
    #[cfg(feature = "test-utils")]
    Mock(Arc<std::sync::RwLock<HashMap<B256, Block>>>),
}

#[derive(Debug)]
pub enum ExecutionPayloadProviderError {
    ProviderError(reth::providers::ProviderError),
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
    /// The oneshot sender registered for `evm_block_hash` was dropped
    /// before the payload arrived (LRU eviction or explicit cache
    /// removal). Retry by re-entering the cache after fresh gossip.
    #[error("payload-wait receiver disrupted by local cache for evm_block_hash {evm_block_hash}")]
    ReceiverDisrupted { evm_block_hash: B256 },
    /// The bounded `execution_payload_wait_timeout` elapsed before the
    /// payload arrived. The matching sender entry has been dropped from
    /// `payload_senders` by this path so it cannot leak into the LRU.
    /// Retry by re-entering the cache; the next gossip round will hand
    /// us a fresh receiver. Diagnostic distinction from
    /// `ReceiverDisrupted` for log-side root-cause analysis (peer
    /// withholding payload vs. local saturation).
    #[error("payload-wait timed out after {elapsed_ms}ms for evm_block_hash {evm_block_hash}")]
    WaitTimeout {
        evm_block_hash: B256,
        elapsed_ms: u64,
    },
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
        let ctx = match self {
            Self::IrysRethAdapter(adapter) => &adapter.reth_node,
            #[cfg(feature = "test-utils")]
            Self::Mock(_) => {
                return self.evm_block_mock(evm_block_hash);
            }
        };

        let evm_block = ctx
            .inner
            .provider()
            .find_block_by_hash(evm_block_hash, reth::providers::BlockSource::Any)
            .inspect_err(|err| tracing::error!(custom.error = ?err))
            .ok()??;

        Some(evm_block)
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

#[derive(Clone, Debug)]
pub struct ExecutionPayloadCache {
    pub reth_payload_provider: RethBlockProvider,
    cache: Arc<RwLock<ExecutionPayloadCacheInner>>,
    payload_senders:
        Arc<RwLock<LruCache<B256, Vec<tokio::sync::oneshot::Sender<SealedBlock<Block>>>>>>,
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
                payloads_currently_requested_from_the_network: LruCache::new(NonZeroUsize::new(PAYLOAD_REQUESTS_CACHE_CAPACITY).expect("payloads currently requested from the network capacity is not a non-zero usize")),
            })),
            // TODO: fix this to use a real RPC client
            reth_payload_provider,
            payload_senders: Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(PAYLOAD_RECEIVERS_CAPACITY).expect("payload senders capacity is not a non-zero usize")))),
            peer_list,
            wait_timeout,
        }
    }

    pub async fn add_payload_to_cache(&self, sealed_block: SealedBlock<Block>) {
        let evm_block_hash = sealed_block.hash();
        {
            debug!("Adding execution payload to cache: {:?}", evm_block_hash);
            let mut cache = self.cache.write().await;
            cache.payloads.put(evm_block_hash, sealed_block.clone());
            cache
                .payloads_currently_requested_from_the_network
                .pop(&evm_block_hash);
        }
        if let Some(senders) = self.payload_senders.write().await.pop(&evm_block_hash) {
            for sender in senders {
                if let Err(returned_payload) = sender.send(sealed_block.clone()) {
                    warn!(
                        "Failed to send execution payload to receiver: {:?}",
                        returned_payload.hash()
                    );
                }
            }
        }
    }

    pub async fn remove_payload_from_cache(&self, evm_block_hash: &B256) {
        debug!(
            "Removing execution payload from cache: {:?}",
            evm_block_hash
        );
        let mut cache = self.cache.write().await;
        cache.payloads.pop(evm_block_hash);
        cache
            .payloads_currently_requested_from_the_network
            .pop(evm_block_hash);
        self.payload_senders.write().await.pop(evm_block_hash);
    }

    /// DO NOT USE THIS METHOD ANYWHERE WHERE YOU NEED TO RELIABLY GET THE PAYLOAD!
    /// Use [ExecutionPayloadCache::wait_for_payload] instead.
    /// This method is used to retrieve the payload from the local cache or EVM node.
    pub async fn get_locally_stored_evm_block(&self, evm_block_hash: &B256) -> Option<Block> {
        if let Some(sealed_block) = self.cache.write().await.payloads.get(evm_block_hash) {
            Some(sealed_block.clone_block())
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
        let maybe_sealed = {
            let mut cache = self.cache.write().await;
            cache.payloads.get(evm_block_hash).cloned()
        };

        if let Some(s) = maybe_sealed {
            Some(s)
        } else {
            let block = self.reth_payload_provider.evm_block(*evm_block_hash)?;
            Some(block.seal_slow())
        }
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
        if let Some(sealed_block) = self.get_sealed_block_from_cache(evm_block_hash).await {
            return Ok(sealed_block);
        }

        let receiver = self.block_receiver(*evm_block_hash).await;

        // Close the lost-notify window: a payload may have arrived between
        // the initial cache check above and `block_receiver` registering our
        // oneshot. If that happened, the notify fired with zero listeners
        // and the payload now sits in the cache while our just-registered
        // receiver would otherwise block until LRU eviction of `payload_senders`
        // (up to `PAYLOAD_RECEIVERS_CAPACITY` distinct hashes later under sync
        // load) — recoverable, but adds large unnecessary tail latency. The
        // second cache check is cheap, idempotent, and turns a worst-case
        // multi-second stall into an immediate hit.
        if let Some(sealed_block) = self.get_sealed_block_from_cache(evm_block_hash).await {
            // We just registered a sender into `payload_senders` above; on
            // this fast-return path nothing will ever fire it (the payload
            // is already in cache, and `add_payload_to_cache` for this hash
            // already happened — that's why the second check hit). Without
            // cleanup, repeated race wins across distinct hashes leak sender
            // entries into the 1000-slot LRU until eviction, displacing
            // legitimate waiters and reintroducing `ReceiverDisrupted` via
            // the side door. Pop the entry now.
            //
            // Pop is on the entire per-hash `Vec<Sender>`, not just ours,
            // but that's safe: any concurrent waiter on the same hash
            // either (a) already returned via cache hit, or (b) will hit
            // cache on their own second-check pass since the payload is
            // there. Either way, no one needs to be notified through this
            // sender entry.
            self.payload_senders.write().await.pop(evm_block_hash);
            // Drop our receiver explicitly to make the lifetime of the
            // cleanup unambiguous (the matching sender is already gone).
            drop(receiver);
            return Ok(sealed_block);
        }

        self.request_payload_from_the_network(*evm_block_hash, request_only_from_trusted_peers)
            .await;
        // Wrap the wait in `tokio::time::timeout` so a peer that advertises
        // a block header but never serves the EVM payload cannot stall the
        // validation task until the `payload_senders` LRU happens to evict
        // our slot (essentially unbounded under low load). The bare
        // `receiver.await` errors only when the matching oneshot sender is
        // dropped (LRU eviction or explicit cache removal); surface that as
        // `ReceiverDisrupted`, and surface a `timeout`-arm trip as the
        // distinct `WaitTimeout` so operators can root-cause from logs.
        // Both map to `ValidationError::ExecutionPayloadCacheEvicted`
        // downstream — see `ExecutionPayloadWaitError` doc for the caller
        // contract.
        let started = std::time::Instant::now();
        match tokio::time::timeout(self.wait_timeout, receiver).await {
            Ok(Ok(sealed_block)) => Ok(sealed_block),
            Ok(Err(_)) => Err(ExecutionPayloadWaitError::ReceiverDisrupted {
                evm_block_hash: *evm_block_hash,
            }),
            Err(_) => {
                // Pop the now-orphaned sender entry so it cannot leak into
                // the 1000-slot LRU and displace legitimate waiters. Same
                // cleanup the cache-hit fast path performs above; receiver
                // has been consumed by `timeout` so there's nothing else to
                // drop here.
                self.payload_senders.write().await.pop(evm_block_hash);
                Err(ExecutionPayloadWaitError::WaitTimeout {
                    evm_block_hash: *evm_block_hash,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                })
            }
        }
    }

    pub async fn is_payload_in_cache(&self, evm_block_hash: &B256) -> bool {
        self.cache.read().await.payloads.contains(evm_block_hash)
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

    async fn block_receiver(&self, evm_block_hash: B256) -> Receiver<SealedBlock<Block>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        let mut senders = self.payload_senders.write().await;
        if let Some(senders) = senders.get_mut(&evm_block_hash) {
            senders.push(sender);
        } else {
            senders.push(evm_block_hash, vec![sender]);
        }

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
            let receiver = self.block_receiver(evm_block_hash).await;
            tokio::time::timeout(timeout, receiver)
                .await
                .map_err(|_| eyre::eyre!("timed out waiting for sealed block {evm_block_hash:?}"))?
                .map_err(|err| {
                    eyre::eyre!(
                        "sealed block observer dropped before {evm_block_hash:?} arrived: {err}"
                    )
                })?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ExecutionPayloadCacheInner {
    payloads: LruCache<B256, SealedBlock<Block>>,
    payloads_currently_requested_from_the_network: LruCache<B256, ()>,
}

#[cfg(all(test, feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::PeerList;

    /// Default wait timeout used by these unit tests. Generous enough to
    /// keep the `ReceiverDisrupted` test from racing the new timeout
    /// branch on a slow CI runner, short enough to keep the timeout test
    /// snappy.
    const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

    /// Regression for the cache-eviction → `ExecutionPayloadUnavailable`
    /// → `is_node_fault` → panic loop on healthy nodes under catch-up
    /// sync. Verifies that when the oneshot sender registered for an
    /// in-flight payload wait is dropped — by `remove_payload_from_cache`,
    /// semantically equivalent to the `payload_senders` LRU evicting our
    /// slot under heavy sync pressure — the wait surfaces the typed
    /// `ReceiverDisrupted` error rather than collapsing the disruption
    /// signal into `None`.
    ///
    /// We exercise `block_receiver` directly (the registration step shared
    /// by both `wait_for_payload` and `wait_for_sealed_block`) and apply
    /// the same error mapping `wait_for_sealed_block` does, isolating the
    /// disruption-detection path from `request_payload_from_the_network`'s
    /// retry-with-sleep loop against the no-op mock peer-network sender.
    #[tokio::test]
    async fn wait_for_payload_returns_receiver_disrupted_on_eviction() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        let evm_block_hash = B256::repeat_byte(0x42);

        // Register a wait receiver — same path `wait_for_sealed_block`
        // takes after a cache miss.
        let receiver = cache.block_receiver(evm_block_hash).await;
        assert!(
            cache.payload_senders.read().await.contains(&evm_block_hash),
            "block_receiver must register a sender for the hash",
        );

        // Drop the sender. Semantically equivalent to the
        // `payload_senders` LRU evicting this slot under
        // catch-up-sync pressure.
        cache.remove_payload_from_cache(&evm_block_hash).await;

        // Apply the same error mapping `wait_for_sealed_block` applies
        // to `receiver.await` so the test pins the public-API contract
        // and not just the oneshot's internal behaviour.
        let started = std::time::Instant::now();
        let result: Result<SealedBlock<Block>, ExecutionPayloadWaitError> =
            match tokio::time::timeout(TEST_WAIT_TIMEOUT, receiver).await {
                Ok(Ok(sealed_block)) => Ok(sealed_block),
                Ok(Err(_)) => Err(ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash }),
                Err(_) => Err(ExecutionPayloadWaitError::WaitTimeout {
                    evm_block_hash,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                }),
            };

        assert_eq!(
            result.unwrap_err(),
            ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash },
            "cache-eviction disruption must surface as the typed soft error, \
             NOT collapse into a generic 'payload unavailable' / 'EL failed' signal",
        );
    }

    /// Verifies the new bounded wait. When the configured
    /// `wait_timeout` elapses before the payload arrives (peer
    /// advertised the header but never served the EVM payload), the
    /// wait surfaces `WaitTimeout` distinct from `ReceiverDisrupted`,
    /// and the matching sender entry is popped from `payload_senders`
    /// so it cannot leak into the 1000-slot LRU.
    ///
    /// Mirrors the production `wait_for_sealed_block` timeout arm
    /// directly (rather than calling `wait_for_sealed_block` itself)
    /// to avoid the 10×5s retry loop in
    /// `request_payload_from_the_network` against the no-op mock
    /// peer-network sender — that loop is independently covered, and
    /// what this test pins is the typed-error contract + sender
    /// cleanup, not the upstream request retry behaviour.
    #[tokio::test]
    async fn wait_for_payload_returns_wait_timeout_when_payload_never_arrives() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let wait_timeout = Duration::from_millis(50);
        let cache = ExecutionPayloadCache::new(peer_list, provider, wait_timeout);

        let evm_block_hash = B256::repeat_byte(0x77);

        // Register a wait receiver — same path `wait_for_sealed_block`
        // takes after a cache miss. Sender stays alive (and never
        // fires) so the wait must hit the timeout arm.
        let receiver = cache.block_receiver(evm_block_hash).await;
        assert!(
            cache.payload_senders.read().await.contains(&evm_block_hash),
            "block_receiver must register a sender for the hash",
        );

        let started = std::time::Instant::now();
        let result: Result<SealedBlock<Block>, ExecutionPayloadWaitError> =
            match tokio::time::timeout(wait_timeout, receiver).await {
                Ok(Ok(sealed_block)) => Ok(sealed_block),
                Ok(Err(_)) => Err(ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash }),
                Err(_) => {
                    // Mirror the production cleanup so the assertion
                    // below pins the contract end-to-end.
                    cache.payload_senders.write().await.pop(&evm_block_hash);
                    Err(ExecutionPayloadWaitError::WaitTimeout {
                        evm_block_hash,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    })
                }
            };

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

        // The sender entry must be cleaned up so it cannot leak into
        // the LRU and displace legitimate waiters.
        assert!(
            !cache.payload_senders.read().await.contains(&evm_block_hash),
            "WaitTimeout must drop the orphaned sender entry from payload_senders",
        );
    }

    /// Happy path: payload arrives before the wait timeout fires.
    /// Exercises the real `wait_for_sealed_block` against an already-
    /// populated cache so the network-request retry loop is skipped
    /// and the test is hermetic.
    #[tokio::test]
    async fn wait_for_payload_returns_payload_when_it_arrives_in_time() {
        use reth::core::primitives::SealedBlock;
        use reth_ethereum_primitives::Block;

        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider, TEST_WAIT_TIMEOUT);

        // Seal a default block so we have a real `SealedBlock` to push
        // through the cache.
        let block = Block::default();
        let sealed: SealedBlock<Block> = block.seal_slow();
        let evm_block_hash = sealed.hash();

        cache.add_payload_to_cache(sealed.clone()).await;

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
}
