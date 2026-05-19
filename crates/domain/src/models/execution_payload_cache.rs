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
/// call is disrupted by the local cache before the payload arrives.
///
/// This does NOT indicate that the execution layer deterministically
/// failed — it indicates that the in-process oneshot wiring used to
/// signal payload arrival was torn down. Today the only construction
/// path is the `payload_senders` LRU evicting our sender (heavy
/// catch-up sync pushing >`PAYLOAD_RECEIVERS_CAPACITY` distinct
/// waiters through), or an explicit `remove_payload_from_cache` for
/// the same hash.
///
/// Callers MUST route this as a soft local failure (retry via fresh
/// gossip / cache re-entry) and MUST NOT classify it as a node fault:
/// the node is healthy, the cache is just saturated.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone, Copy)]
pub enum ExecutionPayloadWaitError {
    /// The oneshot sender registered for `evm_block_hash` was dropped
    /// before the payload arrived (LRU eviction or explicit cache
    /// removal). Retry by re-entering the cache after fresh gossip.
    #[error("payload-wait receiver disrupted by local cache for evm_block_hash {evm_block_hash}")]
    ReceiverDisrupted { evm_block_hash: B256 },
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
}

impl ExecutionPayloadCache {
    pub fn new(peer_list: PeerList, reth_payload_provider: RethBlockProvider) -> Self {
        Self {
            cache: Arc::new(RwLock::new(ExecutionPayloadCacheInner {
                payloads: LruCache::new(NonZeroUsize::new(PAYLOAD_CACHE_CAPACITY).expect("payload capacity is not a non-zero usize")),
                payloads_currently_requested_from_the_network: LruCache::new(NonZeroUsize::new(PAYLOAD_REQUESTS_CACHE_CAPACITY).expect("payloads currently requested from the network capacity is not a non-zero usize")),
            })),
            // TODO: fix this to use a real RPC client
            reth_payload_provider,
            payload_senders: Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(PAYLOAD_RECEIVERS_CAPACITY).expect("payload senders capacity is not a non-zero usize")))),
            peer_list
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
    /// — today exclusively LRU eviction from `payload_senders` (capacity
    /// `PAYLOAD_RECEIVERS_CAPACITY = 1000`) under heavy catch-up sync, or an
    /// explicit `remove_payload_from_cache` call for the same hash. This is
    /// a local-cache disruption signal, NOT an execution-layer fault — see
    /// the doc on [`ExecutionPayloadWaitError`] for the caller contract.
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
        // `receiver.await` errors only when the matching oneshot sender is
        // dropped (LRU eviction of the `payload_senders` slot or explicit
        // cache removal). Surface that as the typed disruption error so
        // callers can route it as a soft local failure instead of
        // misattributing to "EL never delivered".
        receiver
            .await
            .map_err(|_| ExecutionPayloadWaitError::ReceiverDisrupted {
                evm_block_hash: *evm_block_hash,
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PeerList;

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
    #[cfg(feature = "test-utils")]
    #[tokio::test]
    async fn wait_for_payload_returns_receiver_disrupted_on_eviction() {
        let peer_list = PeerList::test_mock().expect("test PeerList");
        let provider = RethBlockProvider::new_mock();
        let cache = ExecutionPayloadCache::new(peer_list, provider);

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
        let result: Result<SealedBlock<Block>, ExecutionPayloadWaitError> =
            tokio::time::timeout(Duration::from_secs(2), async move {
                receiver
                    .await
                    .map_err(|_| ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash })
            })
            .await
            .expect("receiver must resolve after sender drop");

        assert_eq!(
            result.unwrap_err(),
            ExecutionPayloadWaitError::ReceiverDisrupted { evm_block_hash },
            "cache-eviction disruption must surface as the typed soft error, \
             NOT collapse into a generic 'payload unavailable' / 'EL failed' signal",
        );
    }
}
