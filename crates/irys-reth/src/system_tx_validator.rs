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
pub struct EthTransactionValidatorBuilder<Client> {
    client: Client,
    /// How to handle [`TransactionOrigin::Local`](TransactionOrigin) transactions.
    local_transactions_config: LocalTransactionConfig,
    /// Max size in bytes of a single transaction allowed
    max_tx_input_bytes: usize,
}

impl<Client> EthTransactionValidatorBuilder<Client> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blobstore::InMemoryBlobStore, error::PoolErrorKind, traits::PoolTransaction,
        CoinbaseTipOrdering, EthPooledTransaction, Pool, TransactionPool,
    };
    use alloy_consensus::Transaction;
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{hex, U256};
    use reth_ethereum_primitives::PooledTransaction;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    fn get_transaction() -> EthPooledTransaction {
        let raw = "0x02f914950181ad84b2d05e0085117553845b830f7df88080b9143a6040608081523462000414576200133a803803806200001e8162000419565b9283398101608082820312620004145781516001600160401b03908181116200041457826200004f9185016200043f565b92602092838201519083821162000414576200006d9183016200043f565b8186015190946001600160a01b03821692909183900362000414576060015190805193808511620003145760038054956001938488811c9816801562000409575b89891014620003f3578190601f988981116200039d575b50899089831160011462000336576000926200032a575b505060001982841b1c191690841b1781555b8751918211620003145760049788548481811c9116801562000309575b89821014620002f457878111620002a9575b5087908784116001146200023e5793839491849260009562000232575b50501b92600019911b1c19161785555b6005556007805460ff60a01b19169055600880546001600160a01b0319169190911790553015620001f3575060025469d3c21bcecceda100000092838201809211620001de57506000917fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9160025530835282815284832084815401905584519384523093a351610e889081620004b28239f35b601190634e487b7160e01b6000525260246000fd5b90606493519262461bcd60e51b845283015260248201527f45524332303a206d696e7420746f20746865207a65726f2061646472657373006044820152fd5b0151935038806200013a565b9190601f198416928a600052848a6000209460005b8c8983831062000291575050501062000276575b50505050811b0185556200014a565b01519060f884600019921b161c191690553880808062000267565b86860151895590970196948501948893500162000253565b89600052886000208880860160051c8201928b8710620002ea575b0160051c019085905b828110620002dd5750506200011d565b60008155018590620002cd565b92508192620002c4565b634e487b7160e01b600052602260045260246000fd5b97607f1697620000ae565b50503461011c578160031936011261011c5760085490516001600160a01b039091168152602090f35b50503461011c578160031936011261011c576020906005549051908152f35b833461061a578060031936011261061a576107dd610b5a565b338252600160209081528383206001600160a01b038316845290528282205460243581019290831061054f57602084610519858533610d31565b50503461011c578160031936011261011c576020905160128152f35b83833461011c57606036600319011261011c5761084e610b5a565b610856610b75565b6044359160018060a01b0381169485815260209560018752858220338352875285822054976000198903610893575b505050906105199291610bc3565b85891061096957811561091a5733156108cc5750948481979861051997845260018a528284203385528a52039120558594938780610885565b865162461bcd60e51b8152908101889052602260248201527f45524332303a20617070726f766520746f20746865207a65726f206164647265604482015261737360f01b6064820152608490fd5b865162461bcd60e51b81529081018890526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b865162461bcd60e51b8152908101889052601d60248201527f45524332303a20696e73756666696369656e7420616c6c6f77616e63650000006044820152606490fd5b50503461011c578160031936011261011c5760209060ff60075460a01c1690519015158152f35b50503461011c578160031936011261011c576020906002549051908152f35b50503461011c578060031936011261011c57602090610519610a12610b5a565b6024359033610d31565b92915034610b0d5783600319360112610b0d57600354600181811c9186908281168015610b03575b6020958686108214610af05750848852908115610ace5750600114610a75575b6106838686610679828b0383610b8b565b929550600383527fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b5b828410610abb575050508261068394610679928201019438610a64565b8054868501880152928601928101610a9e565b60ff191687860152505050151560051b83010192506106798261068338610a64565b634e487b7160e01b845260229052602483fd5b93607f1693610a44565b8380fd5b6020808252825181830181905290939260005b828110610b4657505060409293506000838284010152601f8019910116010190565b818101860151848201604001528501610b24565b600435906001600160a01b0382168203610b7057565b600080fd5b602435906001600160a01b0382168203610b7057565b90601f8019910116810190811067ffffffffffffffff821117610bad57604052565b634e487b7160e01b600052604160045260246000fd5b6001600160a01b03908116918215610cde5716918215610c8d57600082815280602052604081205491808310610c3957604082827fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef958760209652828652038282205586815220818154019055604051908152a3565b60405162461bcd60e51b815260206004820152602660248201527f45524332303a207472616e7366657220616d6f756e7420657863656564732062604482015265616c616e636560d01b6064820152608490fd5b60405162461bcd60e51b815260206004820152602360248201527f45524332303a207472616e7366657220746f20746865207a65726f206164647260448201526265737360e81b6064820152608490fd5b60405162461bcd60e51b815260206004820152602560248201527f45524332303a207472616e736665722066726f6d20746865207a65726f206164604482015264647265737360d81b6064820152608490fd5b6001600160a01b03908116918215610de25716918215610d925760207f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925918360005260018252604060002085600052825280604060002055604051908152a3565b60405162461bcd60e51b815260206004820152602260248201527f45524332303a20617070726f766520746f20746865207a65726f206164647265604482015261737360f01b6064820152608490fd5b60405162461bcd60e51b8152602060048201526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b90816020910312610b7057516001600160a01b0381168103610b70579056fea2646970667358221220285c200b3978b10818ff576bb83f2dc4a2a7c98dfb6a36ea01170de792aa652764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000d3fd4f95820a9aa848ce716d6c200eaefb9a2e4900000000000000000000000000000000000000000000000000000000000000640000000000000000000000000000000000000000000000000000000000000003543131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000035431310000000000000000000000000000000000000000000000000000000000c001a04e551c75810ffdfe6caff57da9f5a8732449f42f0f4c57f935b05250a76db3b6a046cd47e6d01914270c1ec0d9ac7fae7dfb240ec9a8b6ec7898c4d6aa174388f2";

        let data = hex::decode(raw).unwrap();
        let tx = PooledTransaction::decode_2718(&mut data.as_ref()).unwrap();

        EthPooledTransaction::from_pooled(tx.try_into_recovered().unwrap())
    }

    // <https://github.com/paradigmxyz/reth/issues/5178>
    #[tokio::test]
    async fn validate_transaction() {
        let transaction = get_transaction();
        let mut fork_tracker = ForkTracker {
            shanghai: false.into(),
            cancun: false.into(),
            prague: false.into(),
            max_blob_count: 0.into(),
        };

        let res = ensure_intrinsic_gas(&transaction, &fork_tracker);
        assert!(res.is_ok());

        fork_tracker.shanghai = true.into();
        let res = ensure_intrinsic_gas(&transaction, &fork_tracker);
        assert!(res.is_ok());

        let provider = MockEthProvider::default();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );
        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider).build(blob_store.clone());

        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());

        assert!(outcome.is_valid());

        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            Default::default(),
        );

        let res = pool.add_external_transaction(transaction.clone()).await;
        assert!(res.is_ok());
        let tx = pool.get(transaction.hash());
        assert!(tx.is_some());
    }

    // <https://github.com/paradigmxyz/reth/issues/8550>
    #[tokio::test]
    async fn invalid_on_gas_limit_too_high() {
        let transaction = get_transaction();

        let provider = MockEthProvider::default();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider)
            .set_block_gas_limit(1_000_000) // tx gas limit is 1_015_288
            .build(blob_store.clone());

        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());

        assert!(outcome.is_invalid());

        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            Default::default(),
        );

        let res = pool.add_external_transaction(transaction.clone()).await;
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsGasLimit(
                1_015_288, 1_000_000
            ))
        ));
        let tx = pool.get(transaction.hash());
        assert!(tx.is_none());
    }

    #[tokio::test]
    async fn invalid_on_fee_cap_exceeded() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider)
            .set_tx_fee_cap(100) // 100 wei cap
            .build(blob_store.clone());

        let outcome = validator.validate_one(TransactionOrigin::Local, transaction.clone());
        assert!(outcome.is_invalid());

        if let TransactionValidationOutcome::Invalid(_, err) = outcome {
            assert!(matches!(
                err,
                InvalidPoolTransactionError::ExceedsFeeCap { max_tx_fee_wei, tx_fee_cap_wei }
                if (max_tx_fee_wei > tx_fee_cap_wei)
            ));
        }

        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            Default::default(),
        );
        let res = pool
            .add_transaction(TransactionOrigin::Local, transaction.clone())
            .await;
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsFeeCap { .. })
        ));
        let tx = pool.get(transaction.hash());
        assert!(tx.is_none());
    }

    #[tokio::test]
    async fn valid_on_zero_fee_cap() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider)
            .set_tx_fee_cap(0) // no cap
            .build(blob_store);

        let outcome = validator.validate_one(TransactionOrigin::Local, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn valid_on_normal_fee_cap() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider)
            .set_tx_fee_cap(2e18 as u128) // 2 ETH cap
            .build(blob_store);

        let outcome = validator.validate_one(TransactionOrigin::Local, transaction);
        assert!(outcome.is_valid());
    }
}
