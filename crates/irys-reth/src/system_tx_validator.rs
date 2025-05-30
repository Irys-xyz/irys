//! Ethereum transaction validator.
//!
//! Original code:
//! https://github.com/paradigmxyz/reth/blob/91d8ee287bc442d7d421f090defbaeaf8b54643c/crates/transaction-pool/src/validate/eth.rs
use crate::{
    EthPoolTransaction, TransactionValidationOutcome, TransactionValidationTaskExecutor,
    TransactionValidator,
};
use alloy_consensus::{
    constants::{
        EIP1559_TX_TYPE_ID, EIP2930_TX_TYPE_ID, EIP4844_TX_TYPE_ID, EIP7702_TX_TYPE_ID,
        LEGACY_TX_TYPE_ID,
    },
    BlockHeader,
};
use alloy_eips::{
    eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M, eip4844::env_settings::EnvKzgSettings,
    eip7840::BlobParams,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_primitives_traits::{
    transaction::error::InvalidTransactionError, Block, GotExpected, SealedBlock,
};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_tasks::TaskSpawner;
use reth_transaction_pool::{
    error::{
        Eip4844PoolTransactionError, Eip7702PoolTransactionError, InvalidPoolTransactionError,
    },
    metrics::TxPoolValidationMetrics,
    validate::{
        ValidTransaction, ValidationTask, DEFAULT_MAX_TX_INPUT_BYTES, MAX_INIT_CODE_BYTE_SIZE,
    },
    BlobStore, EthBlobTransactionSidecar, LocalTransactionConfig, TransactionOrigin,
};
use std::{
    marker::PhantomData,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    },
    time::Instant,
};
use tokio::sync::Mutex;

/// Validator for Ethereum transactions.
/// It is a [`TransactionValidator`] implementation that validates ethereum transaction.
#[derive(Debug, Clone)]
pub struct SystemTxValidator<Client, T> {
    /// The type that performs the actual validation.
    inner: Arc<SystemTxValidatorInner<Client, T>>,
}

impl<Client, Tx> SystemTxValidator<Client, Tx> {
    /// Returns the configured chain spec
    pub fn chain_spec(&self) -> Arc<Client::ChainSpec>
    where
        Client: ChainSpecProvider,
    {
        self.client().chain_spec()
    }

    /// Returns the configured client
    pub fn client(&self) -> &Client {
        &self.inner.client
    }
}

impl<Client, Tx> SystemTxValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    pub fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        self.inner.validate_one(origin, transaction)
    }

    /// Validates a single transaction with the provided state provider.
    ///
    /// This allows reusing the same provider across multiple transaction validations,
    /// which can improve performance when validating many transactions.
    ///
    /// If `state` is `None`, a new state provider will be created.
    pub fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn StateProvider>>,
    ) -> TransactionValidationOutcome<Tx> {
        self.inner
            .validate_one_with_provider(origin, transaction, state)
    }

    /// Validates all given transactions.
    ///
    /// Returns all outcomes for the given transactions in the same order.
    ///
    /// See also [`Self::validate_one`]
    pub fn validate_all(
        &self,
        transactions: Vec<(TransactionOrigin, Tx)>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        self.inner.validate_batch(transactions)
    }
}

impl<Client, Tx> TransactionValidator for SystemTxValidator<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    type Transaction = Tx;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction)
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.validate_all(transactions)
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: Block,
    {
        self.inner.on_new_head_block(new_tip_block.header())
    }
}

/// A [`TransactionValidator`] implementation that validates ethereum transaction.
///
/// It supports all known ethereum transaction types:
/// - Legacy
/// - EIP-2718
/// - EIP-1559
/// - EIP-4844
/// - EIP-7702
///
/// And enforces additional constraints such as:
/// - Maximum transaction size
/// - Maximum gas limit
///
/// And adheres to the configured [`LocalTransactionConfig`].
#[derive(Debug)]
pub(crate) struct SystemTxValidatorInner<Client, T> {
    /// This type fetches account info from the db
    client: Client,
    /// How to handle [`TransactionOrigin::Local`](TransactionOrigin) transactions.
    local_transactions_config: LocalTransactionConfig,
    /// Maximum size in bytes a single transaction can have in order to be accepted into the pool.
    max_tx_input_bytes: usize,
    /// Marker for the transaction type
    _marker: PhantomData<T>,
}

// === impl EthTransactionValidatorInner ===

impl<Client: ChainSpecProvider, Tx> SystemTxValidatorInner<Client, Tx> {
    /// Returns the configured chain id
    pub(crate) fn chain_id(&self) -> u64 {
        self.client.chain_spec().chain().id()
    }
}

impl<Client, Tx> SystemTxValidatorInner<Client, Tx>
where
    Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory,
    Tx: EthPoolTransaction,
{
    /// Returns the configured chain spec
    fn chain_spec(&self) -> Arc<Client::ChainSpec> {
        self.client.chain_spec()
    }

    /// Validates a single transaction using an optional cached state provider.
    /// If no provider is passed, a new one will be created. This allows reusing
    /// the same provider across multiple txs.
    fn validate_one_with_provider(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        maybe_state: &mut Option<Box<dyn StateProvider>>,
    ) -> TransactionValidationOutcome<Tx> {
        match self.validate_one_no_state(origin, transaction) {
            Ok(transaction) => {
                // stateless checks passed, pass transaction down stateful validation pipeline
                // If we don't have a state provider yet, fetch the latest state
                if maybe_state.is_none() {
                    match self.client.latest() {
                        Ok(new_state) => {
                            *maybe_state = Some(new_state);
                        }
                        Err(err) => {
                            return TransactionValidationOutcome::Error(
                                *transaction.hash(),
                                Box::new(err),
                            )
                        }
                    }
                }

                let state = maybe_state.as_deref().expect("provider is set");

                self.validate_one_against_state(origin, transaction, state)
            }
            Err(invalid_outcome) => invalid_outcome,
        }
    }

    /// Performs stateless validation on single transaction. Returns unaltered input transaction
    /// if all checks pass, so transaction can continue through to stateful validation as argument
    /// to [`validate_one_against_state`](Self::validate_one_against_state).
    fn validate_one_no_state(
        &self,
        _origin: TransactionOrigin,
        transaction: Tx,
    ) -> Result<Tx, TransactionValidationOutcome<Tx>> {
        // Checks for tx_type - Only allow legacy transactions
        match transaction.ty() {
            LEGACY_TX_TYPE_ID => {
                // Accept legacy transactions only
            }
            EIP2930_TX_TYPE_ID => {
                return Err(TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidTransactionError::Eip2930Disabled.into(),
                ));
            }
            EIP1559_TX_TYPE_ID => {
                return Err(TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidTransactionError::Eip1559Disabled.into(),
                ));
            }
            EIP4844_TX_TYPE_ID => {
                return Err(TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidTransactionError::Eip4844Disabled.into(),
                ));
            }
            EIP7702_TX_TYPE_ID => {
                return Err(TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidTransactionError::Eip7702Disabled.into(),
                ));
            }
            _ => {
                return Err(TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidTransactionError::TxTypeNotSupported.into(),
                ))
            }
        };

        // Reject transactions over defined size to prevent DOS attacks
        let tx_input_len = transaction.input().len();
        if tx_input_len > self.max_tx_input_bytes {
            return Err(TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::OversizedData(tx_input_len, self.max_tx_input_bytes),
            ));
        }

        // Checks for chainid
        if let Some(chain_id) = transaction.chain_id() {
            if chain_id != self.chain_id() {
                return Err(TransactionValidationOutcome::Invalid(
                    transaction,
                    InvalidTransactionError::ChainIdMismatch.into(),
                ));
            }
        }

        Ok(transaction)
    }

    /// Validates a single transaction using given state provider.
    /// Simplified for legacy-only transactions with minimal validation.
    fn validate_one_against_state<P>(
        &self,
        origin: TransactionOrigin,
        mut transaction: Tx,
        state: P,
    ) -> TransactionValidationOutcome<Tx>
    where
        P: StateProvider,
    {
        // Use provider to get account info
        let account = match state.basic_account(transaction.sender_ref()) {
            Ok(account) => account.unwrap_or_default(),
            Err(err) => {
                return TransactionValidationOutcome::Error(*transaction.hash(), Box::new(err))
            }
        };

        if account.bytecode_hash.is_some() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::SignerAccountHasBytecode.into(),
            );
        }

        let tx_nonce = transaction.nonce();

        // Checks for nonce
        if tx_nonce < account.nonce {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::NonceNotConsistent {
                    tx: tx_nonce,
                    state: account.nonce,
                }
                .into(),
            );
        }

        if transaction.authorization_list().is_some() {
            tracing::error!("transaction authorities list must be empty");
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        // Return the valid transaction
        TransactionValidationOutcome::Valid {
            balance: account.balance,
            state_nonce: account.nonce,
            bytecode_hash: account.bytecode_hash,
            transaction: ValidTransaction::new(transaction, None),
            // by this point assume all external transactions should be propagated
            propagate: match origin {
                TransactionOrigin::External => true,
                TransactionOrigin::Local => {
                    self.local_transactions_config.propagate_local_transactions
                }
                TransactionOrigin::Private => false,
            },
            authorities: None,
        }
    }

    /// Validates a single transaction.
    fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        let mut provider = None;
        self.validate_one_with_provider(origin, transaction, &mut provider)
    }

    /// Validates all given transactions.
    fn validate_batch(
        &self,
        transactions: Vec<(TransactionOrigin, Tx)>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        let mut provider = None;
        transactions
            .into_iter()
            .map(|(origin, tx)| self.validate_one_with_provider(origin, tx, &mut provider))
            .collect()
    }

    fn on_new_head_block<T: BlockHeader>(&self, _new_tip_block: &T) {
        // No fork tracking needed for legacy-only transactions
        // This is a no-op for minimal validation
    }
}

/// A builder for [`EthTransactionValidator`] and [`TransactionValidationTaskExecutor`]
#[derive(Debug)]
pub struct SystemTxValidatorBuilder<Client> {
    client: Client,
    /// How to handle [`TransactionOrigin::Local`](TransactionOrigin) transactions.
    local_transactions_config: LocalTransactionConfig,
    /// Max size in bytes of a single transaction allowed
    max_tx_input_bytes: usize,
}

impl<Client> SystemTxValidatorBuilder<Client> {
    /// Creates a new builder for the given client
    ///
    /// This validator only accepts legacy transactions and performs minimal validation.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            local_transactions_config: Default::default(),
            max_tx_input_bytes: DEFAULT_MAX_TX_INPUT_BYTES,
        }
    }
    /// Whether to allow exemptions for local transaction exemptions.
    pub fn with_local_transactions_config(
        mut self,
        local_transactions_config: LocalTransactionConfig,
    ) -> Self {
        self.local_transactions_config = local_transactions_config;
        self
    }

    /// Sets a max size in bytes of a single transaction allowed into the pool
    pub const fn with_max_tx_input_bytes(mut self, max_tx_input_bytes: usize) -> Self {
        self.max_tx_input_bytes = max_tx_input_bytes;
        self
    }

    /// Builds a the [`SystemTxValidator`] without spawning validator tasks.
    pub fn build<Tx>(self) -> SystemTxValidator<Client, Tx> {
        let Self {
            client,
            local_transactions_config,
            max_tx_input_bytes,
            ..
        } = self;

        let inner = SystemTxValidatorInner {
            client,
            local_transactions_config,
            max_tx_input_bytes,
            _marker: Default::default(),
        };

        SystemTxValidator {
            inner: Arc::new(inner),
        }
    }

    /// Builds a [`SystemTxValidator`] and spawns validation tasks via the
    /// [`TransactionValidationTaskExecutor`]
    ///
    /// The validator will spawn `additional_tasks` additional tasks for validation.
    ///
    /// By default this will spawn 1 additional task.
    pub fn build_with_tasks<Tx, T>(
        self,
        tasks: T,
    ) -> TransactionValidationTaskExecutor<SystemTxValidator<Client, Tx>>
    where
        T: TaskSpawner,
    {
        let additional_tasks = 1;
        let validator = self.build();

        let (tx, task) = ValidationTask::new();

        // Spawn validation tasks, they are blocking because they perform db lookups
        for _ in 0..additional_tasks {
            let task = task.clone();
            tasks.spawn_blocking(Box::pin(async move {
                task.run().await;
            }));
        }

        // we spawn them on critical tasks because validation, especially for EIP-4844 can be quite
        // heavy
        tasks.spawn_critical_blocking(
            "system-tx-transaction-validation-service",
            Box::pin(async move {
                task.run().await;
            }),
        );

        let to_validation_task = Arc::new(Mutex::new(tx));

        TransactionValidationTaskExecutor {
            validator,
            to_validation_task,
        }
    }
}
