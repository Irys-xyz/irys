//! A basic Ethereum payload builder implementation.
//! Original impl: https://github.com/paradigmxyz/reth/blob/2b283ae83f6c68b4c851206f8cd01491f63bb608/crates/ethereum/payload/src/lib.rs#L53

use alloy_consensus::Transaction as _;
use lru::LruCache;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_payload_builder::{EthBuiltPayload, EthPayloadBuilderAttributes, PayloadId};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    error::InvalidPoolTransactionError,
    identifier::{SenderId, TransactionId},
    BestTransactions, BestTransactionsAttributes, EthPooledTransaction, TransactionOrigin,
    TransactionPool, ValidPoolTransaction,
};
use revm_primitives::FixedBytes;
use std::{collections::HashSet, num::NonZeroUsize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

use reth_ethereum_payload_builder::{default_ethereum_payload, EthereumBuilderConfig};

type BestTransactionsIter =
    Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>>;

/// Request for shadow transactions for a specific payload ID
///
/// This is sent through the notification channel when a payload is requested
/// but not found in the cache.
#[derive(Debug)]
pub struct ShadowTxRequest {
    pub payload_id: PayloadId,
    pub response_tx: oneshot::Sender<(Vec<EthPooledTransaction>, Instant)>,
}

/// Thread-safe store for shadow transactions indexed by payload ID with notification system
#[derive(Debug, Clone)]
pub struct ShadowTxStore {
    inner: Arc<Mutex<LruCache<DeterministicShadowTxKey, (Vec<EthPooledTransaction>, Instant)>>>,
    request_tx: Option<mpsc::UnboundedSender<ShadowTxRequest>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeterministicShadowTxKey(PayloadId);

impl DeterministicShadowTxKey {
    pub fn new(payload_id: PayloadId) -> Self {
        Self(payload_id)
    }
}

impl ShadowTxStore {
    /// Create a new shadow transaction store with LRU cache capacity of 50
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(50).expect("50 is non-zero"),
            ))),
            request_tx: None,
        }
    }

    /// Create a new shadow transaction store with notification capability
    pub fn new_with_notifications() -> (Self, mpsc::UnboundedReceiver<ShadowTxRequest>) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let store = Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(50).expect("50 is non-zero"),
            ))),
            request_tx: Some(request_tx),
        };
        (store, request_rx)
    }

    /// Set shadow transactions for a specific payload ID
    /// This method should be called by external code (like block producers) to provide
    /// shadow transactions that will be included in the payload for the given ID.
    pub fn set_shadow_txs(
        &self,
        key: DeterministicShadowTxKey,
        shadow_txs: Vec<EthPooledTransaction>,
    ) {
        let timestamp = Instant::now();
        let mut store = self.inner.lock().unwrap();
        store.put(key, (shadow_txs, timestamp));
    }

    /// Get shadow transactions for a specific payload ID (blocking version)
    /// This version blocks until shadow transactions are available or timeout occurs
    pub async fn get_shadow_txs_blocking(
        &self,
        payload_id: PayloadId,
        timeout: Duration,
    ) -> (Vec<EthPooledTransaction>, Instant) {
        let key = DeterministicShadowTxKey::new(payload_id);
        // First attempt to get from cache
        {
            let mut store = self.inner.lock().unwrap();
            if let Some((shadow_txs, timestamp)) = store.get(&key) {
                return (shadow_txs.clone(), *timestamp);
            }
        }

        // If not found and notifications are enabled, send notification and wait
        if let Some(request_tx) = &self.request_tx {
            let (response_tx, response_rx) = oneshot::channel();
            let request = ShadowTxRequest {
                payload_id,
                response_tx,
            };

            // Send notification
            request_tx
                .send(request)
                .expect("Notification channel closed");
            // Wait for response with specified timeout
            if let Ok(Ok((shadow_txs, timestamp))) =
                tokio::time::timeout(timeout, response_rx).await
            {
                // add to cache
                self.inner
                    .lock()
                    .unwrap()
                    .put(key, (shadow_txs.clone(), timestamp));

                return (shadow_txs, timestamp);
            }
        }

        // Fallback: return empty if not found
        (Vec::new(), Instant::now())
    }
}

impl Default for ShadowTxStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Ethereum payload builder
#[derive(Debug, Clone)]
pub struct IrysPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    /// Payload builder configuration.
    builder_config: EthereumBuilderConfig,
    /// Shadow txs don't live inside the tx pool, so they need to be handled separately.
    shadow_tx_store: ShadowTxStore,
}

/// Combined iterator that yields shadow transactions first, then pool transactions
pub struct CombinedTransactionIterator {
    /// Shadow transactions to yield first
    shadow_txs: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    shadow_tx_hashes: HashSet<FixedBytes<32>>,
    /// Pool transactions iterator
    pool_iter: BestTransactionsIter,
}

impl CombinedTransactionIterator {
    /// Create a new combined iterator
    pub fn new(
        timestamp: Instant,
        shadow_txs: Vec<EthPooledTransaction>,
        pool_iter: BestTransactionsIter,
    ) -> Self {
        let shadow_txs = shadow_txs
            .into_iter()
            .map(|tx| ValidPoolTransaction {
                transaction_id: TransactionId::new(SenderId::from(0), tx.nonce()),
                transaction: tx,
                propagate: false,
                timestamp,
                origin: TransactionOrigin::Private,
                authority_ids: None,
            })
            .map(Arc::new)
            .collect::<VecDeque<_>>();
        let shadow_tx_hashes = shadow_txs
            .iter()
            .map(|tx| *tx.hash())
            .collect::<HashSet<_>>();

        Self {
            shadow_txs,
            shadow_tx_hashes,
            pool_iter,
        }
    }
}

impl Iterator for CombinedTransactionIterator {
    type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        // First yield all shadow transactions
        if let Some(shadow_tx) = self.shadow_txs.pop_front() {
            return Some(shadow_tx);
        }

        // Then yield pool transactions
        self.pool_iter.next()
    }
}

impl BestTransactions for CombinedTransactionIterator {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if self.shadow_tx_hashes.contains(transaction.hash()) {
            // Shadow txs are already removed from the queue, so we don't need to do anything
            // NOTE FOR READER: if you refactor the code here, ensure that we *never*
            // try to mark a shadow tx as invalid by calling the underlying pool_iter.
            // This for some reason `clear` the whole pool_iter.
            return;
        }

        // For pool transactions, delegate to the pool iterator
        self.pool_iter.mark_invalid(transaction, kind);
    }

    fn no_updates(&mut self) {
        self.pool_iter.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.pool_iter.set_skip_blobs(skip_blobs);
    }
}

impl<Pool, Client, EvmConfig> IrysPayloadBuilder<Pool, Client, EvmConfig> {
    /// `IrysPayloadBuilder` constructor.
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
        shadow_tx_store: ShadowTxStore,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            shadow_tx_store,
        }
    }

    pub fn shadow_tx_store(&self) -> &ShadowTxStore {
        &self.shadow_tx_store
    }

    pub fn shadow_tx_store_cloned(&self) -> ShadowTxStore {
        self.shadow_tx_store.clone()
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Pool, Client, EvmConfig> IrysPayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
{
    pub fn best_transactions_with_attributes(
        &self,
        attributes: BestTransactionsAttributes,
        payload_id: PayloadId,
    ) -> BestTransactionsIter {
        // Get shadow transactions from the store
        let (shadow_txs, timestamp) = futures::executor::block_on(
            self.shadow_tx_store
                .get_shadow_txs_blocking(payload_id, Duration::from_secs(1)),
        );

        // Get pool transactions iterator
        let pool_txs = self.pool.best_transactions_with_attributes(attributes);

        // Create combined iterator
        Box::new(CombinedTransactionIterator::new(
            timestamp, shadow_txs, pool_txs,
        ))
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Pool, Client, EvmConfig> PayloadBuilder for IrysPayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
{
    type Attributes = EthPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        let payload_id = args.config.attributes.payload_id();
        let result = default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.best_transactions_with_attributes(attributes, payload_id),
        )?;
        Ok(result)
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        if self.builder_config.await_payload_on_missing {
            MissingPayloadBehaviour::AwaitInProgress
        } else {
            MissingPayloadBehaviour::RaceEmptyPayload
        }
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        let payload_id = config.attributes.payload_id();
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.best_transactions_with_attributes(attributes, payload_id),
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_shadow_tx_store_blocking() {
        // Create store with notifications
        let (store, mut request_rx) = ShadowTxStore::new_with_notifications();
        let store_clone = store.clone();

        // Spawn a handler that responds to requests
        let handler = tokio::spawn(async move {
            if let Some(request) = request_rx.recv().await {
                // Simulate generating shadow transactions
                let shadow_txs = vec![]; // Empty for test
                let timestamp = Instant::now();
                let _ = request.response_tx.send((shadow_txs, timestamp));
            }
        });

        // Test blocking version
        let payload_id = PayloadId::new([5; 8]);

        let (txs, _) = store_clone
            .get_shadow_txs_blocking(payload_id, Duration::from_millis(500))
            .await;
        assert!(txs.is_empty()); // Should get empty response from handler

        // Wait for handler to complete
        let _ = timeout(Duration::from_secs(1), handler).await;
    }
}
