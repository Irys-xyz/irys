//! A basic Ethereum payload builder implementation.
//! Original impl: https://github.com/Irys-xyz/reth-irys/blob/6c892d38bfcd6689c618386bd7ccc6fde7fbb64e/crates/ethereum/payload/src/lib.rs?plain=1#L1

use crate::IrysPayloadBuilderAttributes;
use alloy_consensus::Transaction as _;
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm_ethereum::EthEvmConfig;
use reth_payload_builder::EthBuiltPayload;
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, EthPooledTransaction, TransactionOrigin,
    TransactionPool, ValidPoolTransaction,
    error::InvalidPoolTransactionError,
    identifier::{SenderId, TransactionId},
};
use revm_primitives::FixedBytes;
use std::collections::HashSet;
use std::{collections::VecDeque, sync::Arc, time::Instant};

use reth_ethereum_payload_builder::{EthereumBuilderConfig, default_ethereum_payload};

type BestTransactionsIter =
    Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>>;

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
    fn mark_invalid(&mut self, transaction: &Self::Item, kind: &InvalidPoolTransactionError) {
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
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
        }
    }
}

// Default implementation of [PayloadBuilder] for unit type
impl<Pool, Client, EvmConfig> IrysPayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction = EthPooledTransaction>,
{
    /// Creates a combined transaction iterator with shadow transactions first, then pool transactions.
    ///
    /// Shadow transactions are passed directly from the payload attributes.
    pub fn best_transactions_with_attributes(
        &self,
        attributes: BestTransactionsAttributes,
        shadow_txs: Vec<EthPooledTransaction>,
    ) -> BestTransactionsIter {
        let timestamp = Instant::now();

        // Get pool transactions iterator
        let pool_txs = self.pool.best_transactions_with_attributes(attributes);

        // Create combined iterator with shadow txs from attributes
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
    type Attributes = IrysPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<IrysPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        // Extract shadow transactions from attributes
        let shadow_txs = args.config.attributes.shadow_txs.clone();

        // Convert IrysPayloadBuilderAttributes to EthPayloadBuilderAttributes for the inner builder
        let BuildArguments {
            cached_reads,
            config,
            cancel,
            best_payload,
        } = args;
        let PayloadConfig {
            parent_header,
            attributes,
        } = config;
        let eth_args = BuildArguments {
            cached_reads,
            config: PayloadConfig {
                parent_header,
                attributes: attributes.inner,
            },
            cancel,
            best_payload,
        };
        let result = default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            eth_args,
            |attributes| self.best_transactions_with_attributes(attributes, shadow_txs.clone()),
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
        // Extract shadow transactions from attributes
        let shadow_txs = config.attributes.shadow_txs.clone();

        // Convert IrysPayloadBuilderAttributes to EthPayloadBuilderAttributes for the inner builder
        let PayloadConfig {
            parent_header,
            attributes,
        } = config;
        let eth_config = PayloadConfig {
            parent_header,
            attributes: attributes.inner,
        };
        let args = BuildArguments::new(Default::default(), eth_config, Default::default(), None);

        default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.best_transactions_with_attributes(attributes, shadow_txs.clone()),
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}
