use std::{marker::PhantomData, sync::Arc, time::SystemTime};

use alloy_consensus::TxLegacy;
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_primitives::{Address, TxKind, U256};
use alloy_rlp::{Decodable, Encodable};
use evm::{CustomBlockAssembler, MyEthEvmFactory};
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodePrimitives, NodeTypes, PayloadTypes},
    builder::{
        components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder, PoolBuilder},
        BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder, PayloadBuilderConfig,
    },
    payload::{EthBuiltPayload, EthPayloadBuilderAttributes},
    primitives::EthPrimitives,
    providers::{providers::ProviderFactoryBuilder, CanonStateSubscriptions, EthStorage},
    transaction_pool::TransactionValidationTaskExecutor,
};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_ethereum::{
    node::{
        EthereumAddOns, EthereumConsensusBuilder, EthereumNetworkBuilder, EthereumPayloadBuilder,
    },
    EthEngineTypes, EthEvmConfig,
};
use reth_tracing::tracing::{self};
use reth_transaction_pool::{
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
    EthPooledTransaction, EthTransactionValidator, Pool, PoolTransaction, Priority,
    TransactionOrdering,
};
use reth_trie_db::MerklePatriciaTrie;
use std::sync::LazyLock;
use system_tx::SystemTransaction;
use tracing::{debug, info};

pub mod system_tx;

pub fn compose_system_tx(nonce: u64, chain_id: u64, system_tx: SystemTransaction) -> TxLegacy {
    let mut system_tx_rlp = Vec::with_capacity(512);
    system_tx.encode(&mut system_tx_rlp);
    let tx_raw = TxLegacy {
        gas_limit: 99000,
        value: U256::ZERO,
        nonce,
        gas_price: 1_000_000_000u128, // 1 Gwei
        chain_id: Some(chain_id),
        to: TxKind::Call(Address::ZERO),
        input: system_tx_rlp.into(),
        ..Default::default()
    };
    tx_raw
}

// todo: write tests
// todo: see how 2 nodes behave
// todo exepriments how to prevent user from producing system tx -- maybe we can access the block producer address
// todo: custom mempool - don't drop system txs if they dont have gas properties
// todo: custom mempool - after each block, drop all system txs

/// Type configuration for an Irys-Ethereum node.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct IrysEthereumNode;

impl NodeTypes for IrysEthereumNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl IrysEthereumNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular Ethereum node.
    pub fn components<Node>() -> ComponentsBuilder<
        Node,
        CustomPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        CustomEthereumExecutorBuilder,
        EthereumConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
        <Node::Types as NodeTypes>::Payload: PayloadTypes<
            BuiltPayload = EthBuiltPayload,
            PayloadAttributes = EthPayloadAttributes,
            PayloadBuilderAttributes = EthPayloadBuilderAttributes,
        >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(CustomPoolBuilder::default())
            .executor(CustomEthereumExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl<N> Node<N> for IrysEthereumNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        CustomPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        CustomEthereumExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = EthereumAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components()
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for IrysEthereumNode {
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        let alloy_rpc_types_eth::Block {
            header,
            transactions,
            withdrawals,
            ..
        } = rpc_block;
        reth_ethereum_primitives::Block {
            header: header.inner,
            body: reth_ethereum_primitives::BlockBody {
                transactions: transactions
                    .into_transactions()
                    .map(|tx| tx.inner.into_inner().into())
                    .collect(),
                ommers: Default::default(),
                withdrawals,
            },
        }
    }
}

/// A custom pool builder
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CustomPoolBuilder;

// todo add a link form where this code was copied from
/// Implement the [`PoolBuilder`] trait for the custom pool builder
///
/// This will be used to build the transaction pool and its maintenance tasks during launch.
impl<Types, Node> PoolBuilder<Node> for CustomPoolBuilder
where
    Types: NodeTypes<
        ChainSpec: EthereumHardforks,
        Primitives: NodePrimitives<SignedTx = TransactionSigned>,
    >,
    Node: FullNodeTypes<Types = Types>,
{
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            EthTransactionValidator<Node::Provider, EthPooledTransaction>,
        >,
        SystemTxsCoinbaseTipOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.config().datadir();
        let pool_config = ctx.pool_config();

        let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
            blob_cache_size
        } else {
            // get the current blob params for the current timestamp, fallback to default Cancun
            // params
            let current_timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs();
            let blob_params = ctx
                .chain_spec()
                .blob_params_at_timestamp(current_timestamp)
                .unwrap_or_else(BlobParams::cancun);

            // Derive the blob cache size from the target blob count, to auto scale it by
            // multiplying it with the slot count for 2 epochs: 384 for pectra
            (blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32
        };

        let custom_config =
            DiskFileBlobStoreConfig::default().with_max_cached_entries(blob_cache_size);

        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), custom_config)?;
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
            .with_head_timestamp(ctx.head().timestamp)
            .kzg_settings(ctx.kzg_settings()?)
            .with_local_transactions_config(pool_config.local_transactions_config.clone())
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        let ordering = SystemTxsCoinbaseTipOrdering::default();
        let transaction_pool =
            reth_transaction_pool::Pool::new(validator, ordering, blob_store, pool_config);
        info!(target: "reth::cli", "Transaction pool initialized");

        // spawn txpool maintenance task
        {
            let pool = transaction_pool.clone();
            let chain_events = ctx.provider().canonical_state_stream();
            let client = ctx.provider().clone();
            // Only spawn backup task if not disabled
            if !ctx.config().txpool.disable_transactions_backup {
                // Use configured backup path or default to data dir
                let transactions_path = ctx
                    .config()
                    .txpool
                    .transactions_backup_path
                    .clone()
                    .unwrap_or_else(|| data_dir.txpool_transactions());

                let transactions_backup_config =
                    reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

                ctx.task_executor()
                    .spawn_critical_with_graceful_shutdown_signal(
                        "local transactions backup task",
                        |shutdown| {
                            reth_transaction_pool::maintain::backup_local_transactions_task(
                                shutdown,
                                pool.clone(),
                                transactions_backup_config,
                            )
                        },
                    );
            }

            // spawn the maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client,
                    pool,
                    chain_events,
                    ctx.task_executor().clone(),
                    reth_transaction_pool::maintain::MaintainPoolConfig {
                        max_tx_lifetime: transaction_pool.config().max_queued_lifetime,
                        no_local_exemptions: transaction_pool
                            .config()
                            .local_transactions_config
                            .no_exemptions,
                        ..Default::default()
                    },
                ),
            );
            debug!(target: "reth::cli", "Spawned txpool maintenance task");
        }

        Ok(transaction_pool)
    }
}

/// System txs go to the top
/// The transactions are ordered by their coinbase tip.
/// The higher the coinbase tip is, the higher the priority of the transaction.
#[derive(Debug)]
#[non_exhaustive]
pub struct SystemTxsCoinbaseTipOrdering<T>(PhantomData<T>);

impl<T> TransactionOrdering for SystemTxsCoinbaseTipOrdering<T>
where
    T: PoolTransaction + 'static,
{
    type PriorityValue = U256;
    type Transaction = T;

    /// Source: <https://github.com/ethereum/go-ethereum/blob/7f756dc1185d7f1eeeacb1d12341606b7135f9ea/core/txpool/legacypool/list.go#L469-L482>.
    ///
    /// NOTE: The implementation is incomplete for missing base fee.
    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        let tx_envelope_input_buf = transaction.input();
        let rlp_decoded_system_tx =
            system_tx::SystemTransaction::decode(&mut &tx_envelope_input_buf[..]);
        if rlp_decoded_system_tx.is_ok() {
            return Priority::Value(U256::MAX);
        }
        transaction
            .effective_tip_per_gas(base_fee)
            .map(U256::from)
            .into()
    }
}

impl<T> Default for SystemTxsCoinbaseTipOrdering<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<T> Clone for SystemTxsCoinbaseTipOrdering<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// A regular ethereum evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
pub struct CustomEthereumExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for CustomEthereumExecutorBuilder
where
    Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = evm::CustomEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = EthEvmConfig::new(ctx.chain_spec())
            .with_extra_data(ctx.payload_builder_config().extra_data_bytes());
        let spec = ctx.chain_spec();
        let evm_factory = MyEthEvmFactory::default();
        let evm_config = evm::CustomEvmConfig {
            inner: evm_config,
            assembler: CustomBlockAssembler::new(ctx.chain_spec()),
            executor_factory: evm::CustomBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                spec,
                evm_factory,
            ),
        };
        Ok(evm_config)
    }
}

mod evm {

    use std::convert::Infallible;

    use alloy_consensus::{Block, Header, Transaction};
    use alloy_dyn_abi::DynSolValue;
    use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
    use alloy_evm::eth::receipt_builder::ReceiptBuilder;

    use alloy_evm::eth::EthBlockExecutor;
    use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};

    use alloy_primitives::{keccak256, Bytes, FixedBytes, Log, LogData};
    use reth::primitives::{SealedBlock, SealedHeader};
    use reth::providers::BlockExecutionResult;
    use reth::revm::context::result::ExecutionResult;
    use reth::revm::context::TxEnv;
    use reth::revm::primitives::hardfork::SpecId;
    use reth::revm::{Inspector, State};
    use reth_ethereum_primitives::Receipt;
    use reth_evm::block::{BlockExecutorFactory, BlockExecutorFor, BlockValidationError};
    use reth_evm::eth::receipt_builder::ReceiptBuilderCtx;

    use reth_evm::eth::{EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmContext};
    use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
    use reth_evm::precompiles::PrecompilesMap;
    use reth_evm::{
        ConfigureEvm, EthEvm, EthEvmFactory, EvmEnv, EvmFactory, NextBlockEnvAttributes,
    };
    use reth_evm_ethereum::{EthBlockAssembler, RethReceiptBuilder};
    use revm::context::result::{EVMError, HaltReason, InvalidTransaction, Output};
    use revm::context::{BlockEnv, CfgEnv};

    use revm::database::states::plain_account::PlainStorage;
    use revm::database::PlainAccount;
    use revm::inspector::NoOpInspector;
    use revm::precompile::{PrecompileSpecId, Precompiles};
    use revm::state::{Account, EvmStorageSlot};
    use revm::{DatabaseCommit, MainBuilder, MainContext};

    use crate::system_tx::system_tx_topics;

    use super::*;

    pub(crate) struct CustomBlockExecutor<'a, Evm> {
        receipt_builder: &'a RethReceiptBuilder,
        system_call_receipts: Vec<Receipt>,
        pub inner: EthBlockExecutor<'a, Evm, &'a Arc<ChainSpec>, &'a RethReceiptBuilder>,
    }

    impl<'db, DB, E> BlockExecutor for CustomBlockExecutor<'_, E>
    where
        DB: Database + 'db,
        E: Evm<
            DB = &'db mut State<DB>,
            Tx: FromRecoveredTx<TransactionSigned> + FromTxWithEncoded<TransactionSigned>,
        >,
    {
        type Transaction = TransactionSigned;
        type Receipt = Receipt;
        type Evm = E;

        fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
            self.inner.apply_pre_execution_changes()
        }

        fn execute_transaction_with_result_closure(
            &mut self,
            tx: impl ExecutableTx<Self>,
            f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>),
        ) -> Result<u64, BlockExecutionError> {
            let tx_envelope = tx.tx();
            let tx_envelope_input_buf = tx_envelope.input();
            let rlp_decoded_system_tx =
                system_tx::SystemTransaction::decode(&mut &tx_envelope_input_buf[..]);

            if let Ok(system_tx) = rlp_decoded_system_tx {
                // Handle the signer nonce increment
                self.increment_signer_nonce(&tx)?;

                // Process different system transaction types
                let topic = system_tx.topic();
                let target;
                let new_account_state = match system_tx {
                    system_tx::SystemTransaction::ReleaseStake(balance_increment) => {
                        let log = self.create_system_log(
                            balance_increment.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_increment.amount, 256),
                                DynSolValue::Address(balance_increment.target),
                            ],
                        );
                        target = balance_increment.target;
                        let res = self.handle_balance_increment(log, balance_increment);
                        Ok(res)
                    }
                    system_tx::SystemTransaction::BlockReward(balance_increment) => {
                        let log = self.create_system_log(
                            balance_increment.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_increment.amount, 256),
                                DynSolValue::Address(balance_increment.target),
                            ],
                        );
                        target = balance_increment.target;
                        let res = self.handle_balance_increment(log, balance_increment);
                        Ok(res)
                    }
                    system_tx::SystemTransaction::Stake(balance_decrement) => {
                        let log = self.create_system_log(
                            balance_decrement.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_decrement.amount, 256),
                                DynSolValue::Address(balance_decrement.target),
                            ],
                        );
                        target = balance_decrement.target;
                        self.handle_balance_decrement(log, tx_envelope.hash(), balance_decrement)?
                    }
                    system_tx::SystemTransaction::StorageFees(balance_decrement) => {
                        let log = self.create_system_log(
                            balance_decrement.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_decrement.amount, 256),
                                DynSolValue::Address(balance_decrement.target),
                            ],
                        );
                        target = balance_decrement.target;
                        self.handle_balance_decrement(log, tx_envelope.hash(), balance_decrement)?
                    }
                };

                // at this point, the system tx has been processed, and it was valid *enough*
                // that we should generate a receipt for it even in a failure state
                let mut new_state = alloy_primitives::map::foldhash::HashMap::default();
                let execution_result = match new_account_state {
                    Ok((plain_account, execution_result)) => {
                        let storage = plain_account
                            .storage
                            .iter()
                            .map(|(k, v)| (*k, EvmStorageSlot::new(*v)))
                            .collect();
                        new_state.insert(
                            target,
                            Account {
                                info: plain_account.info,
                                storage,
                                status: revm::state::AccountStatus::Touched,
                            },
                        );
                        dbg!(&execution_result);

                        execution_result
                    }
                    Err(execution_result) => execution_result,
                };

                f(&execution_result);

                dbg!(&new_state);
                // Build and store the receipt
                let evm = self.inner.evm_mut();
                self.system_call_receipts
                    .push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                        tx: tx_envelope,
                        evm,
                        result: execution_result,
                        state: &new_state,
                        cumulative_gas_used: 0,
                    }));

                // Commit the changes to the database
                let db = evm.db_mut();
                db.commit(new_state);
                Ok(0)
            } else {
                // Handle regular transactions using the inner executor
                self.inner.execute_transaction_with_result_closure(tx, f)
            }
        }

        fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<Receipt>), BlockExecutionError> {
            let (evm, mut block_res) = self.inner.finish()?;
            // Combine system receipts with regular transaction receipts
            let total_receipts = [self.system_call_receipts, block_res.receipts].concat();
            block_res.receipts = total_receipts;

            Ok((evm, block_res))
        }

        fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
            self.inner.set_state_hook(hook)
        }

        fn evm_mut(&mut self) -> &mut Self::Evm {
            self.inner.evm_mut()
        }

        fn evm(&self) -> &Self::Evm {
            self.inner.evm()
        }
    }

    impl<'db, DB, E> CustomBlockExecutor<'_, E>
    where
        DB: Database + 'db,
        E: Evm<
            DB = &'db mut State<DB>,
            Tx: FromRecoveredTx<TransactionSigned> + FromTxWithEncoded<TransactionSigned>,
        >,
    {
        /// Creates a system transaction log with the specified event name and parameters
        fn create_system_log(
            &self,
            target: Address,
            topics: Vec<FixedBytes<32>>,
            params: Vec<DynSolValue>,
        ) -> Log {
            let encoded_data = DynSolValue::Tuple(params).abi_encode();
            Log {
                address: target,
                data: LogData::new(topics, encoded_data.into()).unwrap(),
            }
        }

        /// Increments the signer's nonce for system transactions
        fn increment_signer_nonce<T: ExecutableTx<Self>>(
            &mut self,
            tx: &T,
        ) -> Result<(), BlockExecutionError> {
            let evm = self.inner.evm_mut();
            let db = evm.db_mut();
            let signer = tx.signer();
            let state = db.load_cache_account(*signer).unwrap();

            let Some(plain_account) = state.account.as_ref() else {
                tracing::warn!("signer account does not exist");
                return Err(BlockExecutionError::Validation(
                    BlockValidationError::InvalidTx {
                        hash: *tx.tx().hash(),
                        error: Box::new(InvalidTransaction::OverflowPaymentInTransaction),
                    },
                ));
            };

            let mut new_state = alloy_primitives::map::foldhash::HashMap::default();
            let storage = plain_account
                .storage
                .iter()
                .map(|(k, v)| (*k, EvmStorageSlot::new(*v)))
                .collect();

            let mut new_account_info = plain_account.info.clone();
            new_account_info.set_nonce(tx.tx().nonce() + 1);

            new_state.insert(
                *signer,
                Account {
                    info: new_account_info,
                    storage,
                    status: revm::state::AccountStatus::Touched,
                },
            );

            db.commit(new_state);
            Ok(())
        }

        /// Handles system transaction that increases account balance
        fn handle_balance_increment(
            &mut self,
            log: Log,
            balance_increment: system_tx::BalanceIncrement,
        ) -> (PlainAccount, ExecutionResult<<E as Evm>::HaltReason>) {
            let evm = self.inner.evm_mut();

            let db = evm.db_mut();
            let state = db.load_cache_account(balance_increment.target).unwrap();

            // Get the existing account or create a new one if it doesn't exist
            let account_info = if let Some(plain_account) = state.account.as_ref() {
                let mut plain_account = plain_account.clone();
                // Add the incremented amount to the balance
                plain_account.info.balance += balance_increment.amount;
                plain_account
            } else {
                // Create a new account with the incremented balance
                let mut account = PlainAccount::new_empty_with_storage(PlainStorage::default());
                account.info.balance = balance_increment.amount;
                account
            };

            let execution_result = ExecutionResult::Success {
                reason: revm::context::result::SuccessReason::Return,
                gas_used: 0,
                gas_refunded: 0,
                logs: vec![log],
                output: Output::Call(Bytes::new()),
            };

            (account_info, execution_result)
        }

        /// Handles system transaction that decreases account balance
        fn handle_balance_decrement(
            &mut self,
            log: Log,
            tx_hash: &FixedBytes<32>,
            balance_decrement: system_tx::BalanceDecrement,
        ) -> Result<
            Result<
                (PlainAccount, ExecutionResult<<E as Evm>::HaltReason>),
                ExecutionResult<<E as Evm>::HaltReason>,
            >,
            BlockExecutionError,
        > {
            let evm = self.inner.evm_mut();

            let db = evm.db_mut();
            let state = db.load_cache_account(balance_decrement.target).unwrap();

            // Get the existing account or create a new one if it doesn't exist
            // handle a case when an account has never existed (0 balance, no data stored on it)
            // We don't even create a receipt in this case (eth does the same with native txs)
            let Some(plain_account) = state.account.as_ref() else {
                tracing::warn!("account does not exist");
                return Err(BlockExecutionError::Validation(
                    BlockValidationError::InvalidTx {
                        hash: *tx_hash,
                        // todo is there a more appropriate error to use?
                        error: Box::new(InvalidTransaction::OverflowPaymentInTransaction),
                    },
                ));
            };
            let mut new_account_info = plain_account.clone();
            if new_account_info.info.balance < balance_decrement.amount {
                tracing::warn!(?plain_account.info.balance, ?balance_decrement.amount);
                return Ok(Err(ExecutionResult::Revert {
                    gas_used: 0,
                    output: Bytes::new(),
                }));
            }
            // Apply the decrement amount to the balance
            new_account_info.info.balance -= balance_decrement.amount;

            let execution_result = ExecutionResult::Success {
                reason: revm::context::result::SuccessReason::Return,
                gas_used: 0,
                gas_refunded: 0,
                logs: vec![log],
                output: Output::Call(Bytes::new()),
            };

            Ok(Ok((new_account_info, execution_result)))
        }
    }

    /// Block builder for Ethereum.
    #[derive(Debug, Clone)]
    pub struct CustomBlockAssembler<ChainSpec = reth_chainspec::ChainSpec> {
        inner: EthBlockAssembler<ChainSpec>,
    }

    impl<ChainSpec> CustomBlockAssembler<ChainSpec> {
        /// Creates a new [`CustomBlockAssembler`].
        pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
            Self {
                inner: EthBlockAssembler::new(chain_spec),
            }
        }
    }

    impl<F, ChainSpec> BlockAssembler<F> for CustomBlockAssembler<ChainSpec>
    where
        F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = EthBlockExecutionCtx<'a>,
            Transaction = TransactionSigned,
            Receipt = Receipt,
        >,
        ChainSpec: EthChainSpec + EthereumHardforks,
    {
        type Block = Block<TransactionSigned>;

        fn assemble_block(
            &self,
            input: BlockAssemblerInput<'_, '_, F>,
        ) -> Result<Block<TransactionSigned>, BlockExecutionError> {
            self.inner.assemble_block(input)
        }
    }

    /// Ethereum block executor factory.
    #[derive(Debug, Clone, Default)]
    pub struct CustomBlockExecutorFactory {
        inner: EthBlockExecutorFactory<RethReceiptBuilder, Arc<ChainSpec>, MyEthEvmFactory>,
    }

    impl CustomBlockExecutorFactory {
        /// Creates a new [`EthBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
        /// [`ReceiptBuilder`].
        pub const fn new(
            receipt_builder: RethReceiptBuilder,
            spec: Arc<ChainSpec>,
            evm_factory: MyEthEvmFactory,
        ) -> Self {
            Self {
                inner: EthBlockExecutorFactory::new(receipt_builder, spec, evm_factory),
            }
        }

        /// Exposes the receipt builder.
        pub const fn receipt_builder(&self) -> &RethReceiptBuilder {
            self.inner.receipt_builder()
        }

        /// Exposes the chain specification.
        pub const fn spec(&self) -> &Arc<ChainSpec> {
            self.inner.spec()
        }

        /// Exposes the EVM factory.
        pub const fn evm_factory(&self) -> &MyEthEvmFactory {
            self.inner.evm_factory()
        }
    }

    impl BlockExecutorFactory for CustomBlockExecutorFactory
    where
        Self: 'static,
    {
        type EvmFactory = MyEthEvmFactory;
        type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
        type Transaction = TransactionSigned;
        type Receipt = Receipt;

        fn evm_factory(&self) -> &Self::EvmFactory {
            &self.inner.evm_factory()
        }

        fn create_executor<'a, DB, I>(
            &'a self,
            evm: <MyEthEvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
            ctx: Self::ExecutionCtx<'a>,
        ) -> impl BlockExecutorFor<'a, Self, DB, I>
        where
            DB: Database + 'a,
            I: Inspector<<EthEvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
        {
            let receipt_builder = self.inner.receipt_builder();
            CustomBlockExecutor {
                inner: EthBlockExecutor::new(evm, ctx, self.inner.spec(), receipt_builder),
                receipt_builder,
                system_call_receipts: vec![],
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct CustomEvmConfig {
        pub inner: EthEvmConfig<EthEvmFactory>,
        pub executor_factory: CustomBlockExecutorFactory,
        pub assembler: CustomBlockAssembler,
    }

    impl ConfigureEvm for CustomEvmConfig {
        type Primitives = EthPrimitives;
        type Error = Infallible;
        type NextBlockEnvCtx = NextBlockEnvAttributes;
        type BlockExecutorFactory = CustomBlockExecutorFactory;
        type BlockAssembler = CustomBlockAssembler<ChainSpec>;

        fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
            &self.executor_factory
        }

        fn block_assembler(&self) -> &Self::BlockAssembler {
            &self.assembler
        }

        fn evm_env(&self, header: &Header) -> EvmEnv {
            self.inner.evm_env(header)
        }

        fn next_evm_env(
            &self,
            parent: &Header,
            attributes: &NextBlockEnvAttributes,
        ) -> Result<EvmEnv, Self::Error> {
            self.inner.next_evm_env(parent, attributes)
        }

        fn context_for_block<'a>(
            &self,
            block: &'a SealedBlock<alloy_consensus::Block<TransactionSigned>>,
        ) -> EthBlockExecutionCtx<'a> {
            self.inner.context_for_block(block)
        }

        fn context_for_next_block(
            &self,
            parent: &SealedHeader,
            attributes: Self::NextBlockEnvCtx,
        ) -> EthBlockExecutionCtx<'_> {
            self.inner.context_for_next_block(parent, attributes)
        }
    }

    /// Factory producing [`EthEvm`].
    #[derive(Debug, Default, Clone, Copy)]
    #[non_exhaustive]
    pub struct MyEthEvmFactory;

    impl EvmFactory for MyEthEvmFactory {
        type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = EthEvm<DB, I, Self::Precompiles>;
        type Context<DB: Database> = revm::Context<BlockEnv, TxEnv, CfgEnv, DB>;
        type Tx = TxEnv;
        type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
        type HaltReason = HaltReason;
        type Spec = SpecId;
        type Precompiles = PrecompilesMap;

        fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
            let spec_id = input.cfg_env.spec;
            EthEvm::new(
                revm::Context::mainnet()
                    .with_block(input.block_env)
                    .with_cfg(input.cfg_env)
                    .with_db(db)
                    .build_mainnet_with_inspector(NoOpInspector {})
                    .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                        PrecompileSpecId::from_spec_id(spec_id),
                    ))),
                false,
            )
        }

        fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
            &self,
            db: DB,
            input: EvmEnv,
            inspector: I,
        ) -> Self::Evm<DB, I> {
            let spec_id = input.cfg_env.spec;
            EthEvm::new(
                revm::Context::mainnet()
                    .with_block(input.block_env)
                    .with_cfg(input.cfg_env)
                    .with_db(db)
                    .build_mainnet_with_inspector(inspector)
                    .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                        PrecompileSpecId::from_spec_id(spec_id),
                    ))),
                true,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip4844, TxLegacy};
    use alloy_genesis::Genesis;
    use alloy_network::{EthereumWallet, TxSigner};
    use alloy_primitives::{FixedBytes, Signature, TxKind, B256};
    use alloy_rlp::Encodable;
    use alloy_rpc_types::{engine::PayloadAttributes, TransactionInput, TransactionRequest};
    use alloy_signer_local::PrivateKeySigner;
    use eyre::OptionExt;
    use reth::{
        api::FullNodePrimitives,
        builder::{rpc::RethRpcAddOns, FullNode},
        providers::AccountReader,
        rpc::api::eth::helpers::EthTransactions,
    };
    use reth_e2e_test_utils::{setup, wallet::Wallet};
    use reth_network::NetworkEventListenerProvider;
    use reth_transaction_pool::TransactionPool;

    use crate::system_tx::{BalanceDecrement, SystemTransaction};

    use super::*;

    pub(crate) fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        };
        EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
    }

    // todo tx ordering test
    // todo block broadcast test

    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::release_stake(release_stake, signer_b())]
    #[case::release_stake_init_no_balance(release_stake, signer_random())]
    #[case::block_reward(block_reward, signer_b())]
    #[case::block_reward_init_no_balance(block_reward, signer_random())]
    async fn incr_system_txs(
        #[case] system_tx: impl Fn(Address) -> SystemTransaction,
        #[case] signer_b: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        // setup
        let (mut nodes, _tasks, ..) =
            setup::<IrysEthereumNode>(1, custom_chain(), false, eth_payload_attributes).await?;
        let mut node = nodes.pop().ok_or_eyre("no node")?;
        let wallets = Wallet::new(2).wallet_gen();
        let block_producer = EthereumWallet::from(wallets[0].clone());
        let block_producer = block_producer.default_signer();

        let signer_b_balance = get_balance(&node.inner, signer_b.address());
        let block_producer_balance = get_balance(&node.inner, block_producer.address());

        let tx_count = 5;
        let system_tx = system_tx(signer_b.address());
        let pooled_txs = (0..tx_count)
            .map(|nonce| compose_system_tx(nonce, 1, system_tx.clone()))
            .map(|tx| sign_tx(tx, &block_producer))
            .collect::<Vec<_>>();
        let pooled_txs = futures::future::join_all(pooled_txs).await;

        // actoin: submit txs and get produce a new block payload and update fork choice
        let tx_hashes = pooled_txs
            .into_iter()
            .map(|tx| {
                node.inner
                    .pool
                    .add_transaction(reth_transaction_pool::TransactionOrigin::Private, tx)
            })
            .collect::<Vec<_>>();
        let tx_hashes = futures::future::try_join_all(tx_hashes).await?;
        let block_payload = node.new_payload().await?;
        let block_payload_hash = node.submit_payload(block_payload.clone()).await?;
        node.update_forkchoice(block_payload_hash, block_payload_hash)
            .await?;

        // Assert
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();

        // ensure all txs are included in the block
        let block_txs = get_block_txs(block_payload);
        let submitted_tx_hashes: std::collections::HashSet<_> = tx_hashes.into_iter().collect();
        assert_eq!(
            block_txs, submitted_tx_hashes,
            "Not all submitted system transactions were included in the block"
        );

        // assert all txs have a corresponding log
        asserst_topic_present_in_logs(block_execution, system_tx.topic().into(), tx_count);

        // assert balance for signer_b is updated
        let signer_b_balance_after = get_balance(&node.inner, signer_b.address());
        assert_eq!(
            signer_b_balance_after,
            signer_b_balance + U256::from(5u64),
            "signer_b's balance should be increased by 5"
        );

        // assert balance for sginer b remains as is (system txs cost nothing)
        let block_producer_balance_after = get_balance(&node.inner, block_producer.address());
        assert_eq!(
            block_producer_balance_after, block_producer_balance,
            "bolck_producer's balance should not change"
        );
        Ok(())
    }

    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::stake(stake, signer_b())]
    #[case::storage_fees(storage_fees, signer_b())]
    async fn decr_system_txs(
        #[case] system_tx: impl Fn(Address) -> SystemTransaction,
        #[case] signer_b: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        // setup
        let (mut nodes, _tasks, ..) =
            setup::<IrysEthereumNode>(1, custom_chain(), false, eth_payload_attributes).await?;
        let mut node = nodes.pop().ok_or_eyre("no node")?;
        let wallets = Wallet::new(2).wallet_gen();
        let tx_count = 2;

        let block_producer = EthereumWallet::from(wallets[0].clone());
        let block_producer = block_producer.default_signer();

        let signer_b_balance = get_balance(&node.inner, signer_b.address());
        let block_producer_balance = get_balance(&node.inner, block_producer.address());

        let system_tx = system_tx(signer_b.address());
        let pooled_txs = (0..tx_count)
            .map(|nonce| compose_system_tx(nonce, 1, system_tx.clone()))
            .map(|tx| sign_tx(tx, &block_producer))
            .collect::<Vec<_>>();

        let pooled_txs = futures::future::join_all(pooled_txs).await;

        // actoin: submit txs and get produce a new block payload and update fork choice
        let tx_hashes = pooled_txs
            .into_iter()
            .map(|tx| {
                node.inner
                    .pool
                    .add_transaction(reth_transaction_pool::TransactionOrigin::Private, tx)
            })
            .collect::<Vec<_>>();
        let tx_hashes = futures::future::try_join_all(tx_hashes).await?;
        let block_payload = node.new_payload().await?;

        let block_payload_hash = node.submit_payload(block_payload.clone()).await?;
        node.update_forkchoice(block_payload_hash, block_payload_hash)
            .await?;

        // Assert
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();

        // ensure all txs are included in the block
        let block_txs = get_block_txs(block_payload);
        let submitted_tx_hashes: std::collections::HashSet<_> = tx_hashes.into_iter().collect();
        assert_eq!(
            block_txs, submitted_tx_hashes,
            "Not all submitted system transactions were included in the block"
        );

        // assert all txs have a corresponding log
        asserst_topic_present_in_logs(block_execution, system_tx.topic().into(), tx_count);

        // assert balance for signer_b is updated
        let signer_b_balance_after = get_balance(&node.inner, signer_b.address());
        assert_eq!(
            signer_b_balance_after,
            signer_b_balance - U256::from(tx_count),
            "signer_b's balance should be reduced by 5"
        );

        // assert balance for sginer b remains as is (system txs cost nothing)
        let block_producer_balance_after = get_balance(&node.inner, block_producer.address());
        assert_eq!(
            block_producer_balance_after, block_producer_balance,
            "bolck_producer's balance should not change"
        );
        Ok(())
    }

    fn release_stake(address: Address) -> SystemTransaction {
        let system_tx = SystemTransaction::ReleaseStake(system_tx::BalanceIncrement {
            amount: U256::ONE,
            target: address,
        });
        system_tx
    }

    fn block_reward(address: Address) -> SystemTransaction {
        let system_tx = SystemTransaction::BlockReward(system_tx::BalanceIncrement {
            amount: U256::ONE,
            target: address,
        });
        system_tx
    }

    fn stake(address: Address) -> SystemTransaction {
        let system_tx = SystemTransaction::Stake(system_tx::BalanceDecrement {
            amount: U256::ONE,
            target: address,
        });
        system_tx
    }

    fn storage_fees(address: Address) -> SystemTransaction {
        let system_tx = SystemTransaction::StorageFees(system_tx::BalanceDecrement {
            amount: U256::ONE,
            target: address,
        });
        system_tx
    }

    #[rstest::fixture]
    fn signer_b() -> Arc<dyn TxSigner<Signature> + Send + Sync> {
        let wallets = Wallet::new(2).wallet_gen();
        let signer_b = EthereumWallet::from(wallets[1].clone());
        let signer_b = signer_b.default_signer();
        signer_b
    }

    #[rstest::fixture]
    fn signer_random() -> Arc<dyn TxSigner<Signature> + Send + Sync> {
        Arc::new(PrivateKeySigner::random())
    }

    fn get_block_txs(
        block_payload: EthBuiltPayload,
    ) -> std::collections::HashSet<alloy_primitives::FixedBytes<32>> {
        block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect()
    }

    fn asserst_topic_present_in_logs(
        block_execution: reth::providers::ExecutionOutcome,
        storage_fees_topic: [u8; 32],
        desired_repetitions: u64,
    ) {
        let receipts = &block_execution.receipts;
        let mut storage_fees_receipt_count = 0;
        for block_receipt in receipts {
            for receipt in block_receipt {
                if receipt.logs.iter().any(|log| {
                    log.data
                        .topics()
                        .iter()
                        .any(|topic| topic == &storage_fees_topic)
                }) {
                    storage_fees_receipt_count += 1;
                }
            }
        }
        assert!(
            storage_fees_receipt_count >= desired_repetitions,
            "Expected at least {desired_repetitions} receipts, found {storage_fees_receipt_count}",
        );
    }

    fn get_balance<N, AddOns>(
        node: &FullNode<N, AddOns>,
        addr: Address,
    ) -> alloy_primitives::Uint<256, 4>
    where
        N: FullNodeComponents<Provider: CanonStateSubscriptions>,
        AddOns: RethRpcAddOns<N, EthApi: EthTransactions>,
        N::Types: NodeTypes<Primitives: FullNodePrimitives>,
    {
        let signer_balance = node
            .provider
            .basic_account(&addr)
            .map(|account_info| account_info.map_or(U256::ZERO, |acc| acc.balance))
            .unwrap_or_else(|err| {
                tracing::warn!("Failed to get signer_b balance: {}", err);
                U256::ZERO
            });
        signer_balance
    }

    async fn sign_tx(
        mut tx_raw: TxLegacy,
        new_signer: &Arc<dyn alloy_network::TxSigner<Signature> + Send + Sync>,
    ) -> EthPooledTransaction<alloy_consensus::EthereumTxEnvelope<TxEip4844>> {
        let signed_tx = new_signer.sign_transaction(&mut tx_raw).await.unwrap();
        let tx = alloy_consensus::EthereumTxEnvelope::Legacy(tx_raw.into_signed(signed_tx))
            .try_into_recovered()
            .unwrap();

        let pooled_tx = EthPooledTransaction::new(tx.clone(), 300);

        return pooled_tx;
    }

    fn custom_chain() -> Arc<ChainSpec> {
        let custom_genesis = r#"
{
  "config": {
    "chainId": 1,
    "homesteadBlock": 0,
    "daoForkSupport": true,
    "eip150Block": 0,
    "eip155Block": 0,
    "eip158Block": 0,
    "byzantiumBlock": 0,
    "constantinopleBlock": 0,
    "petersburgBlock": 0,
    "istanbulBlock": 0,
    "muirGlacierBlock": 0,
    "berlinBlock": 0,
    "londonBlock": 0,
    "arrowGlacierBlock": 0,
    "grayGlacierBlock": 0,
    "shanghaiTime": 0,
    "cancunTime": 0,
    "terminalTotalDifficulty": "0x0",
    "terminalTotalDifficultyPassed": true
  },
  "nonce": "0x0",
  "timestamp": "0x0",
  "extraData": "0x00",
  "gasLimit": "0x1c9c380",
  "difficulty": "0x0",
  "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "coinbase": "0x0000000000000000000000000000000000000000",
  "alloc": {
    "0x14dc79964da2c08b23698b3d3cc7ca32193d9955": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x15d34aaf54267db7d7c367839aaf71a00a2c6a65": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x1cbd3b2770909d4e10f157cabc84c7264073c9ec": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x23618e81e3f5cdf7f54c3d65f7fbc0abf5b21e8f": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x2546bcd3c84621e976d8185a91a922ae77ecec30": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x70997970c51812dc3a010c7d01b50e0d17dc79c8": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x71be63f3384f5fb98995898a86b02fb2426c5788": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x8626f6940e2eb28930efb4cef49b2d1f2c9c1199": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x90f79bf6eb2c4f870365e785982e1f101e93b906": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x976ea74026e726554db657fa54763abd0c3a0aa9": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x9965507d1a55bcc2695c58ba16fb37d819b0a4dc": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0x9c41de96b2088cdc640c6182dfcf5491dc574a57": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xa0ee7a142d267c1f36714e4a8f75612f20a79720": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xbcd4042de499d14e55001ccbb24a551f3b954096": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xbda5747bfd65f08deb54cb465eb87d40e51b197e": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xcd3b766ccdd6ae721141f452c550ca635964ce71": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xdd2fd4581271e230360230f9337d5c0430bf44c0": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xdf3e18d64bc6a983f673ab319ccae4f1a57c7097": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266": {
      "balance": "0xd3c21bcecceda1000000"
    },
    "0xfabb0ac9d68b0b445fb7357272ff202c5651694a": {
      "balance": "0xd3c21bcecceda1000000"
    }
  },
  "number": "0x0"
}
"#;
        let genesis: Genesis = serde_json::from_str(custom_genesis).unwrap();
        Arc::new(genesis.into())
    }
}
