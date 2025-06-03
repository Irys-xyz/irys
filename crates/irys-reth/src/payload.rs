//! A basic Ethereum payload builder implementation.

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
    error::InvalidPoolTransactionError, BestTransactions, BestTransactionsAttributes,
    EthPooledTransaction, TransactionPool, ValidPoolTransaction,
};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tracing::{debug, warn};

use reth_ethereum_payload_builder::{default_ethereum_payload, EthereumBuilderConfig};

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
    /// System txs don't live inside the tx pool, so they need to be handled separately.
    system_tx_requester: std::sync::mpsc::Sender<SystemTxRequest>,
}

pub struct SystemTxRequest {
    /// Payload id
    pub payload_id: PayloadId,
    /// oneshot channel to send the system txs response
    pub response_tx: tokio::sync::oneshot::Sender<Vec<ValidPoolTransaction<EthPooledTransaction>>>,
}

/// Combined iterator that yields system transactions first, then pool transactions
pub struct CombinedTransactionIterator {
    /// System transactions to yield first
    system_txs: VecDeque<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
    /// Pool transactions iterator
    pool_iter: BestTransactionsIter,
}

impl CombinedTransactionIterator {
    /// Create a new combined iterator
    pub fn new(
        system_txs: Vec<ValidPoolTransaction<EthPooledTransaction>>,
        pool_iter: BestTransactionsIter,
    ) -> Self {
        let system_txs = system_txs
            .into_iter()
            .map(Arc::new)
            .collect::<VecDeque<_>>();

        Self {
            system_txs,
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
        // For system transactions, we just remove them from our queue
        // as they cannot be marked invalid in the same way as pool transactions
        self.system_txs.retain(|tx| {
            warn!(
                "mark_invalid: tx: {:?}, tx.hash(): {:?}, transaction.hash(): {:?}",
                tx,
                tx.hash(),
                transaction.hash()
            );
            tx.hash() != transaction.hash()
        });

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
    /// `IyrsPayloadBuilder` constructor.
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
        system_tx_requester: std::sync::mpsc::Sender<SystemTxRequest>,
    ) -> Self {
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            system_tx_requester,
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
    pub fn best_transactions_with_attributes(
        &self,
        attributes: BestTransactionsAttributes,
        payload_id: PayloadId,
    ) -> BestTransactionsIter {
        // Get pool transactions iterator
        let pool_txs = self.pool.best_transactions_with_attributes(attributes);

        // Try to get system transactions from the channel
        let system_txs = self.try_get_system_transactions(payload_id);

        // Create combined iterator
        Box::new(CombinedTransactionIterator::new(system_txs, pool_txs))
    }

    /// Attempts to get system transactions from the channel
    /// Returns empty vector if no system transactions are available or if there's an error
    fn try_get_system_transactions(
        &self,
        payload_id: PayloadId,
    ) -> Vec<ValidPoolTransaction<EthPooledTransaction>> {
        // Create oneshot channel for receiving system transactions
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        let request = SystemTxRequest {
            payload_id,
            response_tx,
        };

        // Try to send the request
        if let Err(e) = self.system_tx_requester.send(request) {
            warn!("Failed to send system tx request: {}", e);
            return Vec::new();
        }

        // Try to receive system transactions with a timeout
        // Note: This is a blocking call, but with a short timeout
        let rt = tokio::runtime::Handle::try_current();

        if let Ok(handle) = rt {
            match handle.block_on(async {
                tokio::time::timeout(Duration::from_millis(100), response_rx).await
            }) {
                Ok(Ok(system_txs)) => {
                    debug!("Received {} system transactions", system_txs.len());
                    system_txs
                }
                Ok(Err(e)) => {
                    warn!("Failed to receive system transactions: {}", e);
                    Vec::new()
                }
                Err(_) => {
                    debug!("Timeout waiting for system transactions");
                    Vec::new()
                }
            }
        } else {
            // If we're not in a tokio runtime, we can't await
            warn!("Not in tokio runtime, cannot retrieve system transactions");
            Vec::new()
        }
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
        let payload_id = args.config.payload_id();
        let result = default_ethereum_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            args,
            |attributes| self.best_transactions_with_attributes(attributes, payload_id),
        );
        result
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
        let payload_id = config.payload_id();
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
