//! A basic Ethereum payload builder implementation adapted for Irys.
//! Original impl: [here](https://github.com/paradigmxyz/reth/blob/2b283ae83f6c68b4c851206f8cd01491f63bb608/crates/ethereum/payload/src/lib.rs#L53)
//!
//! ## High-level flow
//! During block production the payload builder pulls transactions from multiple sources:
//! 1. **Shadow transactions**: bundled by the block producer outside of the mempool. These must
//!    be emitted first to guarantee deterministic inclusion.
//! 2. **Pool transactions**: standard mempool ordering (priority-driven) fetched via the
//!    `BestTransactions` iterator.
//!
//! Both sources feed into [`CombinedTransactionIterator`], which decorates the pool iterator with
//! the additional logic required for shadow transactions and Programmable Data (PD) constraints.
//!
//! ## PD transactions and chunk budgeting
//! PD-aware transactions embed a PD header and list required data chunks in their access list. The
//! builder must avoid exceeding `MAX_PD_CHUNKS_PER_BLOCK` (currently 7,500) when assembling a block.
//! Each candidate transaction is scanned for PD metadata; if it carries PD state we add its chunk
//! demand to the running total and include it only if the budget still permits.
//!
//! When the budget would be exceeded we *defer* the PD transaction by placing it on a queue owned by
//! the PD budget manager. This allows the iterator to keep yielding subsequent pool transactions
//! (including non-PD ones) and avoids losing track of the over-budget PD candidate.
//!
//! ## Why deferring matters
//! The EVM can later reject a transaction that was already emitted by the iterator (for example,
//! due to state inconsistencies), which triggers `mark_invalid`. In that case we restore the PD
//! chunk budget for that transaction, then re-check the deferred queue: if a previously-skipped PD
//! transaction now fits within the freed capacity it becomes the next candidate. This ensures the
//! builder respects mempool ordering while still honoring the PD limit.

use crate::pd_tx::{detect_and_decode_pd_header, sum_pd_chunks_in_access_list};
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
use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Instant,
};

use reth_ethereum_payload_builder::{default_ethereum_payload, EthereumBuilderConfig};

type BestTransactionsIter =
    Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>>;

/// Thread-safe store for shadow transactions indexed by payload ID.
#[derive(Debug, Clone)]
pub struct ShadowTxStore {
    inner: Arc<Mutex<LruCache<DeterministicShadowTxKey, (Vec<EthPooledTransaction>, Instant)>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeterministicShadowTxKey(PayloadId);

impl DeterministicShadowTxKey {
    pub fn new(payload_id: PayloadId) -> Self {
        Self(payload_id)
    }
}

impl ShadowTxStore {
    /// Create a new shadow transaction store with LRU cache capacity of 50.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(50).expect("50 is non-zero"),
            ))),
        }
    }

    /// Set shadow transactions for a specific payload ID.
    pub fn set_shadow_txs(
        &self,
        key: DeterministicShadowTxKey,
        shadow_txs: Vec<EthPooledTransaction>,
    ) {
        let timestamp = Instant::now();
        let mut store = self.inner.lock().unwrap();
        store.put(key, (shadow_txs, timestamp));
    }

    /// Returns the shadow transactions for `payload_id`, or an empty list if none are registered.
    pub fn shadow_txs(&self, payload_id: PayloadId) -> (Vec<EthPooledTransaction>, Instant) {
        let key = DeterministicShadowTxKey::new(payload_id);
        let mut store = self.inner.lock().unwrap();
        if let Some((shadow_txs, timestamp)) = store.get(&key) {
            return (shadow_txs.clone(), *timestamp);
        }
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
    /// Maximum number of PD chunks that can be included in a single block.
    max_pd_chunks_per_block: u64,
}

/// Combined iterator that yields shadow transactions first, then pool transactions
pub struct CombinedTransactionIterator {
    shadow: ShadowTxQueue,
    pd_budget: PdChunkBudget,
    pool_iter: BestTransactionsIter,
}

struct ShadowTxQueue {
    pending: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    hashes: HashSet<FixedBytes<32>>,
}

impl ShadowTxQueue {
    fn from_shadow_transactions(timestamp: Instant, shadow_txs: Vec<EthPooledTransaction>) -> Self {
        let pending = shadow_txs
            .into_iter()
            .map(|tx| ValidPoolTransaction {
                // todo - recheck the SenderId::from(0) - what are the consequences of using `0`
                transaction_id: TransactionId::new(SenderId::from(0), tx.nonce()),
                transaction: tx,
                propagate: false,
                timestamp,
                origin: TransactionOrigin::Private,
                authority_ids: None,
            })
            .map(Arc::new)
            .collect::<VecDeque<_>>();
        let hashes = pending.iter().map(|tx| *tx.hash()).collect::<HashSet<_>>();

        Self { pending, hashes }
    }

    fn pop_front(&mut self) -> Option<Arc<ValidPoolTransaction<EthPooledTransaction>>> {
        self.pending.pop_front()
    }

    fn contains_hash(&self, hash: &FixedBytes<32>) -> bool {
        self.hashes.contains(hash)
    }
}

struct PdChunkBudget {
    max: u64,
    used: u64,
    deferred: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    accounted: HashMap<FixedBytes<32>, u64>,
}

impl PdChunkBudget {
    fn new(max_pd_chunks_per_block: u64) -> Self {
        Self {
            max: max_pd_chunks_per_block,
            used: 0,
            deferred: VecDeque::new(),
            accounted: HashMap::new(),
        }
    }

    fn can_fit(&self, chunks: u64) -> bool {
        self.used.saturating_add(chunks) <= self.max
    }

    fn try_consume(
        &mut self,
        tx: &Arc<ValidPoolTransaction<EthPooledTransaction>>,
        chunks: u64,
    ) -> bool {
        // TODO: We must check users pd max base fee and the expected base fee
        // for current block, reject the tx if user did not want his tx to be included
        if self.can_fit(chunks) {
            self.used = self.used.saturating_add(chunks);
            self.accounted.insert(*tx.hash(), chunks);
            return true;
        }
        false
    }

    fn defer(&mut self, tx: Arc<ValidPoolTransaction<EthPooledTransaction>>) {
        self.deferred.push_back(tx);
    }

    fn pop_ready_deferred(
        &mut self,
        mut chunk_counter: impl FnMut(&ValidPoolTransaction<EthPooledTransaction>) -> u64,
    ) -> Option<Arc<ValidPoolTransaction<EthPooledTransaction>>> {
        if let Some(candidate) = self.deferred.front() {
            let chunks = chunk_counter(candidate.as_ref());
            if self.can_fit(chunks) {
                return self.deferred.pop_front();
            }
        }
        None
    }

    fn on_invalid(&mut self, tx: &Arc<ValidPoolTransaction<EthPooledTransaction>>) {
        let hash = *tx.hash();
        if let Some(chunks) = self.accounted.remove(&hash) {
            self.used = self.used.saturating_sub(chunks);
            return;
        }

        if let Some(pos) = self
            .deferred
            .iter()
            .position(|candidate| candidate.hash() == tx.hash())
        {
            self.deferred.remove(pos);
        }
    }
}

impl CombinedTransactionIterator {
    fn pd_chunks_for_transaction(transaction: &ValidPoolTransaction<EthPooledTransaction>) -> u64 {
        match detect_and_decode_pd_header(transaction.transaction.input()) {
            Ok(Some(_)) => transaction
                .transaction
                .access_list()
                .map(sum_pd_chunks_in_access_list)
                .unwrap_or_default(),
            _ => 0,
        }
    }

    pub fn new(
        timestamp: Instant,
        shadow_txs: Vec<EthPooledTransaction>,
        pool_iter: BestTransactionsIter,
        max_pd_chunks_per_block: u64,
    ) -> Self {
        Self {
            shadow: ShadowTxQueue::from_shadow_transactions(timestamp, shadow_txs),
            pd_budget: PdChunkBudget::new(max_pd_chunks_per_block),
            pool_iter,
        }
    }

    fn next_candidate(&mut self) -> Option<Arc<ValidPoolTransaction<EthPooledTransaction>>> {
        if let Some(tx) = self.shadow.pop_front() {
            return Some(tx);
        }

        if let Some(tx) = self
            .pd_budget
            .pop_ready_deferred(Self::pd_chunks_for_transaction)
        {
            return Some(tx);
        }

        self.pool_iter.next()
    }
}

impl Iterator for CombinedTransactionIterator {
    type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(tx) = self.next_candidate() {
            let chunks = Self::pd_chunks_for_transaction(tx.as_ref());
            if chunks == 0 || self.pd_budget.try_consume(&tx, chunks) {
                return Some(tx);
            }

            self.pd_budget.defer(tx);
        }

        None
    }
}

impl BestTransactions for CombinedTransactionIterator {
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: InvalidPoolTransactionError) {
        if self.shadow.contains_hash(transaction.hash()) {
            // Shadow txs are already removed from the queue, so we don't need to do anything
            // NOTE FOR READER: if you refactor the code here, ensure that we *never*
            // try to mark a shadow tx as invalid by calling the underlying pool_iter.
            // This for some reason `clear` the whole pool_iter.
            return;
        }

        self.pd_budget.on_invalid(transaction);

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
        max_pd_chunks_per_block: u64,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            shadow_tx_store,
            max_pd_chunks_per_block,
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
        let (shadow_txs, timestamp) = self.shadow_tx_store.shadow_txs(payload_id);

        // Get pool transactions iterator
        let pool_txs = self.pool.best_transactions_with_attributes(attributes);

        // Create combined iterator
        Box::new(CombinedTransactionIterator::new(
            timestamp,
            shadow_txs,
            pool_txs,
            self.max_pd_chunks_per_block,
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
    use crate::pd_tx::{build_pd_access_list, prepend_pd_header_v1_to_calldata, PdHeaderV1, PdKey};
    use alloy_primitives::U256;
    use rand09::{rngs::StdRng, SeedableRng as _};
    use reth_primitives_traits::SignedTransaction;
    use reth_transaction_pool::{test_utils::TransactionGenerator, PoolTransaction as _};
    use std::collections::VecDeque;

    #[test]
    fn shadow_tx_store_returns_inserted_transactions() {
        let store = ShadowTxStore::new();
        let payload_id = PayloadId::new([5; 8]);

        let mut generator = TransactionGenerator::with_num_signers(StdRng::seed_from_u64(33), 1);
        generator.set_base_fee(1);
        generator.set_gas_limit(210_000);
        let tx = normal_pooled_transaction(&mut generator, 0);

        store.set_shadow_txs(DeterministicShadowTxKey::new(payload_id), vec![tx.clone()]);

        let (retrieved, _) = store.shadow_txs(payload_id);
        assert_eq!(retrieved, vec![tx]);

        let (missing, _) = store.shadow_txs(PayloadId::new([6; 8]));
        assert!(missing.is_empty());
    }

    fn pd_pooled_transaction(
        generator: &mut TransactionGenerator<StdRng>,
        nonce: u64,
        chunk_count: u16,
    ) -> EthPooledTransaction {
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: U256::from(1),
            max_base_fee_per_chunk: U256::from(1),
        };
        let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);
        let access_list = build_pd_access_list([PdKey {
            slot_index_be: [0_u8; 26],
            offset: 0,
            chunk_count,
        }]);
        let signed = generator
            .transaction()
            .nonce(nonce)
            .access_list(access_list)
            .input(calldata)
            .into_eip1559();
        let recovered = SignedTransaction::try_into_recovered(signed).unwrap();
        EthPooledTransaction::try_from_consensus(recovered).unwrap()
    }

    fn normal_pooled_transaction(
        generator: &mut TransactionGenerator<StdRng>,
        nonce: u64,
    ) -> EthPooledTransaction {
        let signed = generator.transaction().nonce(nonce).into_eip1559();
        let recovered = SignedTransaction::try_into_recovered(signed).unwrap();
        EthPooledTransaction::try_from_consensus(recovered).unwrap()
    }

    fn make_valid(
        tx: EthPooledTransaction,
        sender: u64,
        timestamp: Instant,
    ) -> Arc<ValidPoolTransaction<EthPooledTransaction>> {
        Arc::new(ValidPoolTransaction {
            transaction_id: TransactionId::new(SenderId::from(sender), tx.nonce()),
            transaction: tx,
            propagate: true,
            timestamp,
            origin: TransactionOrigin::Local,
            authority_ids: None,
        })
    }

    struct StubBestTxIter {
        inner: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    }

    impl StubBestTxIter {
        fn new(txs: Vec<Arc<ValidPoolTransaction<EthPooledTransaction>>>) -> Self {
            Self { inner: txs.into() }
        }
    }

    impl Iterator for StubBestTxIter {
        type Item = Arc<ValidPoolTransaction<EthPooledTransaction>>;

        fn next(&mut self) -> Option<Self::Item> {
            self.inner.pop_front()
        }
    }

    impl BestTransactions for StubBestTxIter {
        fn mark_invalid(&mut self, _transaction: &Self::Item, _kind: InvalidPoolTransactionError) {}

        fn no_updates(&mut self) {}

        fn set_skip_blobs(&mut self, _skip_blobs: bool) {}
    }

    #[test]
    fn combined_iterator_prioritizes_shadow_and_respects_pd_budget() {
        let mut generator = TransactionGenerator::with_num_signers(StdRng::seed_from_u64(1), 1);
        generator.set_base_fee(1);
        generator.set_gas_limit(210_000);

        let timestamp = Instant::now();

        // Prepare transactions
        let shadow_tx = generator.gen_eip1559_pooled();
        let shadow_hash = *shadow_tx.hash();

        let pd_small = pd_pooled_transaction(&mut generator, 0, 2_000);
        let pd_small_hash = *pd_small.hash();

        let pd_large = pd_pooled_transaction(&mut generator, 1, 7_000);
        let pd_large_hash = *pd_large.hash();

        let normal_tx = normal_pooled_transaction(&mut generator, 2);
        let normal_hash = *normal_tx.hash();

        // `pd_small` consumes 2k chunks, `pd_large` would push the total above 7.5k, so it should
        // be deferred and not appear before the normal tx that follows.
        let pool_iter: BestTransactionsIter = Box::new(StubBestTxIter::new(vec![
            make_valid(pd_small, 11, timestamp),
            make_valid(pd_large, 12, timestamp),
            make_valid(normal_tx, 13, timestamp),
        ]));

        let mut iterator =
            CombinedTransactionIterator::new(timestamp, vec![shadow_tx], pool_iter, 7_500);

        let collected: Vec<_> = (&mut iterator).take(3).collect();

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].hash(), &shadow_hash);
        assert_eq!(collected[1].hash(), &pd_small_hash);
        assert_eq!(collected[2].hash(), &normal_hash);
        assert!(collected.iter().all(|tx| tx.hash() != &pd_large_hash));
        assert!(iterator.next().is_none());
    }
}
