// Standard library imports
use core::convert::Infallible;

// External crate imports - Alloy
use alloy_consensus::{Block, Header, Transaction as _};
use alloy_dyn_abi::DynSolValue;
use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
use alloy_evm::eth::receipt_builder::ReceiptBuilder as _;
use alloy_evm::eth::EthBlockExecutor;
use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Bytes, FixedBytes, Log, LogData};

// External crate imports - Reth
use reth::primitives::{SealedBlock, SealedHeader};
use reth::providers::BlockExecutionResult;
use reth::revm::context::result::ExecutionResult;
use reth::revm::context::TxEnv;
use reth::revm::primitives::hardfork::SpecId;
use reth::revm::{Inspector, State};
use reth_ethereum_primitives::Receipt;
use reth_evm::block::{
    BlockExecutorFactory, BlockExecutorFor, BlockValidationError, CommitChanges,
};
use reth_evm::eth::receipt_builder::ReceiptBuilderCtx;
use reth_evm::eth::{EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmContext};
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_evm::precompiles::PrecompilesMap;
use reth_evm::{ConfigureEvm, EthEvm, EthEvmFactory, EvmEnv, EvmFactory, NextBlockEnvAttributes};
use reth_evm_ethereum::{EthBlockAssembler, RethReceiptBuilder};

// External crate imports - Revm
use revm::context::result::{EVMError, HaltReason, InvalidTransaction, Output};
use revm::context::{BlockEnv, CfgEnv};
use revm::database::states::plain_account::PlainStorage;
use revm::database::PlainAccount;
use revm::inspector::NoOpInspector;
use revm::precompile::{PrecompileSpecId, Precompiles};
use revm::state::{Account, AccountStatus, EvmStorageSlot};
use revm::Database as _;
use revm::{DatabaseCommit as _, MainBuilder as _, MainContext as _};

// External crate imports - Other
use tracing::error_span;

use super::*;

/// Constants for system transaction processing
mod constants {
    /// Gas used for system transactions (always 0)
    pub(super) const SYSTEM_TX_GAS_USED: u64 = 0;

    /// Gas refunded for system transactions (always 0)
    pub(super) const SYSTEM_TX_GAS_REFUNDED: u64 = 0;

    /// Cumulative gas used for system transaction receipts
    pub(super) const SYSTEM_TX_CUMULATIVE_GAS: u64 = 0;
}

/// Result type for system transaction processing
type SystemTransactionResult<HaltReason> =
    Result<(PlainAccount, ExecutionResult<HaltReason>, bool), ExecutionResult<HaltReason>>;

/// Helper for creating block validation errors
fn create_invalid_tx_error(
    hash: FixedBytes<32>,
    reason: InvalidTransaction,
) -> BlockExecutionError {
    BlockExecutionError::Validation(BlockValidationError::InvalidTx {
        hash,
        error: Box::new(reason),
    })
}

/// Helper for creating internal errors
fn create_internal_error(msg: &str) -> BlockExecutionError {
    BlockExecutionError::Internal(reth_evm::block::InternalBlockExecutionError::msg(msg))
}

/// Irys block executor that handles execution of both regular and system transactions.
#[derive(Debug)]
pub struct IrysBlockExecutor<'a, Evm> {
    receipt_builder: &'a RethReceiptBuilder,
    system_tx_receipts: Vec<Receipt>,
    inner: EthBlockExecutor<'a, Evm, &'a Arc<ChainSpec>, &'a RethReceiptBuilder>,
}

impl<'a, Evm> IrysBlockExecutor<'a, Evm> {
    /// Access the inner block executor
    pub const fn inner(
        &self,
    ) -> &EthBlockExecutor<'a, Evm, &'a Arc<ChainSpec>, &'a RethReceiptBuilder> {
        &self.inner
    }

    /// Access the inner block executor mutably
    pub const fn inner_mut(
        &mut self,
    ) -> &mut EthBlockExecutor<'a, Evm, &'a Arc<ChainSpec>, &'a RethReceiptBuilder> {
        &mut self.inner
    }
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

    fn execute_transaction_with_result_closure(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>),
    ) -> Result<u64, BlockExecutionError> {
        self.execute_transaction_with_commit_condition(tx, |res| {
            f(res);
            CommitChanges::Yes
        })
        .map(Option::unwrap_or_default)
    }

    /// Executes a transaction with custom commit logic for system transactions.
    ///
    /// This method handles both regular Ethereum transactions and Irys system transactions.
    /// System transactions are special protocol-level operations that modify account balances
    /// according to consensus rules (staking, rewards, fees, etc.).
    ///
    /// # System Transaction Processing
    /// System transactions undergo additional validation:
    /// 1. Parent block hash must match current chain state
    /// 2. Block height must match current block number
    /// 3. Balance operations must respect account constraints
    ///
    /// # Note
    /// When executing system transactions, reth may give a warning: "State root task returned incorrect state root"
    /// This is because we require direct access to the db to execute system txs, preventing parallel state root
    /// computations. This does not affect the correctness of the block.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        on_result_f: impl FnOnce(
            &ExecutionResult<<Self::Evm as Evm>::HaltReason>,
        ) -> reth_evm::block::CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        let tx_envelope = tx.tx();
        let tx_envelope_input_buf = tx_envelope.input();
        let rlp_decoded_system_tx = SystemTransaction::decode(&mut &tx_envelope_input_buf[..]);

        let Ok(system_tx) = rlp_decoded_system_tx else {
            // if the tx is not a system tx, execute it as a regular transaction
            return self
                .inner
                .execute_transaction_with_commit_condition(tx, on_result_f);
        };
        tracing::trace!(tx_hash = %tx.tx().hash(), "executing system transaction");

        // Validate system tx metadata
        self.validate_system_transaction_metadata(&system_tx, tx_envelope.hash())?;

        // Process the system transaction
        let (new_account_state, target) =
            self.process_system_transaction(&system_tx, tx_envelope.hash())?;

        let mut new_state = alloy_primitives::map::foldhash::HashMap::default();
        // at this point, the system tx has been processed, and it was valid *enough*
        // that we should generate a receipt for it even in a failure state
        let execution_result = match new_account_state {
            Ok((plain_account, execution_result, account_existed)) => {
                let storage = plain_account
                    .storage
                    .iter()
                    .map(|(key, val)| (*key, EvmStorageSlot::new(*val)))
                    .collect();
                let mut status = AccountStatus::Touched;
                if plain_account.info.is_empty() {
                    // Existing account that is still empty after increment - don't touch it
                    // This handles the case where increment amount is 0 or results in 0 balance
                    status |= AccountStatus::SelfDestructed;
                } else if !account_existed {
                    // New account being created with non-zero balance - mark as created and touched
                    status |= AccountStatus::Created;
                };

                new_state.insert(
                    target,
                    Account {
                        info: plain_account.info,
                        storage,
                        status,
                    },
                );

                execution_result
            }
            Err(execution_result) => execution_result,
        };

        if !on_result_f(&execution_result).should_commit() {
            return Ok(None);
        }

        // Build and store the receipt
        let evm = self.inner.evm_mut();
        self.system_tx_receipts
            .push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx: tx_envelope,
                evm,
                result: execution_result,
                state: &new_state,
                cumulative_gas_used: constants::SYSTEM_TX_CUMULATIVE_GAS,
            }));

        // Commit the changes to the database
        let db = evm.db_mut();
        db.commit(new_state);
        Ok(Some(0))
    }

    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<Receipt>), BlockExecutionError> {
        let (evm, mut block_res) = self.inner.finish()?;
        // Combine system receipts with regular transaction receipts
        let total_receipts = [self.system_tx_receipts, block_res.receipts].concat();
        block_res.receipts = total_receipts;

        Ok((evm, block_res))
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(hook);
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
    /// Validates system transaction metadata (block height and parent hash)
    fn validate_system_transaction_metadata(
        &mut self,
        system_tx: &SystemTransaction,
        tx_hash: &FixedBytes<32>,
    ) -> Result<(), BlockExecutionError> {
        let block_number = self.inner.evm().block().number;
        let block_hash = self
            .inner
            .evm_mut()
            .db_mut()
            .block_hash(block_number.saturating_sub(1))
            .map_err(|_err| create_internal_error("could not retrieve block by this hash"))?;

        let span = error_span!(
            "system_tx_processing",
            "parent_block_hash" = block_hash.to_string(),
            "block_number" = block_number,
            "allowed_parent_block_hash" = system_tx.parent_blockhash().to_string(),
            "allowed_block_height" = system_tx.valid_for_block_height()
        );
        let _guard = span.enter();

        // ensure that parent block hashes match.
        // This check ensures that a system tx does not get executed for an off-case fork of the desired chain.
        if system_tx.parent_blockhash() != block_hash {
            tracing::error!(
                "A system tx leaked into a block that was not approved by the system tx producer"
            );
            return Err(create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::PriorityFeeGreaterThanMaxFee,
            ));
        }

        // ensure that block heights match.
        // This ensures that the system tx does not leak into future blocks.
        if system_tx.valid_for_block_height() != block_number {
            tracing::error!(
                "A system tx leaked into a block that was not approved by the system tx producer"
            );
            return Err(create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::PriorityFeeGreaterThanMaxFee,
            ));
        }

        Ok(())
    }

    /// Processes a system transaction and returns the new account state and target address
    fn process_system_transaction(
        &mut self,
        system_tx: &SystemTransaction,
        tx_envelope_hash: &FixedBytes<32>,
    ) -> Result<(SystemTransactionResult<<E as Evm>::HaltReason>, Address), BlockExecutionError>
    {
        let topic = system_tx.topic();

        match system_tx {
            system_tx::SystemTransaction::V1 { packet, .. } => match packet {
                system_tx::TransactionPacket::Unstake(balance_increment)
                | system_tx::TransactionPacket::BlockReward(balance_increment)
                | system_tx::TransactionPacket::Unpledge(balance_increment) => {
                    let log = Self::create_system_log(
                        balance_increment.target,
                        vec![topic],
                        vec![
                            DynSolValue::Uint(balance_increment.amount, 256),
                            DynSolValue::Address(balance_increment.target),
                        ],
                    );
                    let target = balance_increment.target;
                    let (plain_account, execution_result, account_existed) =
                        self.handle_balance_increment(log, balance_increment);
                    Ok((
                        Ok((plain_account, execution_result, account_existed)),
                        target,
                    ))
                }
                system_tx::TransactionPacket::Stake(balance_decrement)
                | system_tx::TransactionPacket::StorageFees(balance_decrement)
                | system_tx::TransactionPacket::Pledge(balance_decrement) => {
                    let log = Self::create_system_log(
                        balance_decrement.target,
                        vec![topic],
                        vec![
                            DynSolValue::Uint(balance_decrement.amount, 256),
                            DynSolValue::Address(balance_decrement.target),
                        ],
                    );
                    let target = balance_decrement.target;
                    let res =
                        self.handle_balance_decrement(log, tx_envelope_hash, balance_decrement)?;
                    Ok((
                        res.map(|(plain_account, execution_result)| {
                            (plain_account, execution_result, true)
                        }),
                        target,
                    ))
                }
            },
        }
    }

    /// Creates a successful execution result for system transactions
    fn create_success_result(log: Log) -> ExecutionResult<<E as Evm>::HaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Return,
            gas_used: constants::SYSTEM_TX_GAS_USED,
            gas_refunded: constants::SYSTEM_TX_GAS_REFUNDED,
            logs: vec![log],
            output: Output::Call(Bytes::new()),
        }
    }

    /// Creates a system transaction log with the specified event name and parameters
    fn create_system_log(
        target: Address,
        topics: Vec<FixedBytes<32>>,
        params: Vec<DynSolValue>,
    ) -> Log {
        let encoded_data = DynSolValue::Tuple(params).abi_encode();
        Log {
            address: target,
            data: LogData::new(topics, encoded_data.into())
                .expect("System log creation should not fail"),
        }
    }

    /// Handles system transaction that increases account balance
    fn handle_balance_increment(
        &mut self,
        log: Log,
        balance_increment: &system_tx::BalanceIncrement,
    ) -> (PlainAccount, ExecutionResult<<E as Evm>::HaltReason>, bool) {
        let evm = self.inner.evm_mut();

        let db = evm.db_mut();
        let state = db
            .load_cache_account(balance_increment.target)
            .expect("Failed to load account for balance increment");

        // Get the existing account or create a new one if it doesn't exist
        let account_existed = state.account.is_some();
        let account_info = if let Some(plain_account) = state.account.as_ref() {
            let mut plain_account = plain_account.clone();
            let original_balance = plain_account.info.balance;
            // Add the incremented amount to the balance
            plain_account.info.balance = plain_account
                .info
                .balance
                .saturating_add(balance_increment.amount);

            tracing::trace!(
                target_address = %balance_increment.target,
                original_balance = %original_balance,
                increment_amount = %balance_increment.amount,
                final_balance = %plain_account.info.balance,
                "Balance increment on existing account"
            );

            plain_account
        } else {
            // Create a new account with the incremented balance
            let mut account = PlainAccount::new_empty_with_storage(PlainStorage::default());
            account.info.balance = balance_increment.amount;

            tracing::debug!(
                target_address = %balance_increment.target,
                increment_amount = %balance_increment.amount,
                final_balance = %account.info.balance,
                "Balance increment on new account"
            );

            account
        };

        let execution_result = Self::create_success_result(log);

        (account_info, execution_result, account_existed)
    }

    /// Handles system transaction that decreases account balance
    #[expect(clippy::type_complexity, reason = "original trait definition")]
    fn handle_balance_decrement(
        &mut self,
        log: Log,
        tx_hash: &FixedBytes<32>,
        balance_decrement: &system_tx::BalanceDecrement,
    ) -> Result<
        Result<
            (PlainAccount, ExecutionResult<<E as Evm>::HaltReason>),
            ExecutionResult<<E as Evm>::HaltReason>,
        >,
        BlockExecutionError,
    > {
        let evm = self.inner.evm_mut();

        let db = evm.db_mut();
        let state = db
            .load_cache_account(balance_decrement.target)
            .map_err(|_err| {
                create_internal_error("Could not load account for balance decrement")
            })?;

        // Get the existing account or create a new one if it doesn't exist
        // handle a case when an account has never existed (0 balance, no data stored on it)
        // We don't even create a receipt in this case (eth does the same with native txs)
        let Some(plain_account) = state.account.as_ref() else {
            tracing::warn!("account does not exist");
            return Err(create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::OverflowPaymentInTransaction,
            ));
        };
        let mut new_account_info = plain_account.clone();
        if new_account_info.info.balance < balance_decrement.amount {
            tracing::warn!(?plain_account.info.balance, ?balance_decrement.amount);
            return Ok(Err(ExecutionResult::Revert {
                gas_used: constants::SYSTEM_TX_GAS_USED,
                output: Bytes::new(),
            }));
        }
        // Apply the decrement amount to the balance
        new_account_info.info.balance = new_account_info
            .info
            .balance
            .saturating_sub(balance_decrement.amount);

        let execution_result = Self::create_success_result(log);

        Ok(Ok((new_account_info, execution_result)))
    }
}

/// Irys block assembler that ensures proper ordering and inclusion of system transactions.
///
/// This assembler wraps the standard Ethereum block assembler ensures that system transactions are properly ordered
/// and included according to protocol rules.
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

/// Factory for creating Irys block executors with system transaction support.
///
/// This factory produces [`IrysBlockExecutor`] instances that can handle both
/// regular Ethereum transactions and Irys-specific system transactions. It wraps
/// the standard Ethereum block executor factory with Irys-specific configuration.
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
    #[must_use]
    pub const fn receipt_builder(&self) -> &RethReceiptBuilder {
        self.inner.receipt_builder()
    }

    /// Exposes the chain specification.
    #[must_use]
    pub const fn spec(&self) -> &Arc<ChainSpec> {
        self.inner.spec()
    }

    /// Exposes the EVM factory.
    #[must_use]
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
        self.inner.evm_factory()
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

/// Irys EVM configuration that integrates system transaction support.
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
