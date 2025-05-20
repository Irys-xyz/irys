use std::{
    marker::PhantomData,
    sync::Arc,
    time::{Duration, SystemTime},
};

use alloy_consensus::{SignableTransaction, TxEip4844, TxLegacy};
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS, Encodable2718};
use alloy_genesis::Genesis;
use alloy_network::{EthereumWallet, NetworkWallet};
use alloy_primitives::{keccak256, Address, Bytes, Log, LogData, Signature, TxKind, B256, U256};
use alloy_rlp::{Decodable, Encodable, Error as RlpError, Result as RlpResult};
use alloy_rlp::{RlpDecodable, RlpDecodableWrapper, RlpEncodable, RlpEncodableWrapper};
use alloy_rpc_types::{engine::PayloadAttributes, TransactionInput, TransactionRequest};
use bytes::BytesMut;
use evm::{CustomBlockAssembler, MyEthEvmFactory};
use futures::StreamExt;
use reth::{
    api::{
        BuiltPayload, FullNodeComponents, FullNodeTypes, NodePrimitives, NodeTypes, PayloadTypes,
    },
    builder::{
        components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder, PoolBuilder},
        BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder, PayloadBuilderConfig,
    },
    payload::{EthBuiltPayload, EthPayloadBuilderAttributes},
    primitives::{EthPrimitives, Recovered},
    providers::{
        providers::ProviderFactoryBuilder, AccountReader, CanonStateSubscriptions, EthStorage,
    },
    transaction_pool::{
        blobstore::InMemoryBlobStore, EthTransactionPool, PoolConfig,
        TransactionValidationTaskExecutor,
    },
};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::EthEvmFactory;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_network::NetworkEventListenerProvider;
use reth_node_ethereum::{
    node::{
        EthereumAddOns, EthereumConsensusBuilder, EthereumExecutorBuilder, EthereumNetworkBuilder,
        EthereumPayloadBuilder,
    },
    EthEngineTypes, EthEvmConfig, EthereumNode,
};
use reth_tracing::{
    tracing::{self, level_filters::LevelFilter},
    LayerInfo, LogFormat, RethTracer, Tracer,
};
use reth_transaction_pool::{
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
    CoinbaseTipOrdering, EthPooledTransaction, EthTransactionValidator, Pool, PoolTransaction,
    Priority, TransactionOrdering, TransactionPool,
};
use reth_trie_db::MerklePatriciaTrie;
use tracing::{debug, info};

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, arbitrary::Arbitrary)]
enum SystemTransaction {
    ReleaseStake(BalanceIncrement),
    BlockReward(BalanceIncrement),
    Stake(BalanceDecrement),
    StorageFees(BalanceDecrement),
}

/// Stable 1-byte discriminants
const RELEASE_STAKE_ID: u8 = 0x00;
const BLOCK_REWARD_ID: u8 = 0x01;
const STAKE_ID: u8 = 0x02;
const STORAGE_FEES_ID: u8 = 0x03;

impl Encodable for SystemTransaction {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            SystemTransaction::ReleaseStake(bi) => {
                out.put_u8(RELEASE_STAKE_ID);
                bi.encode(out);
            }
            SystemTransaction::BlockReward(bi) => {
                out.put_u8(BLOCK_REWARD_ID);
                bi.encode(out);
            }
            SystemTransaction::Stake(bd) => {
                out.put_u8(STAKE_ID);
                bd.encode(out);
            }
            SystemTransaction::StorageFees(bd) => {
                out.put_u8(STORAGE_FEES_ID);
                bd.encode(out);
            }
        }
    }

    fn length(&self) -> usize {
        1 + match self {
            SystemTransaction::ReleaseStake(bi) => bi.length(),
            SystemTransaction::BlockReward(bi) => bi.length(),
            SystemTransaction::Stake(bd) => bd.length(),
            SystemTransaction::StorageFees(bd) => bd.length(),
        }
    }
}

impl Decodable for SystemTransaction {
    fn decode(buf: &mut &[u8]) -> RlpResult<Self> {
        if buf.is_empty() {
            return Err(RlpError::InputTooShort);
        }

        let disc = buf[0];
        *buf = &buf[1..]; // advance past the discriminant byte

        match disc {
            RELEASE_STAKE_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(SystemTransaction::ReleaseStake(inner))
            }
            BLOCK_REWARD_ID => {
                let inner = BalanceIncrement::decode(buf)?;
                Ok(SystemTransaction::BlockReward(inner))
            }
            STAKE_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(SystemTransaction::Stake(inner))
            }
            STORAGE_FEES_ID => {
                let inner = BalanceDecrement::decode(buf)?;
                Ok(SystemTransaction::StorageFees(inner))
            }
            _ => Err(RlpError::Custom("invalid system-transaction discriminant")),
        }
    }
}

impl Default for SystemTransaction {
    fn default() -> Self {
        unimplemented!("relying on the default impl for `SYSTEM_TX` is a critical bug")
    }
}

#[derive(
    serde::Deserialize,
    serde::Serialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    RlpEncodable,
    RlpDecodable,
    arbitrary::Arbitrary,
)]
struct BalanceDecrement {
    amount: U256,
    target: Address,
}

#[derive(
    serde::Deserialize,
    serde::Serialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    RlpEncodable,
    RlpDecodable,
    arbitrary::Arbitrary,
)]
struct BalanceIncrement {
    amount: U256,
    target: Address,
}

// todo: balance increments (releasing stake, block rewads); decerments (staking, commitment, storage fees);
// todo: get rid of json, use encode / decode traits (look at shadows)
// todo: nonce update
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
        let rlp_decoded_system_tx = SystemTransaction::decode(&mut &tx_envelope_input_buf[..]);
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

    use alloy_consensus::{Block, Header, Transaction, TxReceipt};
    use alloy_dyn_abi::{DynSolType, DynSolValue};
    use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
    use alloy_evm::eth::receipt_builder::ReceiptBuilder;
    use alloy_evm::eth::spec::EthExecutorSpec;
    use alloy_evm::eth::EthBlockExecutor;
    use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};
    use alloy_primitives::{keccak256, Bytes, Log, LogData, Uint};
    use reth::primitives::{SealedBlock, SealedHeader};
    use reth::providers::BlockExecutionResult;
    use reth::revm::context::result::ExecutionResult;
    use reth::revm::context::TxEnv;
    use reth::revm::primitives::hardfork::SpecId;
    use reth::revm::{Inspector, State};
    use reth_ethereum_primitives::Receipt;
    use reth_evm::block::{BlockExecutorFactory, BlockExecutorFor, BlockValidationError};
    use reth_evm::eth::receipt_builder::ReceiptBuilderCtx;
    use reth_evm::eth::spec::EthSpec;
    use reth_evm::eth::{EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmContext};
    use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
    use reth_evm::precompiles::PrecompilesMap;
    use reth_evm::{
        ConfigureEvm, EthEvm, EthEvmFactory, EvmEnv, EvmFactory, InspectorFor, IntoTxEnv,
        NextBlockEnvAttributes, TransactionEnv,
    };
    use reth_evm_ethereum::{EthBlockAssembler, RethReceiptBuilder};
    use revm::context::result::{EVMError, HaltReason, InvalidTransaction, Output};
    use revm::context::{BlockEnv, CfgEnv};
    use revm::database::PlainAccount;
    use revm::inspector::NoOpInspector;
    use revm::precompile::{PrecompileSpecId, Precompiles};
    use revm::state::{Account, EvmStorageSlot};
    use revm::{DatabaseCommit, MainBuilder, MainContext};

    use super::*;

    pub struct CustomBlockExecutor<'a, Evm> {
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
            let rlp_decoded_system_tx = SystemTransaction::decode(&mut &tx_envelope_input_buf[..]);
            if let Ok(system_tx) = rlp_decoded_system_tx {
                // handle the signer nonce incerment
                {
                    let evm = self.inner.evm_mut();
                    let db = evm.db_mut();
                    {
                        let signer = tx.signer();
                        let state = db.load_cache_account(*signer).unwrap();
                        let Some(plain_account) = state.account.as_ref() else {
                            tracing::warn!("signer account does not exist");
                            return Err(BlockExecutionError::Validation(
                                BlockValidationError::InvalidTx {
                                    hash: *tx_envelope.hash(),
                                    // todo is there a more appropriate error to use?
                                    error: Box::new(
                                        InvalidTransaction::OverflowPaymentInTransaction,
                                    ),
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
                        new_account_info.set_nonce(tx_envelope.nonce() + 1);

                        new_state.insert(
                            *signer,
                            Account {
                                info: new_account_info,
                                storage,
                                status: revm::state::AccountStatus::Touched,
                            },
                        );
                        db.commit(new_state);
                    }
                }

                let evm = self.inner.evm_mut();
                let db = evm.db_mut();

                match system_tx {
                    SystemTransaction::ReleaseStake(balance_increment) => unimplemented!(),
                    SystemTransaction::BlockReward(balance_increment) => unimplemented!(),
                    SystemTransaction::Stake(balance_decrement) => unimplemented!(),
                    SystemTransaction::StorageFees(balance_decrement) => {
                        let state = db.load_cache_account(balance_decrement.target).unwrap();
                        let hash = *tx_envelope.hash();
                        tracing::error!("special tx {:}", hash);

                        // handle a case when an account has never existed (0 balance, no data stored on it)
                        // We don't even create a receipt in this case (eth does the same with native txs)
                        let Some(plain_account) = state.account.as_ref() else {
                            tracing::warn!("account does not exist");
                            return Err(BlockExecutionError::Validation(
                                BlockValidationError::InvalidTx {
                                    hash: *tx_envelope.hash(),
                                    // todo is there a more appropriate error to use?
                                    error: Box::new(
                                        InvalidTransaction::OverflowPaymentInTransaction,
                                    ),
                                },
                            ));
                        };

                        // handle a case where the balance of the user is too low
                        let mut new_state = alloy_primitives::map::foldhash::HashMap::default();
                        let execution_result;
                        if plain_account.info.balance < balance_decrement.amount {
                            tracing::warn!(?plain_account.info.balance, ?balance_decrement.amount);
                            execution_result = ExecutionResult::Revert {
                                gas_used: 0,
                                output: Bytes::new(),
                            };
                        } else {
                            // Create new account info with updated balance
                            let new_account_balance =
                                plain_account.info.balance - balance_decrement.amount;
                            let storage = plain_account
                                .storage
                                .iter()
                                .map(|(k, v)| (*k, EvmStorageSlot::new(*v)))
                                .collect();
                            let mut new_account_info = plain_account.info.clone();

                            new_account_info.balance = new_account_balance;
                            new_state.insert(
                                balance_decrement.target,
                                Account {
                                    info: new_account_info,
                                    storage,
                                    status: revm::state::AccountStatus::Touched,
                                },
                            );

                            // generate logs to attach to the receipt
                            let param_values = DynSolValue::Tuple(vec![
                                DynSolValue::Uint(balance_decrement.amount, 256),
                                DynSolValue::Address(balance_decrement.target),
                            ]);
                            let encoded_data = param_values.abi_encode();
                            let log = Log {
                                address: balance_decrement.target,
                                data: LogData::new(
                                    vec![keccak256("SYSTEM_TX_DECREMENT_BALANCE")],
                                    encoded_data.into(),
                                )
                                .unwrap(),
                            };
                            execution_result = ExecutionResult::Success {
                                reason: revm::context::result::SuccessReason::Return,
                                gas_used: 0,
                                gas_refunded: 0,
                                logs: vec![log],
                                output: Output::Call(Bytes::new()),
                            }
                        }
                        f(&execution_result);

                        self.system_call_receipts
                            .push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                                tx: tx.tx(),
                                evm,
                                result: execution_result,
                                state: &new_state,
                                // cumulative gas used is 0 because system txs are always executed at the beginning of the block, and use 0 gas
                                cumulative_gas_used: 0,
                            }));

                        // Commit the changes to the database
                        let evm = self.inner.evm_mut();
                        let db = evm.db_mut();
                        // todo test revert
                        db.commit(new_state);
                    }
                }

                Ok(0)
            } else {
                let res = self.inner.execute_transaction_with_result_closure(tx, f);
                dbg!(&res);
                res
            }
        }

        fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<Receipt>), BlockExecutionError> {
            let (evm, mut block_res) = self.inner.finish()?;
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
