//! A basic Ethereum payload builder implementation.

use alloy_consensus::Transaction;
use alloy_primitives::Keccak256;
use lru::LruCache;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_payload_builder::{EthBuiltPayload, EthPayloadBuilderAttributes};
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

/// Request for system transactions for a specific payload ID
///
/// This is sent through the notification channel when a payload is requested
/// but not found in the cache.
#[derive(Debug)]
pub struct SystemTxRequest {
    pub parent_beacon_block_root: FixedBytes<32>,
    pub timestamp: u64,
    pub parent_evm_block_hash: FixedBytes<32>,
    pub response_tx: oneshot::Sender<(Vec<EthPooledTransaction>, Instant)>,
}

/// Thread-safe store for system transactions indexed by payload ID with notification system
#[derive(Debug, Clone)]
pub struct SystemTxStore {
    inner: Arc<Mutex<LruCache<DeterministicSystemTxKey, (Vec<EthPooledTransaction>, Instant)>>>,
    request_tx: Option<mpsc::UnboundedSender<SystemTxRequest>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeterministicSystemTxKey(FixedBytes<32>);

impl DeterministicSystemTxKey {
    pub fn new(
        parent_beacon_block_root: FixedBytes<32>,
        timestamp: u64,
        parent_evm_block_hash: FixedBytes<32>,
    ) -> Self {
        let mut hasher = Keccak256::new();
        hasher.update(parent_beacon_block_root.0);
        hasher.update(timestamp.to_be_bytes());
        hasher.update(parent_evm_block_hash.0);
        let hash = hasher.finalize();
        Self(hash)
    }
}

impl SystemTxStore {
    /// Create a new system transaction store with LRU cache capacity of 50
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(50).expect("50 is non-zero"),
            ))),
            request_tx: None,
        }
    }

    /// Create a new system transaction store with notification capability
    pub fn new_with_notifications() -> (Self, mpsc::UnboundedReceiver<SystemTxRequest>) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let store = Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(50).expect("50 is non-zero"),
            ))),
            request_tx: Some(request_tx),
        };
        (store, request_rx)
    }

    /// Set system transactions for a specific payload ID
    /// This method should be called by external code (like block producers) to provide
    /// system transactions that will be included in the payload for the given ID.
    pub fn set_system_txs(
        &self,
        key: DeterministicSystemTxKey,
        system_txs: Vec<EthPooledTransaction>,
    ) {
        let timestamp = Instant::now();
        let mut store = self.inner.lock().unwrap();
        store.put(key, (system_txs, timestamp));
    }

    /// Get system transactions for a specific payload ID (blocking version)
    /// This version blocks until system transactions are available or timeout occurs
    pub async fn get_system_txs_blocking(
        &self,
        parent_beacon_block_root: FixedBytes<32>,
        timestamp: u64,
        parent_block_hash: FixedBytes<32>,
        timeout: Duration,
    ) -> (Vec<EthPooledTransaction>, Instant) {
        let key =
            DeterministicSystemTxKey::new(parent_beacon_block_root, timestamp, parent_block_hash);
        // First attempt to get from cache
        {
            let mut store = self.inner.lock().unwrap();
            if let Some((system_txs, timestamp)) = store.get(&key) {
                return (system_txs.clone(), *timestamp);
            }
        }

        // If not found and notifications are enabled, send notification and wait
        if let Some(request_tx) = &self.request_tx {
            let (response_tx, response_rx) = oneshot::channel();
            let request = SystemTxRequest {
                parent_beacon_block_root,
                timestamp,
                parent_evm_block_hash: parent_block_hash,
                response_tx,
            };

            // Send notification
            request_tx
                .send(request)
                .expect("Notification channel closed");
            // Wait for response with specified timeout
            if let Ok(result) = tokio::time::timeout(timeout, response_rx).await {
                if let Ok((system_txs, timestamp)) = result {
                    // add to cache
                    self.inner
                        .lock()
                        .unwrap()
                        .put(key, (system_txs.clone(), timestamp));

                    return (system_txs, timestamp);
                }
            }
        }

        // Fallback: return empty if not found
        (Vec::new(), Instant::now())
    }
}

impl Default for SystemTxStore {
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
    /// System txs don't live inside the tx pool, so they need to be handled separately.
    system_tx_store: SystemTxStore,
}

/// Combined iterator that yields system transactions first, then pool transactions
pub struct CombinedTransactionIterator {
    /// System transactions to yield first
    system_txs: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    system_tx_hashes: HashSet<FixedBytes<32>>,
    /// Pool transactions iterator
    pool_iter: BestTransactionsIter,
}

impl CombinedTransactionIterator {
    /// Create a new combined iterator
    pub fn new(
        timestamp: Instant,
        system_txs: Vec<EthPooledTransaction>,
        pool_iter: BestTransactionsIter,
    ) -> Self {
        let system_txs = system_txs
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
        let system_tx_hashes = system_txs
            .iter()
            .map(|tx| *tx.hash())
            .collect::<HashSet<_>>();

        Self {
            system_txs,
            system_tx_hashes,
            pool_iter,
        }
    }
}

impl Iterator for CombinedTransactionIterator {
    type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        // First yield all system transactions
        if let Some(system_tx) = self.system_txs.pop_front() {
            return Some(system_tx);
        }

        // Then yield pool transactions
        self.pool_iter.next()
    }
}

impl BestTransactions for CombinedTransactionIterator {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if self.system_tx_hashes.contains(transaction.hash()) {
            // System txs are already removed from the queue, so we don't need to do anything
            // NOTE FOR READER: if you refactor the code here, ensure that we *never*
            // try to mark a system tx as invalid by calling the underlying pool_iter.
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
        system_tx_store: SystemTxStore,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            system_tx_store,
        }
    }

    pub fn system_tx_store(&self) -> &SystemTxStore {
        &self.system_tx_store
    }

    pub fn system_tx_store_cloned(&self) -> SystemTxStore {
        self.system_tx_store.clone()
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
        parent_beacon_block_root: FixedBytes<32>,
        timestamp: u64,
        parent_evm_block_hash: FixedBytes<32>,
    ) -> BestTransactionsIter {
        // Get system transactions from the store
        let (system_txs, timestamp) =
            futures::executor::block_on(self.system_tx_store.get_system_txs_blocking(
                parent_beacon_block_root,
                timestamp,
                parent_evm_block_hash,
                Duration::from_secs(1),
            ));

        // Get pool transactions iterator
        let pool_txs = self.pool.best_transactions_with_attributes(attributes);

        // Create combined iterator
        Box::new(CombinedTransactionIterator::new(
            timestamp, system_txs, pool_txs,
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
        tracing::info!("try_build");
        let parent_beacon_block_root = args
            .config
            .attributes
            .parent_beacon_block_root
            .unwrap_or_default();
        let timestamp = args.config.attributes.timestamp;
        let parent_evm_block_hash = args.config.attributes.parent;
        let result = default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| {
                self.best_transactions_with_attributes(
                    attributes,
                    parent_beacon_block_root,
                    timestamp,
                    parent_evm_block_hash,
                )
            },
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
        tracing::info!("build_empty_payload");
        let parent_beacon_block_root = config
            .attributes
            .parent_beacon_block_root
            .unwrap_or_default();
        let timestamp = config.attributes.timestamp;
        let parent_evm_block_hash = config.attributes.parent;
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| {
                self.best_transactions_with_attributes(
                    attributes,
                    parent_beacon_block_root,
                    timestamp,
                    parent_evm_block_hash,
                )
            },
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
    async fn test_system_tx_store_blocking() {
        // Create store with notifications
        let (store, mut request_rx) = SystemTxStore::new_with_notifications();
        let store_clone = store.clone();

        // Spawn a handler that responds to requests
        let handler = tokio::spawn(async move {
            if let Some(request) = request_rx.recv().await {
                // Simulate generating system transactions
                let system_txs = vec![]; // Empty for test
                let timestamp = Instant::now();
                let _ = request.response_tx.send((system_txs, timestamp));
            }
        });

        // Test blocking version
        let parent_beacon_block_root = FixedBytes::from([5; 32]);
        let timestamp = 1234567890;
        let parent_evm_block_hash = FixedBytes::from([13; 32]);

        let (txs, _) = store_clone
            .get_system_txs_blocking(
                parent_beacon_block_root,
                timestamp,
                parent_evm_block_hash,
                Duration::from_millis(500),
            )
            .await;
        assert!(txs.is_empty()); // Should get empty response from handler

        // Wait for handler to complete
        let _ = timeout(Duration::from_secs(1), handler).await;
    }
}
