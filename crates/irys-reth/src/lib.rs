//! # Irys Reth Node
//!
//! ## System Transactions
//! System transactions are special EVM transactions used to encode protocol-level actions such as:
//! - Block rewards (must go to the Irys block producer)
//! - Storage fee collection (balance decrements)
//! - Stake management (release, stake)
//! - Nonce reset (must always be the last system tx in a block)
//!
//! The CL must validate that:
//! - Block rewards are paid to the correct block producer
//! - Balance increments correspond to rewards
//! - Balance decrements correspond to storage transaction fees
//! - Every block ends with a nonce reset system tx

use std::{marker::PhantomData, sync::Arc, time::SystemTime};

use alloy_consensus::TxLegacy;
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_primitives::{Address, TxKind, U256};
use alloy_rlp::{Decodable, Encodable};
use evm::{IrysBlockAssembler, IrysEvmFactory};
use futures::Stream;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadTypes},
    builder::{
        components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder, PoolBuilder},
        BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder, PayloadBuilderConfig,
    },
    payload::{EthBuiltPayload, EthPayloadBuilderAttributes},
    primitives::{EthPrimitives, InvalidTransactionError, SealedBlock},
    providers::{
        providers::ProviderFactoryBuilder, CanonStateNotification, CanonStateSubscriptions,
        EthStorage, StateProviderFactory,
    },
    transaction_pool::TransactionValidationTaskExecutor,
};
use reth_chainspec::{ChainSpec, ChainSpecProvider, EthChainSpec, EthereumHardforks};
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
use reth_transaction_pool::TransactionValidationOutcome;
use reth_transaction_pool::{
    blobstore::{DiskFileBlobStore, DiskFileBlobStoreConfig},
    EthPoolTransaction, EthPooledTransaction, EthTransactionValidator, Pool, PoolTransaction,
    Priority, TransactionOrdering, TransactionOrigin, TransactionPool, TransactionValidator,
};
use reth_trie_db::MerklePatriciaTrie;
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

/// Type configuration for an Irys-Ethereum node.
#[derive(Debug, Default, Clone, Copy)]
// #[non_exhaustive]
pub struct IrysEthereumNode {
    pub allowed_system_tx_origin: Address,
}

impl NodeTypes for IrysEthereumNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl IrysEthereumNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular Ethereum node.
    pub fn components<Node>(
        &self,
    ) -> ComponentsBuilder<
        Node,
        IrysPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        IrysExecutorBuilder,
        EthereumConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types = IrysEthereumNode>,
        <Node::Types as NodeTypes>::Payload: PayloadTypes<
            BuiltPayload = EthBuiltPayload,
            PayloadAttributes = EthPayloadAttributes,
            PayloadBuilderAttributes = EthPayloadBuilderAttributes,
        >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(IrysPoolBuilder {
                allowed_system_tx_origin: self.allowed_system_tx_origin,
            })
            .executor(IrysExecutorBuilder::default())
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
        IrysPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        IrysExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = EthereumAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
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

/// A custom pool builder for Irys system transaction validation and pool configuration.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct IrysPoolBuilder {
    pub allowed_system_tx_origin: Address,
}

/// Implement the [`PoolBuilder`] trait for the Irys pool builder
///
/// This will be used to build the transaction pool and its maintenance tasks during launch.
///
/// Original code from:
/// https://github.com/Irys-xyz/reth-irys/blob/67abdf25dda69a660d44040d4493421b93d8de7b/crates/ethereum/node/src/node.rs?plain=1#L322
///
/// Notable changes from the original: we evict system txs on every block and frokchoice. They would be deemed stale.
/// A system tx can only live for a single block.
impl<Node> PoolBuilder<Node> for IrysPoolBuilder
where
    Node: FullNodeTypes<Types = IrysEthereumNode>,
{
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            IrysSystemTxValidator<Node::Provider, EthPooledTransaction>,
        >,
        SystemTxPriorityOrdering<EthPooledTransaction>,
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
        let validator = TransactionValidationTaskExecutor {
            validator: IrysSystemTxValidator {
                inner: validator.validator,
                allowed_system_tx_origin: self.allowed_system_tx_origin,
            },
            to_validation_task: validator.to_validation_task,
        };

        let ordering = SystemTxPriorityOrdering::default();
        let transaction_pool =
            reth_transaction_pool::Pool::new(validator, ordering, blob_store, pool_config);
        info!(target: "reth::cli", "Transaction pool initialized");

        // spawn txpool maintenance task
        {
            let pool = transaction_pool.clone();
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
                    client.clone(),
                    pool.clone(),
                    ctx.provider().canonical_state_stream(),
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

            // spawn system txs maintenance task
            ctx.task_executor().spawn_critical(
                "txpool system tx maintenance task",
                maintain_system_txs::<Node, _>(
                    pool.clone(),
                    ctx.provider().canonical_state_stream(),
                ),
            );

            debug!(target: "reth::cli", "Spawned txpool maintenance task");
        }

        Ok(transaction_pool)
    }
}

pub async fn maintain_system_txs<Node, St>(
    pool: Pool<
        TransactionValidationTaskExecutor<
            IrysSystemTxValidator<Node::Provider, EthPooledTransaction>,
        >,
        SystemTxPriorityOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >,
    mut events: St,
) where
    Node: FullNodeTypes<Types = IrysEthereumNode>,
    St: Stream<Item = CanonStateNotification<EthPrimitives>> + Send + Unpin + 'static,
{
    use futures::StreamExt;
    loop {
        let event = events.next().await;
        let Some(event) = event else {
            break;
        };
        match event {
            CanonStateNotification::Commit { .. } => {
                // Get the new block's number and parent hash
                let stale_system_txs = pool
                    .all_transactions()
                    .all()
                    .filter_map(|tx| {
                        use alloy_consensus::transaction::Transaction;
                        let input = tx.inner().input();
                        let Ok(_system_tx) = SystemTransaction::decode(&mut &input[..]) else {
                            return None;
                        };

                        return Some(*tx.hash());
                    })
                    .collect::<Vec<_>>();
                if stale_system_txs.is_empty() {
                    continue;
                }

                tracing::warn!(?stale_system_txs, "dropping stale system transactions");
                pool.remove_transactions(stale_system_txs);
            }
            CanonStateNotification::Reorg { .. } => {
                let stale_system_txs = pool
                    .all_transactions()
                    .all()
                    .filter_map(|tx| {
                        use alloy_consensus::transaction::Transaction;
                        let input = tx.inner().input();
                        let Ok(_system_tx) = SystemTransaction::decode(&mut &input[..]) else {
                            return None;
                        };

                        return Some(*tx.hash());
                    })
                    .collect::<Vec<_>>();
                if stale_system_txs.is_empty() {
                    continue;
                }

                tracing::warn!(?stale_system_txs, "dropping stale system transactions");
                pool.remove_transactions(stale_system_txs);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct IrysSystemTxValidator<Client, T> {
    allowed_system_tx_origin: Address,
    /// The type that performs the actual validation.
    inner: EthTransactionValidator<Client, T>,
}

impl<Client, Tx> TransactionValidator for IrysSystemTxValidator<Client, Tx>
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
        // Try to decode as a system transaction
        let input = transaction.input();
        let Ok(_system_tx) = SystemTransaction::decode(&mut &input[..]) else {
            tracing::trace!(hash = ?transaction.hash(), "non system tx, passing to eth validator");
            return self.inner.validate_one(origin, transaction);
        };

        if transaction.sender() != self.allowed_system_tx_origin {
            tracing::warn!(
                sender = ?transaction.sender(),
                allowed_system_tx_origin = ?self.allowed_system_tx_origin,
                "got system tx that was not signed by the allowed origin");
            return TransactionValidationOutcome::Invalid(
                transaction,
                reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::SignerAccountHasBytecode,
                ),
            );
        }

        if !matches!(origin, TransactionOrigin::Private) {
            tracing::warn!(received_origin = ?origin, "system txs can only be generated via private origin");
            return TransactionValidationOutcome::Invalid(
                transaction,
                reth_transaction_pool::error::InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::SignerAccountHasBytecode,
                ),
            );
        }

        return self.inner.validate_one(origin, transaction);
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        self.inner.validate_all(transactions)
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: reth_primitives_traits::Block,
    {
        self.inner.on_new_head_block(new_tip_block)
    }
}

/// System txs go to the top
/// The transactions are ordered by their coinbase tip.
/// The higher the coinbase tip is, the higher the priority of the transaction.
#[derive(Debug)]
#[non_exhaustive]
pub struct SystemTxPriorityOrdering<T>(PhantomData<T>);

impl<T> TransactionOrdering for SystemTxPriorityOrdering<T>
where
    T: PoolTransaction + 'static,
{
    type PriorityValue = U256;
    type Transaction = T;

    /// Source: <https://github.com/ethereum/go-ethereum/blob/7f756dc1185d7f1eeeacb1d12341606b7135f9ea/core/txpool/legacypool/list.go#L469-L482>.
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

impl<T> Default for SystemTxPriorityOrdering<T> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<T> Clone for SystemTxPriorityOrdering<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// A regular ethereum evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
pub struct IrysExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for IrysExecutorBuilder
where
    Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = evm::IrysEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = EthEvmConfig::new(ctx.chain_spec())
            .with_extra_data(ctx.payload_builder_config().extra_data_bytes());
        let spec = ctx.chain_spec();
        let evm_factory = IrysEvmFactory::default();
        let evm_config = evm::IrysEvmConfig {
            inner: evm_config,
            assembler: IrysBlockAssembler::new(ctx.chain_spec()),
            executor_factory: evm::IrysBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                spec,
                evm_factory,
            ),
        };
        Ok(evm_config)
    }
}

pub mod evm {

    use std::convert::Infallible;

    use alloy_consensus::{Block, Header, Transaction};
    use alloy_dyn_abi::DynSolValue;
    use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
    use alloy_evm::eth::receipt_builder::ReceiptBuilder;

    use alloy_evm::eth::EthBlockExecutor;
    use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};

    use alloy_primitives::{Bytes, FixedBytes, Log, LogData};
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
    use revm::Database as _;
    use revm::{DatabaseCommit, MainBuilder, MainContext};
    use tracing::error_span;

    use super::*;

    /// Irys block executor: handles execution of both regular and system transactions, enforcing protocol rules.
    pub(crate) struct IrysBlockExecutor<'a, Evm> {
        receipt_builder: &'a RethReceiptBuilder,
        system_tx_receipts: Vec<Receipt>,
        pub inner: EthBlockExecutor<'a, Evm, &'a Arc<ChainSpec>, &'a RethReceiptBuilder>,
    }

    impl<'db, DB, E> BlockExecutor for IrysBlockExecutor<'_, E>
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

        //NOTE: whenever we execute system transactions, reth gives a warning: "State root task returned incorrect state root"
        // Current hypothesis is: because we require direct access to the db to execute system txs,
        // reth cannot do parallel state root computations (which presumably are faster than non-parallel).
        // This does not change the end-result of the block but is somtehing we may want to look into.
        fn execute_transaction_with_result_closure(
            &mut self,
            tx: impl ExecutableTx<Self>,
            f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>),
        ) -> Result<u64, BlockExecutionError> {
            let tx_envelope = tx.tx();
            let tx_envelope_input_buf = tx_envelope.input();
            let rlp_decoded_system_tx = SystemTransaction::decode(&mut &tx_envelope_input_buf[..]);

            if let Ok(system_tx) = rlp_decoded_system_tx {
                // Validate system tx metadata
                let block_number = self.inner.evm().block().number;
                let block_hash = self
                    .inner
                    .evm_mut()
                    .db_mut()
                    .block_hash(block_number.saturating_sub(1))
                    .map_err(|_err| {
                        BlockExecutionError::Internal(
                            reth_evm::block::InternalBlockExecutionError::msg(
                                "could not retrieve block by this hash",
                            ),
                        )
                    })?;
                let span = error_span!(
                    "system_tx_processing",
                    "parent_block_hash" = block_hash.to_string(),
                    "block_number" = block_number,
                    "allowed_parent_block_hash" = system_tx.parent_blockhash.to_string(),
                    "allowed_block_height" = system_tx.valid_for_block_height
                );
                let guard = span.enter();

                // ensure that parent block hashes match.
                // This check ensures that a system tx does not get executed for an off-case fork of the desired chain.
                if system_tx.parent_blockhash != block_hash {
                    tracing::error!("A system tx leaked into a block that was not approved by the system tx producer");
                    return Err(BlockExecutionError::Validation(
                        BlockValidationError::InvalidTx {
                            hash: *tx_envelope.hash(),
                            error: Box::new(InvalidTransaction::PriorityFeeGreaterThanMaxFee),
                        },
                    ));
                }

                // ensure that block heights match.
                // This ensures that the system tx does not leak into future blocks.
                if system_tx.valid_for_block_height != block_number {
                    tracing::error!("A system tx leaked into a block that was not approved by the system tx producer");
                    return Err(BlockExecutionError::Validation(
                        BlockValidationError::InvalidTx {
                            hash: *tx_envelope.hash(),
                            error: Box::new(InvalidTransaction::PriorityFeeGreaterThanMaxFee),
                        },
                    ));
                }
                drop(guard);

                // Handle the signer nonce increment
                let mut new_state = self.adjust_signer_nonce(&tx, |nonce| nonce + 1)?;

                // Process different system transaction types
                let topic = system_tx.inner.topic();
                let target;
                let new_account_state = match system_tx.inner {
                    system_tx::TransactionPacket::ReleaseStake(balance_increment) => {
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
                    system_tx::TransactionPacket::BlockReward(balance_increment) => {
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
                    system_tx::TransactionPacket::Stake(balance_decrement) => {
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
                    system_tx::TransactionPacket::StorageFees(balance_decrement) => {
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
                    system_tx::TransactionPacket::ResetSystemTxNonce(reset_system_tx_nonce) => {
                        // in this arm we update the nonce of the signer and do an early return.
                        let state = self.adjust_signer_nonce(&tx, |nonce| {
                            nonce.saturating_sub(reset_system_tx_nonce.decrement_nonce_by)
                        })?;
                        let execution_result = ExecutionResult::Success {
                            reason: revm::context::result::SuccessReason::Return,
                            gas_used: 0,
                            gas_refunded: 0,
                            logs: vec![],
                            output: Output::Call([].into()),
                        };
                        f(&execution_result);
                        self.system_tx_receipts
                            .push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                                tx: tx_envelope,
                                evm: self.inner.evm(),
                                result: execution_result,
                                state: &state,
                                cumulative_gas_used: 0,
                            }));

                        return Ok(0);
                    }
                };

                // at this point, the system tx has been processed, and it was valid *enough*
                // that we should generate a receipt for it even in a failure state
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

                        execution_result
                    }
                    Err(execution_result) => execution_result,
                };

                f(&execution_result);

                // Build and store the receipt
                let evm = self.inner.evm_mut();
                self.system_tx_receipts
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
            let total_receipts = [self.system_tx_receipts, block_res.receipts].concat();
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

    impl<'db, DB, E> IrysBlockExecutor<'_, E>
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
        fn adjust_signer_nonce<T: ExecutableTx<Self>>(
            &mut self,
            tx: &T,
            action: impl Fn(u64) -> u64,
        ) -> Result<alloy_primitives::map::foldhash::HashMap<Address, Account>, BlockExecutionError>
        {
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
            new_account_info.set_nonce(action(tx.tx().nonce()));

            new_state.insert(
                *signer,
                Account {
                    info: new_account_info,
                    storage,
                    status: revm::state::AccountStatus::Touched,
                },
            );

            db.commit(new_state.clone());
            Ok(new_state)
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

    /// Irys block assembler: assembles blocks, ensuring system tx ordering and inclusion rules.
    #[derive(Debug, Clone)]
    pub struct IrysBlockAssembler<ChainSpec = reth_chainspec::ChainSpec> {
        inner: EthBlockAssembler<ChainSpec>,
    }

    impl<ChainSpec> IrysBlockAssembler<ChainSpec> {
        /// Creates a new [`IrysBlockAssembler`].
        pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
            Self {
                inner: EthBlockAssembler::new(chain_spec),
            }
        }
    }

    impl<F, ChainSpec> BlockAssembler<F> for IrysBlockAssembler<ChainSpec>
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

    /// Irys block executor factory: produces block executors.
    #[derive(Debug, Clone, Default)]
    pub struct IrysBlockExecutorFactory {
        inner: EthBlockExecutorFactory<RethReceiptBuilder, Arc<ChainSpec>, IrysEvmFactory>,
    }

    impl IrysBlockExecutorFactory {
        /// Creates a new [`EthBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
        /// [`ReceiptBuilder`].
        pub const fn new(
            receipt_builder: RethReceiptBuilder,
            spec: Arc<ChainSpec>,
            evm_factory: IrysEvmFactory,
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
        pub const fn evm_factory(&self) -> &IrysEvmFactory {
            self.inner.evm_factory()
        }
    }

    impl BlockExecutorFactory for IrysBlockExecutorFactory
    where
        Self: 'static,
    {
        type EvmFactory = IrysEvmFactory;
        type ExecutionCtx<'a> = EthBlockExecutionCtx<'a>;
        type Transaction = TransactionSigned;
        type Receipt = Receipt;

        fn evm_factory(&self) -> &Self::EvmFactory {
            &self.inner.evm_factory()
        }

        fn create_executor<'a, DB, I>(
            &'a self,
            evm: <IrysEvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
            ctx: Self::ExecutionCtx<'a>,
        ) -> impl BlockExecutorFor<'a, Self, DB, I>
        where
            DB: Database + 'a,
            I: Inspector<<EthEvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
        {
            let receipt_builder = self.inner.receipt_builder();
            IrysBlockExecutor {
                inner: EthBlockExecutor::new(evm, ctx, self.inner.spec(), receipt_builder),
                receipt_builder,
                system_tx_receipts: vec![],
            }
        }
    }

    /// Irys EVM config: wraps EVM config, block executor factory, and assembler.
    #[derive(Debug, Clone)]
    pub struct IrysEvmConfig {
        pub inner: EthEvmConfig<EthEvmFactory>,
        pub executor_factory: IrysBlockExecutorFactory,
        pub assembler: IrysBlockAssembler,
    }

    impl ConfigureEvm for IrysEvmConfig {
        type Primitives = EthPrimitives;
        type Error = Infallible;
        type NextBlockEnvCtx = NextBlockEnvAttributes;
        type BlockExecutorFactory = IrysBlockExecutorFactory;
        type BlockAssembler = IrysBlockAssembler<ChainSpec>;

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
    pub struct IrysEvmFactory;

    impl EvmFactory for IrysEvmFactory {
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
    use super::*;
    use crate::system_tx::{
        BalanceDecrement, SystemTransaction, TransactionPacket, BLOCK_REWARD_ID, RELEASE_STAKE_ID,
    };
    use crate::test_utils::*;
    use crate::test_utils::{
        advance_blocks, block_reward, custom_chain, eth_payload_attributes,
        eth_payload_attributes_with_parent, get_balance, nonce_reset, release_stake,
        setup_irys_reth, sign_tx, stake, storage_fees,
    };
    use alloy_consensus::{EthereumTxEnvelope, SignableTransaction, TxEip4844, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_network::{EthereumWallet, TxSigner};
    use alloy_primitives::{Address, Uint, B256};
    use alloy_primitives::{FixedBytes, Signature, TxKind};
    use alloy_rpc_types_engine::ForkchoiceState;
    use alloy_signer_local::PrivateKeySigner;
    use eyre::OptionExt;
    use reth::api::EngineApiMessageVersion;
    use reth::builder::{NodeBuilder, NodeHandle};
    use reth::providers::providers::BlockchainProvider;
    use reth::tasks::TaskManager;
    use reth::{
        providers::{AccountReader, BlockHashReader, BlockNumReader},
        rpc::server_types::eth::EthApiError,
    };
    use reth_e2e_test_utils::{transaction::TransactionTestContext, wallet::Wallet};
    use reth_node_ethereum::EthereumNode;
    use reth_transaction_pool::TransactionPool;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Ensures that only the allowed system tx origin can submit system transactions.
    ///
    /// Steps:
    /// - Setup: Use `setup_irys_reth` to launch node, `block_reward` to compose system tx, `sign_tx` to sign.
    /// - Action: Inject system tx with invalid origin.
    /// - Assertion: Tx is rejected with pool error.
    #[test_log::test(tokio::test)]
    async fn external_users_cannot_submit_system_txs() -> eyre::Result<()> {
        // setup
        let ctx = TestContext::new().await?;
        let (node, ctx) = ctx.get_single_node()?;

        let system_tx = block_reward(ctx.block_producer_a.address(), 1, ctx.genesis_blockhash);
        let mut system_tx_raw = compose_system_tx(0, 1, system_tx);
        let signed_tx = ctx
            .target_account
            .sign_transaction(&mut system_tx_raw)
            .await
            .unwrap();
        let tx = EthereumTxEnvelope::<TxEip4844>::Legacy(system_tx_raw.into_signed(signed_tx))
            .encoded_2718()
            .into();

        let tx_res = node.rpc.inject_tx(tx).await;
        assert!(matches!(tx_res, Err(EthApiError::PoolError(_))));
        Ok(())
    }

    /// Ensures that stale system transactions are dropped from the pool after a commit or reorg event.
    ///
    /// Setup:
    /// - Create two nodes with the same block producer.
    /// - Submit a normal tx and a system tx to the nodes.
    ///
    /// Action:
    /// - Advance the block on the first node.
    /// - Update forkchoice on the second node.
    ///
    /// Assertion:
    /// - The system tx from second node is dropped from both pools and not included in the block.
    #[test_log::test(tokio::test)]
    async fn stale_system_txs_get_dropped() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let ((mut first_node, mut second_node), ctx) = ctx.get_two_nodes()?;

        // Submit normal transaction to first node
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut first_node,
            0,
            U256::from(1000),
            1_000_000_000u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Submit system transaction to second node
        let system_tx = create_system_tx(
            BLOCK_REWARD_ID,
            ctx.block_producer_b.address(),
            1,
            ctx.genesis_blockhash,
        );
        let system_tx_hash =
            create_and_submit_system_tx(&mut second_node, system_tx, 0, &ctx.block_producer_b)
                .await?;

        let payload = first_node.advance_block().await?;
        first_node
            .assert_new_block(
                normal_tx_hash,
                payload.block().hash(),
                payload.block().number,
            )
            .await?;

        // Update forkchoice on second node
        second_node
            .update_forkchoice(payload.block().hash(), payload.block().hash())
            .await?;
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Assert system tx is dropped from both pools
        let pool_txs: Vec<_> = second_node
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !pool_txs.contains(&system_tx_hash),
            "System tx should have been dropped from second node pool"
        );

        let pool_txs: Vec<_> = first_node
            .inner
            .pool
            .all_transactions()
            .all()
            .map(|tx| *tx.hash())
            .collect();
        assert!(
            !pool_txs.contains(&system_tx_hash),
            "System tx should have been dropped from first node pool"
        );

        assert_txs_not_in_block(
            &payload,
            &[system_tx_hash],
            "System tx should not be in block",
        );
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn block_gets_broadcasted_between_peers() -> eyre::Result<()> {
        let wallets = Wallet::new(2).wallet_gen();
        let trget_account = EthereumWallet::from(wallets[0].clone());
        let target_account = trget_account.default_signer();
        let block_producer = EthereumWallet::from(wallets[1].clone());
        let block_producer = block_producer.default_signer();

        let (mut nodes, _tasks, ..) = setup_irys_reth(
            &[block_producer.address(), target_account.address()],
            custom_chain(),
            false,
            eth_payload_attributes,
        )
        .await?;
        let mut second_node = nodes.pop().unwrap();
        let mut first_node = nodes.pop().unwrap();
        let genesis_blockhash = first_node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .block_hash(0)
            .unwrap()
            .unwrap();

        let initial_balance = get_balance(&first_node.inner, block_producer.address());

        let system_tx = SystemTransaction {
            inner: TransactionPacket::BlockReward(system_tx::BalanceIncrement {
                amount: U256::from(7000000000000000000u64),
                target: block_producer.address(),
            }),
            valid_for_block_height: 1,
            parent_blockhash: genesis_blockhash,
        };
        let system_tx_raw = compose_system_tx(0, 1, system_tx.clone());
        let system_pooled_tx = sign_tx(system_tx_raw, &block_producer).await;
        let system_tx_hash = first_node
            .inner
            .pool
            .add_transaction(
                reth_transaction_pool::TransactionOrigin::Private,
                system_pooled_tx,
            )
            .await?;

        // make the node advance
        let payload = first_node.advance_block().await?;

        let block_hash = payload.block().hash();
        let block_number = payload.block().number;

        // assert the block has been committed to the blockchain
        first_node
            .assert_new_block(system_tx_hash, block_hash, block_number)
            .await?;

        // only send forkchoice update to second node
        second_node
            .update_forkchoice(block_hash, block_hash)
            .await?;

        // expect second node advanced via p2p gossip
        second_node
            .assert_new_block(system_tx_hash, block_hash, 1)
            .await?;

        let final_balance = get_balance(&first_node.inner, block_producer.address());
        let final_balance_second = get_balance(&second_node.inner, block_producer.address());
        assert_eq!(final_balance, final_balance_second);
        assert_eq!(
            final_balance,
            initial_balance + U256::from(7000000000000000000u64)
        );

        Ok(())
    }

    // assert that "incrementing" system txs update account state
    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::release_stake(release_stake, signer_b())]
    #[case::release_stake_init_no_balance(release_stake, signer_random())]
    #[case::block_reward(block_reward, signer_b())]
    #[case::block_reward_init_no_balance(block_reward, signer_random())]
    async fn incr_system_txs(
        #[case] system_tx: impl Fn(Address, u64, FixedBytes<32>) -> SystemTransaction,
        #[case] target_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let initial_balance = get_balance(&node.inner, target_signer.address());
        let initial_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        let tx_count = 5;
        let system_tx = system_tx(target_signer.address(), 1, ctx.genesis_blockhash);
        let tx_hashes = create_and_submit_multiple_system_txs(
            &mut node,
            system_tx.clone(),
            tx_count,
            0,
            &ctx.block_producer_a,
        )
        .await?;

        let _block_payload = mine_block_and_validate(&mut node, &tx_hashes, &[]).await?;

        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();
        asserst_topic_present_in_logs(block_execution, system_tx.inner.topic().into(), tx_count);

        assert_balance_change(
            &node,
            target_signer.address(),
            initial_balance,
            U256::from(tx_count),
            true,
            "Target balance should increase",
        );
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_producer_balance,
            U256::ZERO,
            true,
            "Producer balance should not change",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            tx_count,
            "Producer nonce should match tx count",
        );

        Ok(())
    }

    // check if the "decrementing" system txs update account state
    #[test_log::test(tokio::test)]
    #[rstest::rstest]
    #[case::stake(stake, signer_b())]
    #[case::storage_fees(storage_fees, signer_b())]
    async fn decr_system_txs(
        #[case] system_tx: impl Fn(Address, u64, FixedBytes<32>) -> SystemTransaction,
        #[case] target_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let initial_balance = get_balance(&node.inner, target_signer.address());
        let initial_producer_balance = get_balance(&node.inner, ctx.block_producer_a.address());

        let tx_count = 2;
        let system_tx = system_tx(target_signer.address(), 1, ctx.genesis_blockhash);
        let tx_hashes = create_and_submit_multiple_system_txs(
            &mut node,
            system_tx.clone(),
            tx_count,
            0,
            &ctx.block_producer_a,
        )
        .await?;

        // Use new_payload and submit_payload for decrementing transactions
        let block_payload = node.new_payload().await?;
        let block_payload_hash = node.submit_payload(block_payload.clone()).await?;
        node.update_forkchoice(block_payload_hash, block_payload_hash)
            .await?;

        // Assertions
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();

        // Ensure all txs are included in the block
        assert_txs_in_block(&block_payload, &tx_hashes, "System transactions");

        // Assert all txs have a corresponding log
        asserst_topic_present_in_logs(block_execution, system_tx.inner.topic().into(), tx_count);

        // Assert balance for target is decremented
        assert_balance_change(
            &node,
            target_signer.address(),
            initial_balance,
            U256::from(tx_count),
            false, // is_decrement
            "Target balance should be reduced",
        );

        // Assert balance for producer remains the same (system txs cost nothing)
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_producer_balance,
            U256::ZERO,
            true,
            "Producer balance should not change",
        );

        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            tx_count,
            "Producer nonce should match tx count",
        );

        Ok(())
    }

    // expect that system txs get executed first, no matter what. Normal txs get executed only afterwards
    #[test_log::test(tokio::test)]
    async fn test_system_tx_ordering() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create normal transactions with high gas price
        let normal_tx_hashes = create_and_submit_multiple_normal_txs(
            &mut node,
            3,
            0,
            10_000_000_000u128, // High gas price
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Create system transactions with lower effective priority
        let system_tx = create_system_tx(
            RELEASE_STAKE_ID,
            ctx.target_account.address(),
            1,
            ctx.genesis_blockhash,
        );
        let system_tx_hashes = create_and_submit_multiple_system_txs(
            &mut node,
            system_tx,
            2,
            0,
            &ctx.block_producer_a,
        )
        .await?;

        let block_payload = node.advance_block().await?;

        assert_txs_in_block(&block_payload, &system_tx_hashes, "System transactions");
        assert_txs_in_block(&block_payload, &normal_tx_hashes, "Normal transactions");
        assert_system_txs_before_normal_txs(&block_payload, &system_tx_hashes, &normal_tx_hashes);

        Ok(())
    }

    // test decrementing when account does not exist (expect that even receipt not created)
    #[test_log::test(tokio::test)]
    async fn test_decrement_nonexistent_account() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a random address that has never existed on chain
        let nonexistent_address = Address::random();

        // Verify the account doesn't exist
        let account = node
            .inner
            .provider
            .basic_account(&nonexistent_address)
            .unwrap();
        assert!(account.is_none(), "Test account should not exist");

        // Create and submit a system transaction trying to decrement balance of non-existent account
        let system_tx = SystemTransaction {
            inner: TransactionPacket::Stake(BalanceDecrement {
                amount: U256::ONE,
                target: nonexistent_address,
            }),
            valid_for_block_height: 1,
            parent_blockhash: ctx.genesis_blockhash,
        };
        let system_tx_hash =
            create_and_submit_system_tx(&mut node, system_tx, 0, &ctx.block_producer_a).await?;

        // Submit a normal transaction to ensure block is produced
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::ZERO,
            1_000_000_000u128,
            Address::random(),
            &ctx.normal_signer,
        )
        .await?;

        // Produce a new block
        let block_payload = node.advance_block().await?;

        // Verify the system transaction is NOT included
        assert_txs_not_in_block(
            &block_payload,
            &[system_tx_hash],
            "System transaction for non-existent account should not be included in block",
        );

        // Verify the normal transaction IS included
        assert_txs_in_block(
            &block_payload,
            &[normal_tx_hash],
            "Normal transaction should be included in block",
        );

        Ok(())
    }

    // test decrementing when account exists but not enough balance (expect failed tx receipt)
    #[test_log::test(tokio::test)]
    async fn test_decrement_insufficient_balance() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        let funded_balance = get_balance(&node.inner, ctx.normal_signer.address());

        assert!(
            funded_balance > U256::ZERO,
            "Funded account should have nonzero balance"
        );

        // Create a system tx that tries to decrement more than the balance
        let decrement_amount = funded_balance + U256::ONE;
        let system_tx = SystemTransaction {
            inner: TransactionPacket::Stake(BalanceDecrement {
                amount: decrement_amount,
                target: ctx.normal_signer.address(),
            }),
            valid_for_block_height: 1,
            parent_blockhash: ctx.genesis_blockhash,
        };
        let system_tx_hash =
            create_and_submit_system_tx(&mut node, system_tx, 0, &ctx.block_producer_a).await?;

        // Produce a new block
        let block_payload = node.advance_block().await?;

        // Verify the system transaction IS included
        assert_txs_in_block(
            &block_payload,
            &[system_tx_hash],
            "System transaction should be included in block",
        );

        // Verify the receipt for the system tx is a revert/failure
        let block_execution = node.inner.provider.get_state(0..=1).unwrap().unwrap();
        let receipts = block_execution.receipts;
        let receipt = &receipts[1][0];
        assert!(
            !receipt.success,
            "Expected a revert/failure receipt for system tx with insufficient balance"
        );

        Ok(())
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
    /// Mines 5 blocks, each with a system (block reward) and a normal tx.
    /// Asserts both txs are present in every block.
    /// Verifies sequential block production and tx inclusion.
    /// Expects latest block number to be 5 at the end.
    #[test_log::test(tokio::test)]
    async fn mine_5_blocks_with_system_and_normal_tx() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Get genesis block hash for parent_blockhash
        let mut parent_blockhash = ctx.genesis_blockhash;
        let recipient = ctx.target_account.address();

        for block_number in 1..=5 {
            // Block reward system tx
            let system_tx = block_reward(
                ctx.block_producer_a.address(),
                block_number,
                parent_blockhash,
            );
            let system_tx_hash = create_and_submit_system_tx(
                &mut node,
                system_tx,
                block_number - 1,
                &ctx.block_producer_a,
            )
            .await?;

            // Normal tx
            let normal_tx_hash = create_and_submit_normal_tx(
                &mut node,
                block_number - 1,
                U256::from(1234u64),
                2_000_000_000u128, // 2 Gwei
                recipient,
                &ctx.normal_signer,
            )
            .await?;

            // Mine block
            let block_payload = node.advance_block().await?;
            parent_blockhash = block_payload.block().hash();

            // Assert both txs are present
            assert_txs_in_block(
                &block_payload,
                &[system_tx_hash],
                &format!("System tx not found in block {}", block_number),
            );
            assert_txs_in_block(
                &block_payload,
                &[normal_tx_hash],
                &format!("Normal tx not found in block {}", block_number),
            );
        }

        // Assert that the current block is the latest block
        let latest_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        assert_eq!(latest_block, 5, "Latest block is not 5");

        Ok(())
    }

    /// Submits a system tx with an invalid parent blockhash and a valid normal tx.
    /// Asserts the system tx is rejected (not in block), normal tx is included.
    /// Expects only valid txs to be mined.
    /// Ensures parent blockhash check is enforced for system txs.
    #[test_log::test(tokio::test)]
    async fn system_tx_with_invalid_parent_blockhash_is_rejected_by_custom_executor(
    ) -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Get genesis block hash for parent_blockhash
        let recipient = ctx.target_account.address();

        // Use a random blockhash instead of the real parent
        let invalid_parent_blockhash = FixedBytes::random();

        // Create a system tx with the invalid parent blockhash
        let system_tx = block_reward(ctx.block_producer_a.address(), 1, invalid_parent_blockhash);
        let system_tx_hash =
            create_and_submit_system_tx(&mut node, system_tx, 0, &ctx.block_producer_a).await?;

        // Create and submit a normal user tx
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1234u64),
            2_000_000_000u128, // 2 Gwei
            recipient,
            &ctx.normal_signer,
        )
        .await?;

        // Mine a block
        let block_payload = node.advance_block().await?;

        // Assert that the system tx is NOT present in the block
        assert_txs_not_in_block(
            &block_payload,
            &[system_tx_hash],
            "System tx with invalid parent blockhash should not be included in the block",
        );

        // Assert that the normal tx IS present in the block
        assert_txs_in_block(
            &block_payload,
            &[normal_tx_hash],
            "Normal user tx should be included in the block",
        );

        Ok(())
    }

    /// Submits a system tx with a valid parent blockhash but invalid block number, plus a normal tx.
    /// Asserts the system tx is rejected (not in block), normal tx is included.
    /// Expects only valid txs to be mined.
    /// Ensures block number check is enforced for system txs.
    #[test_log::test(tokio::test)]
    async fn system_tx_with_invalid_block_number_is_rejected_by_custom_executor() -> eyre::Result<()>
    {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Use an invalid block number (should be 1, use 2)
        let invalid_block_number = 2u64;

        // Create a system tx with the valid parent blockhash but invalid block number
        let system_tx = block_reward(
            ctx.block_producer_a.address(),
            invalid_block_number,
            ctx.genesis_blockhash,
        );
        let system_tx_hash =
            create_and_submit_system_tx(&mut node, system_tx, 0, &ctx.block_producer_a).await?;

        // Create and submit a normal user tx
        let normal_tx_hash = create_and_submit_normal_tx(
            &mut node,
            0,
            U256::from(1234u64),
            2_000_000_000u128, // 2 Gwei
            ctx.normal_signer.address(),
            &ctx.normal_signer,
        )
        .await?;

        // Mine a block
        let block_payload = node.advance_block().await?;

        // Assert that the system tx is NOT present in the block
        assert_txs_not_in_block(
            &block_payload,
            &[system_tx_hash],
            "System tx with invalid block number should not be included in the block",
        );

        // Assert that the normal tx IS present in the block
        assert_txs_in_block(
            &block_payload,
            &[normal_tx_hash],
            "Normal user tx should be included in the block",
        );

        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn system_tx_with_invalid_signer_is_rejected_by_pool_validator() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a system tx with a valid block number and parent blockhash, but sign with a random (invalid) signer
        let system_tx = block_reward(ctx.block_producer_a.address(), 1, ctx.genesis_blockhash);
        let invalid_signer = signer_random();
        let res = create_and_submit_system_tx(&mut node, system_tx, 0, &invalid_signer).await;
        assert!(
            res.is_err(),
            "System tx with invalid signer should be rejected by the pool validator"
        );

        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn system_tx_with_invalid_origin_is_rejected_by_pool_validator() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Create a system tx with a valid block number and parent blockhash, signed by the correct signer
        let system_tx = block_reward(ctx.block_producer_a.address(), 1, ctx.genesis_blockhash);
        let system_tx_raw = compose_system_tx(0, 1, system_tx.clone());
        let system_pooled_tx = sign_tx(system_tx_raw, &ctx.block_producer_a).await;
        // Submit with TransactionOrigin::Local (should be rejected, only Private is allowed)
        let res = node
            .inner
            .pool
            .add_transaction(
                reth_transaction_pool::TransactionOrigin::Local,
                system_pooled_tx,
            )
            .await;
        assert!(
            res.is_err(),
            "System tx with invalid origin should be rejected by the pool validator"
        );

        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn rollback_state_revert_on_fork_switch() -> eyre::Result<()> {
        // Setup nodes and context
        let ctx = TestContext::new().await?;
        let ((mut node_a, mut node_b), ctx) = ctx.get_two_nodes()?;
        let reward_address = Address::random();

        // Node A: advance 3 blocks, 2 system txs per block
        let _block_hashes_a =
            advance_blocks(&mut node_a, reward_address, 1, 3, 2, &ctx.block_producer_a).await?;
        let consistent_provider_a = node_a.inner.provider.consistent_provider().unwrap();
        let account_a_on_fork = consistent_provider_a
            .basic_account(&ctx.block_producer_a.address())
            .unwrap()
            .unwrap();
        let node_a_reward_balance = get_balance(&node_a.inner, reward_address);

        // Node B: advance 4 blocks, 1 system tx per block
        let _block_hashes_b =
            advance_blocks(&mut node_b, reward_address, 1, 4, 1, &ctx.block_producer_b).await?;

        // Record Node B's state after 4 blocks
        let consistent_provider_b = node_b.inner.provider.consistent_provider().unwrap();
        let account_b = consistent_provider_b
            .basic_account(&ctx.block_producer_b.address())
            .unwrap()
            .unwrap();
        let best_block_b = consistent_provider_b.best_block_number().unwrap();
        let block_hash_b = consistent_provider_b
            .block_hash(best_block_b)
            .unwrap()
            .unwrap();
        let node_b_reward_balance = get_balance(&node_b.inner, reward_address);

        // Node A switches forkchoice to Node B's latest block
        node_a.sync_to(block_hash_b).await?;

        // Node A's state should match Node B's
        let consistent_provider_a = node_a.inner.provider.consistent_provider().unwrap();
        let account_a = consistent_provider_a
            .basic_account(&ctx.block_producer_a.address())
            .unwrap()
            .unwrap();
        let best_block_a = consistent_provider_a.best_block_number().unwrap();
        let block_hash_a = consistent_provider_a.block_hash(4).unwrap().unwrap();
        let node_a_reward_balance_post_switch = get_balance(&node_a.inner, reward_address);
        assert_ne!(
            ctx.block_producer_a.address(),
            ctx.block_producer_b.address()
        );
        assert_eq!(best_block_b, 4);
        assert_eq!(best_block_a, 4);
        assert_eq!(
            block_hash_a, block_hash_b,
            "block hashes after sync must be equal"
        );
        assert_ne!(
            node_a_reward_balance, node_b_reward_balance,
            "initial rewards differ on forks produced by each chain"
        );
        assert_eq!(
            node_a_reward_balance_post_switch, node_b_reward_balance,
            "post FCU, node a has the same state as node b"
        );
        assert_ne!(
            account_a_on_fork.nonce, account_a.nonce,
            "balances for account a pre fork and post fork should be different!"
        );
        assert_eq!(
            account_b.nonce, 4,
            "Nonces should be different for each block producer account"
        );
        assert_eq!(
            account_a.nonce, 0,
            "Nonce should match after forkchoice switch"
        );
        assert_eq!(
            best_block_a, best_block_b,
            "Canonical block height should match after forkchoice switch"
        );

        Ok(())
    }

    /// Tests state rollback functionality on safe block reorgs.
    ///
    /// This test verifies that when a forkchoice update rolls back to an earlier safe block,
    /// the state is correctly reverted and subsequent blocks can be built on the rolled-back state.
    ///
    /// Test scenario:
    /// 1. Build 4 blocks with block rewards and nonce resets (balance +4, nonce reset to 0 each block)
    /// 2. Verify state after 4 blocks: balance = initial + 4, nonce = 0
    /// 3. Roll back to block 1 via forkchoice update (safe/finalized = block 1)
    /// 4. Build a new fork block (block 2) on top of the rolled-back state
    /// 5. Verify final state: balance = initial + 2, nonce = 1 (reflecting the rollback and new block)
    #[test_log::test(tokio::test)]
    async fn rollback_state_on_safe_blocks() -> eyre::Result<()> {
        // Setup custom payload attributes to control parent block hash
        let parent_tracker = Arc::new(Mutex::new(B256::ZERO));
        let payload_attributes = {
            let parent_tracker = parent_tracker.clone();
            move |timestamp: u64| {
                let parent = *parent_tracker.lock().unwrap();
                eth_payload_attributes_with_parent(timestamp, parent)
            }
        };

        let ctx = TestContext::new_with_payload_attributes(payload_attributes).await?;
        let (mut node, ctx) = ctx.get_single_node()?;

        // Initial setup and baseline measurements
        let mut parent_blockhash = ctx.genesis_blockhash;
        let initial_balance = get_balance(&node.inner, ctx.block_producer_a.address());
        let mut block_hashes = vec![parent_blockhash];

        // Phase 1: Build 4 blocks with system transactions
        tracing::info!("Phase 1: Building 4 blocks with block rewards and nonce resets");
        for block_number in 1..=4 {
            // Create block reward transaction
            let block_reward_tx = block_reward(
                ctx.block_producer_a.address(),
                block_number,
                parent_blockhash,
            );
            create_and_submit_system_tx(&mut node, block_reward_tx, 0, &ctx.block_producer_a)
                .await?;

            // Create nonce reset transaction
            let nonce_reset_tx = nonce_reset(1, block_number, parent_blockhash);
            create_and_submit_system_tx(&mut node, nonce_reset_tx, 1, &ctx.block_producer_a)
                .await?;

            // Mine the block
            let payload = node.advance_block().await?;
            parent_blockhash = payload.block().hash();
            block_hashes.push(parent_blockhash);

            tracing::info!("Built block {}: {}", block_number, parent_blockhash);
        }

        // Phase 2: Verify state after 4 blocks
        tracing::info!("Phase 2: Verifying state after building 4 blocks");
        let best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        assert_eq!(best_block, 4, "Should be at block 4");
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(4),
            true,
            "Balance should reflect 4 block rewards",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            0,
            "Nonce should be 0 after nonce resets",
        );

        // Phase 3: Roll back to block 1 (safe/finalized)
        tracing::info!("Phase 3: Rolling back to block 1 via forkchoice update");
        let rollback_target = block_hashes[1]; // Block 1
        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: rollback_target,
                    safe_block_hash: rollback_target,
                    finalized_block_hash: rollback_target,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for rollback to process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 4: Build new fork block on rolled-back state
        tracing::info!("Phase 4: Building new fork block on rolled-back state");
        let fork_block_number = 2; // Building block 2 on top of block 1
        let fork_reward_tx = block_reward(
            ctx.block_producer_a.address(),
            fork_block_number,
            rollback_target,
        );
        create_and_submit_system_tx(&mut node, fork_reward_tx, 0, &ctx.block_producer_a).await?;

        // Update parent tracker for payload attributes
        *parent_tracker.lock().unwrap() = rollback_target;

        // Build and submit the fork block
        let fork_payload = node.build_and_submit_payload().await?;
        let fork_block_hash = fork_payload.block().hash();

        tracing::info!(
            "Built fork block {}: {}",
            fork_block_number,
            fork_block_hash
        );

        // Phase 5: Finalize the new fork
        tracing::info!("Phase 5: Finalizing the new fork");
        node.inner
            .add_ons_handle
            .beacon_engine_handle
            .fork_choice_updated(
                ForkchoiceState {
                    head_block_hash: fork_block_hash,
                    safe_block_hash: fork_block_hash,
                    finalized_block_hash: fork_block_hash,
                },
                None,
                EngineApiMessageVersion::default(),
            )
            .await?;

        // Allow time for finalization to process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Phase 6: Verify final state after rollback and fork
        tracing::info!("Phase 6: Verifying final state after rollback and fork");
        let final_best_block = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .best_block_number()
            .unwrap();
        let final_best_hash = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .block_hash(final_best_block)
            .unwrap()
            .unwrap();

        // Assertions for final state
        assert_eq!(
            fork_block_hash, final_best_hash,
            "Fork block should be the canonical head"
        );
        assert_eq!(
            final_best_block, fork_block_number,
            "Should be at fork block number"
        );
        assert_balance_change(
            &node,
            ctx.block_producer_a.address(),
            initial_balance,
            U256::from(2), // 1 from block 1 + 1 from fork block 2
            true,
            "Balance should reflect rollback to block 1 + new fork block reward",
        );
        assert_nonce(
            &node,
            ctx.block_producer_a.address(),
            1,
            "Nonce should be 1 after fork block transaction",
        );

        tracing::info!("Rollback test completed successfully");
        Ok(())
    }
}

#[cfg(any(feature = "test-utils", test))]
/// Test Utilities for Irys Reth node
pub mod test_utils {
    use super::*;
    use crate::system_tx::{SystemTransaction, TransactionPacket};
    use alloy_consensus::EthereumTxEnvelope;
    use alloy_consensus::{SignableTransaction, TxEip4844, TxLegacy};
    use alloy_genesis::Genesis;
    use alloy_network::EthereumWallet;
    use alloy_network::TxSigner;
    use alloy_primitives::Address;
    use alloy_primitives::{FixedBytes, Signature, B256};
    use alloy_primitives::{TxKind, U256};
    use alloy_rpc_types::engine::PayloadAttributes;
    use reth::{
        api::{FullNodePrimitives, PayloadAttributesBuilder},
        args::{DiscoveryArgs, NetworkArgs, RpcServerArgs},
        builder::{rpc::RethRpcAddOns, FullNode, NodeBuilder, NodeConfig, NodeHandle},
        providers::{AccountReader, BlockHashReader},
        rpc::api::eth::helpers::EthTransactions,
        tasks::TaskManager,
    };
    use reth_e2e_test_utils::{node::NodeTestContext, wallet::Wallet, NodeHelperType};
    use reth_engine_local::LocalPayloadAttributesBuilder;
    use reth_transaction_pool::TransactionPool;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tracing::{span, Level};

    /// Common setup for tests - creates wallets, nodes, and returns initialized context
    pub struct TestContext {
        pub nodes: Vec<NodeHelperType<IrysEthereumNode>>,
        pub block_producer_a: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub block_producer_b: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub normal_signer: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub target_account: Arc<dyn TxSigner<Signature> + Send + Sync>,
        pub genesis_blockhash: FixedBytes<32>,
        #[allow(dead_code)]
        pub tasks: TaskManager,
    }

    impl std::fmt::Debug for TestContext {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "TestContext")
        }
    }

    impl TestContext {
        pub async fn new() -> eyre::Result<Self> {
            Self::new_with_payload_attributes(eth_payload_attributes).await
        }

        pub async fn new_with_payload_attributes(
            payload_attributes: impl Fn(u64) -> EthPayloadBuilderAttributes
                + Send
                + Sync
                + Clone
                + 'static,
        ) -> eyre::Result<Self> {
            let wallets = Wallet::new(4).wallet_gen();
            let block_producer_a = EthereumWallet::from(wallets[0].clone()).default_signer();
            let block_producer_b = EthereumWallet::from(wallets[1].clone()).default_signer();
            let normal_signer = EthereumWallet::from(wallets[2].clone()).default_signer();
            let target_account = EthereumWallet::from(wallets[3].clone()).default_signer();

            let block_producer_addresses =
                vec![block_producer_a.address(), block_producer_b.address()];
            let (nodes, tasks, ..) = setup_irys_reth(
                &block_producer_addresses,
                custom_chain(),
                false,
                payload_attributes,
            )
            .await?;

            let genesis_blockhash = nodes
                .get(0)
                .unwrap()
                .inner
                .provider
                .consistent_provider()
                .unwrap()
                .block_hash(0)
                .unwrap()
                .unwrap();

            Ok(Self {
                tasks,
                nodes,
                block_producer_a,
                block_producer_b,
                normal_signer,
                target_account,
                genesis_blockhash,
            })
        }

        pub fn get_single_node(mut self) -> eyre::Result<(NodeHelperType<IrysEthereumNode>, Self)> {
            if self.nodes.is_empty() {
                return Err(eyre::eyre!("No nodes available"));
            }
            let node = self.nodes.remove(0);
            Ok((node, self))
        }

        pub fn get_two_nodes(
            mut self,
        ) -> eyre::Result<(
            (
                NodeHelperType<IrysEthereumNode>,
                NodeHelperType<IrysEthereumNode>,
            ),
            Self,
        )> {
            if self.nodes.len() < 2 {
                return Err(eyre::eyre!("Need at least 2 nodes"));
            }
            let second = self.nodes.pop().unwrap();
            let first = self.nodes.pop().unwrap();
            Ok(((first, second), self))
        }
    }

    /// Helper for creating and submitting system transactions
    pub async fn create_and_submit_system_tx(
        node: &mut NodeHelperType<IrysEthereumNode>,
        system_tx: SystemTransaction,
        nonce: u64,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
        let system_tx_raw = compose_system_tx(nonce, 1, system_tx);
        let system_pooled_tx = sign_tx(system_tx_raw, signer).await;
        let tx_hash = node
            .inner
            .pool
            .add_transaction(
                reth_transaction_pool::TransactionOrigin::Private,
                system_pooled_tx,
            )
            .await?;
        Ok(tx_hash)
    }

    /// Helper for creating and submitting multiple system transactions
    pub async fn create_and_submit_multiple_system_txs(
        node: &mut NodeHelperType<IrysEthereumNode>,
        system_tx: SystemTransaction,
        count: u64,
        start_nonce: u64,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<Vec<alloy_primitives::FixedBytes<32>>> {
        let mut tx_hashes = Vec::new();
        for i in 0..count {
            let tx_hash =
                create_and_submit_system_tx(node, system_tx.clone(), start_nonce + i, signer)
                    .await?;
            tx_hashes.push(tx_hash);
        }
        Ok(tx_hashes)
    }

    /// Helper for creating and submitting normal transactions
    pub async fn create_and_submit_normal_tx(
        node: &mut NodeHelperType<IrysEthereumNode>,
        nonce: u64,
        value: U256,
        gas_price: u128,
        recipient: Address,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
        let mut normal_tx_raw = TxLegacy {
            gas_limit: 99000,
            value,
            nonce,
            gas_price,
            chain_id: Some(1),
            input: vec![123].into(),
            to: TxKind::Call(recipient),
            ..Default::default()
        };
        let signed_normal = signer.sign_transaction(&mut normal_tx_raw).await.unwrap();
        let normal_tx =
            EthereumTxEnvelope::<TxEip4844>::Legacy(normal_tx_raw.into_signed(signed_normal))
                .try_into_recovered()
                .unwrap();
        let normal_pooled_tx = EthPooledTransaction::new(normal_tx.clone(), 300);
        let tx_hash = node
            .inner
            .pool
            .add_transaction(
                reth_transaction_pool::TransactionOrigin::Local,
                normal_pooled_tx,
            )
            .await?;
        Ok(tx_hash)
    }

    /// Helper for creating multiple normal transactions
    pub async fn create_and_submit_multiple_normal_txs(
        node: &mut NodeHelperType<IrysEthereumNode>,
        count: u64,
        start_nonce: u64,
        gas_price: u128,
        recipient: Address,
        signer: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> eyre::Result<Vec<alloy_primitives::FixedBytes<32>>> {
        let mut tx_hashes = Vec::new();
        for i in 0..count {
            let tx_hash = create_and_submit_normal_tx(
                node,
                start_nonce + i,
                U256::from(1234u64),
                gas_price,
                recipient,
                signer,
            )
            .await?;
            tx_hashes.push(tx_hash);
        }
        Ok(tx_hashes)
    }

    /// Helper for asserting transaction inclusion in blocks
    pub fn assert_txs_in_block(
        block_payload: &EthBuiltPayload,
        expected_txs: &[alloy_primitives::FixedBytes<32>],
        message: &str,
    ) {
        let block_txs: HashSet<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        for tx_hash in expected_txs {
            assert!(
                block_txs.contains(tx_hash),
                "{}: Transaction {:?} not found in block",
                message,
                tx_hash
            );
        }
    }

    /// Helper for asserting transaction exclusion from blocks
    pub fn assert_txs_not_in_block(
        block_payload: &EthBuiltPayload,
        excluded_txs: &[alloy_primitives::FixedBytes<32>],
        message: &str,
    ) {
        let block_txs: HashSet<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        for tx_hash in excluded_txs {
            assert!(
                !block_txs.contains(tx_hash),
                "{}: Transaction {:?} should not be in block",
                message,
                tx_hash
            );
        }
    }

    /// Helper for asserting transaction ordering in blocks
    pub fn assert_system_txs_before_normal_txs(
        block_payload: &EthBuiltPayload,
        system_txs: &[alloy_primitives::FixedBytes<32>],
        normal_txs: &[alloy_primitives::FixedBytes<32>],
    ) {
        let block_txs: Vec<_> = block_payload
            .block()
            .body()
            .transactions
            .iter()
            .map(|tx| *tx.hash())
            .collect();

        let mut last_system_tx_pos = 0;
        let mut first_normal_tx_pos = block_txs.len();

        for (pos, tx_hash) in block_txs.iter().enumerate() {
            if system_txs.contains(tx_hash) {
                last_system_tx_pos = last_system_tx_pos.max(pos);
            }
            if normal_txs.contains(tx_hash) {
                first_normal_tx_pos = first_normal_tx_pos.min(pos);
            }
        }

        assert!(
            last_system_tx_pos < first_normal_tx_pos,
            "System transactions should appear before normal transactions. Last system: {}, First normal: {}",
            last_system_tx_pos,
            first_normal_tx_pos
        );
    }

    /// Helper for asserting balance changes
    pub fn assert_balance_change(
        node: &NodeHelperType<IrysEthereumNode>,
        address: Address,
        initial_balance: U256,
        expected_change: U256,
        is_increment: bool,
        message: &str,
    ) {
        let final_balance = get_balance(&node.inner, address);
        let expected_balance = if is_increment {
            initial_balance + expected_change
        } else {
            initial_balance - expected_change
        };

        assert_eq!(
            final_balance, expected_balance,
            "{}: Expected balance {}, got {}",
            message, expected_balance, final_balance
        );
    }

    /// Helper for asserting nonce values
    pub fn assert_nonce(
        node: &NodeHelperType<IrysEthereumNode>,
        address: Address,
        expected_nonce: u64,
        message: &str,
    ) {
        let actual_nonce = get_nonce(&node.inner, address);
        assert_eq!(
            actual_nonce, expected_nonce,
            "{}: Expected nonce {}, got {}",
            message, expected_nonce, actual_nonce
        );
    }

    /// Helper for block mining and validation
    pub async fn mine_block_and_validate(
        node: &mut NodeHelperType<IrysEthereumNode>,
        expected_system_txs: &[alloy_primitives::FixedBytes<32>],
        expected_normal_txs: &[alloy_primitives::FixedBytes<32>],
    ) -> eyre::Result<EthBuiltPayload> {
        let block_payload = node.advance_block().await?;

        assert_txs_in_block(&block_payload, expected_system_txs, "System transactions");
        assert_txs_in_block(&block_payload, expected_normal_txs, "Normal transactions");

        if !expected_system_txs.is_empty() && !expected_normal_txs.is_empty() {
            assert_system_txs_before_normal_txs(
                &block_payload,
                expected_system_txs,
                expected_normal_txs,
            );
        }

        Ok(block_payload)
    }

    /// Helper to create system transaction based on type
    pub fn create_system_tx(
        tx_type: u8,
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        use crate::system_tx::*;
        match tx_type {
            BLOCK_REWARD_ID => block_reward(address, valid_for_block_height, parent_blockhash),
            RELEASE_STAKE_ID => release_stake(address, valid_for_block_height, parent_blockhash),
            STAKE_ID => stake(address, valid_for_block_height, parent_blockhash),
            STORAGE_FEES_ID => storage_fees(address, valid_for_block_height, parent_blockhash),
            RESET_SYS_SIGNER_NONCE_ID => nonce_reset(100, valid_for_block_height, parent_blockhash),
            _ => panic!("Unknown system transaction type: {}", tx_type),
        }
    }

    /// Mines multiple blocks, each with a configurable number of system txs.
    /// Returns the hashes of all produced blocks.
    ///
    /// # Arguments
    /// - `node`: The node to mine blocks on.
    /// - `reward_to`: Address to receive block rewards.
    /// - `start_block`: Block number to start mining from.
    /// - `num_blocks`: Number of blocks to mine.
    /// - `sys_txs_per_block`: Number of system txs per block.
    /// - `wallets`: Signer for system txs.
    pub async fn advance_blocks(
        node: &mut NodeHelperType<IrysEthereumNode>,
        reward_to: Address,
        start_block: u64,
        num_blocks: u64,
        sys_txs_per_block: u64,
        wallets: &Arc<dyn TxSigner<Signature> + Send + Sync>,
    ) -> Result<Vec<FixedBytes<32>>, eyre::Error> {
        let mut parent_blockhash = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .block_hash(0)
            .unwrap()
            .unwrap();
        let mut block_hashes = vec![parent_blockhash];
        let mut nonce = 0;
        for block_number in start_block..(start_block + num_blocks) {
            for _tx_idx in 0..sys_txs_per_block {
                let system_tx = block_reward(reward_to, block_number, parent_blockhash);
                let system_tx_raw = compose_system_tx(nonce, 1, system_tx.clone());
                nonce += 1;
                let system_pooled_tx = sign_tx(system_tx_raw, wallets).await;
                node.inner
                    .pool
                    .add_transaction(
                        reth_transaction_pool::TransactionOrigin::Private,
                        system_pooled_tx,
                    )
                    .await?;
            }
            let payload = node.advance_block().await?;
            parent_blockhash = payload.block().hash();
            block_hashes.push(parent_blockhash);
        }
        Ok::<_, eyre::Error>(block_hashes)
    }

    /// Compose a system tx for releasing stake.
    pub fn release_stake(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::ReleaseStake(system_tx::BalanceIncrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for block reward.
    pub fn block_reward(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::BlockReward(system_tx::BalanceIncrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for resetting the system tx nonce.
    pub fn nonce_reset(
        decrement_nonce_by: u64,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::ResetSystemTxNonce(system_tx::ResetSystemTxNonce {
                decrement_nonce_by,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for staking.
    pub fn stake(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::Stake(system_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Compose a system tx for storage fees.
    pub fn storage_fees(
        address: Address,
        valid_for_block_height: u64,
        parent_blockhash: FixedBytes<32>,
    ) -> SystemTransaction {
        SystemTransaction {
            inner: TransactionPacket::StorageFees(system_tx::BalanceDecrement {
                amount: U256::ONE,
                target: address,
            }),
            valid_for_block_height,
            parent_blockhash,
        }
    }

    /// Assert that a log topic is present in block execution receipts at least `desired_repetitions` times.
    pub fn asserst_topic_present_in_logs(
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

    /// Get the balance of an address from a node.
    pub fn get_balance<N, AddOns>(
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

    /// Get the nonce of an address from a node.
    pub fn get_nonce<N, AddOns>(node: &FullNode<N, AddOns>, addr: Address) -> u64
    where
        N: FullNodeComponents<Provider: CanonStateSubscriptions>,
        AddOns: RethRpcAddOns<N, EthApi: EthTransactions>,
        N::Types: NodeTypes<Primitives: FullNodePrimitives>,
    {
        let signer_balance = node
            .provider
            .basic_account(&addr)
            .map(|account_info| account_info.map_or(0, |acc| acc.nonce))
            .unwrap_or_else(|err| {
                tracing::warn!("Failed to get nonce: {}", err);
                0
            });
        signer_balance
    }

    /// Sign a legacy transaction with the provided signer.
    pub async fn sign_tx(
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

    /// Returns a custom chain spec for testing.
    pub fn custom_chain() -> Arc<ChainSpec> {
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

    /// Returns payload attributes for a given timestamp.
    pub fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        };
        EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
    }

    /// Returns payload attributes for a given timestamp and parent block hash.
    pub fn eth_payload_attributes_with_parent(
        timestamp: u64,
        parent_block_hash: B256,
    ) -> EthPayloadBuilderAttributes {
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
        };
        EthPayloadBuilderAttributes::new(parent_block_hash, attributes)
    }

    /// Launches and connects multiple Irys+reth nodes for integration tests.
    ///
    /// # Arguments
    /// - `num_nodes`: Addresses for allowed system tx origins (one per node)
    /// - `chain_spec`: Chain spec to use
    /// - `is_dev`: Whether to run in dev mode
    /// - `attributes_generator`: Function to generate payload attributes
    ///
    /// # Returns
    /// - `Vec<NodeHelperType<IrysEthereumNode>>`: Test node handles
    /// - `TaskManager`: Task manager for async tasks
    /// - `Wallet`: Default wallet for test accounts
    pub async fn setup_irys_reth(
        num_nodes: &[Address],
        chain_spec: Arc<<IrysEthereumNode as NodeTypes>::ChainSpec>,
        is_dev: bool,
        attributes_generator: impl Fn(u64) -> <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadBuilderAttributes + Send + Sync + Clone + 'static,
    ) -> eyre::Result<(Vec<NodeHelperType<IrysEthereumNode>>, TaskManager, Wallet)>
    where
        LocalPayloadAttributesBuilder<<IrysEthereumNode as NodeTypes>::ChainSpec>:
            PayloadAttributesBuilder<
                <<IrysEthereumNode as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
            >,
    {
        let tasks = TaskManager::current();
        let exec = tasks.executor();

        let network_config = NetworkArgs {
            discovery: DiscoveryArgs {
                disable_discovery: true,
                ..DiscoveryArgs::default()
            },
            ..NetworkArgs::default()
        };

        // Create nodes and peer them
        let mut nodes: Vec<NodeTestContext<_, _>> = Vec::with_capacity(num_nodes.len());

        for (idx, allowed_system_tx_origin) in num_nodes.iter().enumerate() {
            let node_config = NodeConfig::new(chain_spec.clone())
                .with_network(network_config.clone())
                .with_unused_ports()
                .with_rpc(RpcServerArgs::default().with_unused_ports().with_http())
                .set_dev(is_dev);

            let span = span!(Level::INFO, "node", idx);
            let _enter = span.enter();
            let NodeHandle {
                node,
                node_exit_future: _,
            } = NodeBuilder::new(node_config.clone())
                .testing_node(exec.clone())
                .node(IrysEthereumNode {
                    allowed_system_tx_origin: *allowed_system_tx_origin,
                })
                .launch()
                .await?;

            let mut node = NodeTestContext::new(node, attributes_generator.clone()).await?;

            // Connect each node in a chain.
            if let Some(previous_node) = nodes.last_mut() {
                previous_node.connect(&mut node).await;
            }

            // Connect last node with the first if there are more than two
            if idx + 1 == num_nodes.len() && num_nodes.len() > 2 {
                if let Some(first_node) = nodes.first_mut() {
                    node.connect(first_node).await;
                }
            }

            nodes.push(node);
        }

        Ok((
            nodes,
            tasks,
            Wallet::default().with_chain_id(chain_spec.chain().into()),
        ))
    }
}
