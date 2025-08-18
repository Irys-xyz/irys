// Standard library imports
use core::convert::Infallible;

// External crate imports - Alloy
use alloy_consensus::{Block, Header, Transaction as _};
use alloy_dyn_abi::DynSolValue;
use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
use alloy_evm::eth::receipt_builder::ReceiptBuilder as _;
use alloy_evm::eth::EthBlockExecutor;
use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{Address, Bytes, FixedBytes, Log, LogData, U256};

// External crate imports - Reth
use reth::primitives::{SealedBlock, SealedHeader};
use reth::providers::BlockExecutionResult;
use reth::revm::context::result::ExecutionResult;
use reth::revm::context::TxEnv;
use reth::revm::either::Either;
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
use reth_evm::{ConfigureEvm, EthEvmFactory, EvmEnv, EvmFactory, NextBlockEnvAttributes};
use reth_evm_ethereum::{EthBlockAssembler, RethReceiptBuilder};

// External crate imports - Revm
use revm::context::result::{EVMError, HaltReason, InvalidTransaction, Output};
use revm::context::{BlockEnv, Cfg as _, CfgEnv, ContextTr as _};
use revm::database::states::plain_account::PlainStorage;
use revm::database::PlainAccount;
use revm::inspector::NoOpInspector;
use revm::precompile::{PrecompileSpecId, Precompiles};
use revm::state::{Account, AccountStatus, EvmStorageSlot};
use revm::{DatabaseCommit as _, MainBuilder as _, MainContext as _};
use tracing::trace;

// External crate imports - Other

use super::*;
use crate::shadow_tx::{self, ShadowTransaction, IRYS_SHADOW_EXEC, SHADOW_TX_DESTINATION_ADDR};

/// Constants for shadow transaction processing
mod constants {
    /// Gas used for shadow transactions (always 0)
    pub(super) const SHADOW_TX_GAS_USED: u64 = 0;

    /// Gas refunded for shadow transactions (always 0)
    pub(super) const SHADOW_TX_GAS_REFUNDED: u64 = 0;

    /// Cumulative gas used for shadow transaction receipts
    pub(super) const SHADOW_TX_CUMULATIVE_GAS: u64 = 0;
}

/// Result type for shadow transaction processing
type ShadowTransactionResult<HaltReason> =
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

/// Irys block executor that handles execution of both regular and shadow transactions.
#[derive(Debug)]
pub struct IrysBlockExecutor<'a, Evm> {
    receipt_builder: &'a RethReceiptBuilder,
    shadow_tx_receipts: Vec<Receipt>,
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

    /// Executes a transaction with custom commit logic for shadow transactions.
    ///
    /// This method handles both regular Ethereum transactions and Irys shadow transactions.
    /// Shadow transactions are special protocol-level operations that modify account balances
    /// according to consensus rules (staking, rewards, fees, etc.).
    ///
    /// # Shadow Transaction Processing
    /// Shadow transactions undergo additional validation:
    /// 1. Parent block hash must match current chain state
    /// 2. Block height must match current block number
    /// 3. Balance operations must respect account constraints
    ///
    /// # Note
    /// When executing shadow transactions, reth may give a warning: "State root task returned incorrect state root"
    /// This is because we require direct access to the db to execute shadow txs, preventing parallel state root
    /// computations. This does not affect the correctness of the block.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        on_result_f: impl FnOnce(
            &ExecutionResult<<Self::Evm as Evm>::HaltReason>,
        ) -> reth_evm::block::CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        // divert processing to the underlying EVM, which now has shadow processing logic
        // we keep this old code for now in case I need to hotfix something ;)
        return self
            .inner
            .execute_transaction_with_commit_condition(tx, on_result_f);

        #[expect(unreachable_code)]
        let tx_envelope = tx.tx();
        let tx_envelope_input_buf = tx_envelope.input();

        if tx_envelope.to() != Some(*SHADOW_TX_DESTINATION_ADDR) {
            // if the tx is not a shadow tx, execute it as a regular transaction
            return self
                .inner
                .execute_transaction_with_commit_condition(tx, on_result_f);
        }

        if tx_envelope_input_buf
            .strip_prefix(IRYS_SHADOW_EXEC)
            .is_none()
        {
            // if the tx does not contain the shadow prefix, treat it as a regular transaction
            return self
                .inner
                .execute_transaction_with_commit_condition(tx, on_result_f);
        };

        let shadow_tx = ShadowTransaction::decode(&mut &tx_envelope_input_buf[..])
            .map_err(|e| create_internal_error(&format!("failed to decode shadow tx: {e}")))?;

        tracing::trace!(tx_hash = %tx.tx().hash(), "executing shadow transaction");

        // Calculate and distribute priority fee to beneficiary BEFORE executing shadow tx
        let priority_fee = tx_envelope.max_priority_fee_per_gas().unwrap_or(0);
        let total_fee = U256::from(priority_fee);

        // Get the target address from the shadow transaction
        let fee_payer_address = match &shadow_tx {
            ShadowTransaction::V1 { packet } => packet.fee_payer_address(),
        };

        // Check if this is a block reward transaction
        let is_block_reward = matches!(
            &shadow_tx,
            ShadowTransaction::V1 {
                packet: shadow_tx::TransactionPacket::BlockReward(_),
                ..
            }
        );

        // Block reward transactions MUST have 0 priority fee
        if is_block_reward && !total_fee.is_zero() {
            tracing::error!(
                tx_hash = %tx_envelope.hash(),
                priority_fee = %total_fee,
                "Block reward transaction with non-zero priority fee"
            );
            return Err(create_invalid_tx_error(
                *tx_envelope.hash(),
                InvalidTransaction::PriorityFeeGreaterThanMaxFee,
            ));
        }

        let beneficiary = {
            use revm::context::Block as _;
            self.inner().evm().block().beneficiary()
        };

        // Distribute priority fees
        self.distribute_priority_fee(
            total_fee,
            fee_payer_address,
            beneficiary,
            tx_envelope.hash(),
        )?;

        // Process the shadow transaction
        let (new_account_state, target) =
            self.process_shadow_transaction(&shadow_tx, tx_envelope.hash(), beneficiary)?;

        let mut new_state = alloy_primitives::map::foldhash::HashMap::default();

        // at this point, the shadow tx has been processed, and it was valid *enough*
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

                let account = Account {
                    info: plain_account.info,
                    storage,
                    status,
                };

                new_state.insert(target, account);

                execution_result
            }
            Err(execution_result) => execution_result,
        };

        if !on_result_f(&execution_result).should_commit() {
            return Ok(None);
        }

        // Build and store the receipt
        let evm = self.inner.evm_mut();
        self.shadow_tx_receipts
            .push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx: tx_envelope,
                evm,
                result: execution_result,
                state: &new_state,
                cumulative_gas_used: constants::SHADOW_TX_CUMULATIVE_GAS,
            }));

        // Commit the changes to the database
        let db = evm.db_mut();
        db.commit(new_state);
        Ok(Some(0))
    }

    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<Receipt>), BlockExecutionError> {
        let (evm, mut block_res) = self.inner.finish()?;
        // Combine shadow receipts with regular transaction receipts
        let total_receipts = [self.shadow_tx_receipts, block_res.receipts].concat();
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
    /// Processes a shadow transaction and returns the new account state and target address
    fn process_shadow_transaction(
        &mut self,
        shadow_tx: &ShadowTransaction,
        tx_envelope_hash: &FixedBytes<32>,
        beneficiary: Address,
    ) -> Result<(ShadowTransactionResult<<E as Evm>::HaltReason>, Address), BlockExecutionError>
    {
        let topic = shadow_tx.topic();

        match shadow_tx {
            shadow_tx::ShadowTransaction::V1 { packet, .. } => match packet {
                shadow_tx::TransactionPacket::Unstake(either_inc_dec)
                | shadow_tx::TransactionPacket::Unpledge(either_inc_dec) => match either_inc_dec {
                    shadow_tx::EitherIncrementOrDecrement::BalanceIncrement(balance_increment) => {
                        let log = Self::create_shadow_log(
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
                    shadow_tx::EitherIncrementOrDecrement::BalanceDecrement(balance_decrement) => {
                        let log = Self::create_shadow_log(
                            balance_decrement.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_decrement.amount, 256),
                                DynSolValue::Address(balance_decrement.target),
                            ],
                        );
                        let target = balance_decrement.target;
                        let res = self.handle_balance_decrement(
                            log,
                            &balance_decrement.irys_ref,
                            balance_decrement,
                        )?;
                        Ok((
                            res.map(|(plain_account, execution_result)| {
                                (plain_account, execution_result, true)
                            }),
                            target,
                        ))
                    }
                },
                shadow_tx::TransactionPacket::BlockReward(block_reward_increment) => {
                    let log = Self::create_shadow_log(
                        beneficiary,
                        vec![topic],
                        vec![
                            DynSolValue::Uint(block_reward_increment.amount, 256),
                            DynSolValue::Address(beneficiary),
                        ],
                    );
                    let target = beneficiary;
                    let balance_increment = shadow_tx::BalanceIncrement {
                        amount: block_reward_increment.amount,
                        target: beneficiary,
                        irys_ref: alloy_primitives::FixedBytes::ZERO,
                    };
                    let (plain_account, execution_result, account_existed) =
                        self.handle_balance_increment(log, &balance_increment);
                    Ok((
                        Ok((plain_account, execution_result, account_existed)),
                        target,
                    ))
                }
                shadow_tx::TransactionPacket::Stake(balance_decrement)
                | shadow_tx::TransactionPacket::StorageFees(balance_decrement)
                | shadow_tx::TransactionPacket::Pledge(balance_decrement) => {
                    let log = Self::create_shadow_log(
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
                shadow_tx::TransactionPacket::TermFeeReward(balance_increment)
                | shadow_tx::TransactionPacket::IngressProofReward(balance_increment) => {
                    let log = Self::create_shadow_log(
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
            },
        }
    }

    /// Creates a successful execution result for shadow transactions
    fn create_success_result(log: Log) -> ExecutionResult<<E as Evm>::HaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Return,
            gas_used: constants::SHADOW_TX_GAS_USED,
            gas_refunded: constants::SHADOW_TX_GAS_REFUNDED,
            logs: vec![log],
            output: Output::Call(Bytes::new()),
        }
    }

    /// Creates a shadow transaction log with the specified event name and parameters
    fn create_shadow_log(
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

    /// Handles shadow transaction that increases account balance
    fn handle_balance_increment(
        &mut self,
        log: Log,
        balance_increment: &shadow_tx::BalanceIncrement,
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

    /// Handles shadow transaction that decreases account balance
    #[expect(clippy::type_complexity, reason = "original trait definition")]
    fn handle_balance_decrement(
        &mut self,
        log: Log,
        tx_hash: &FixedBytes<32>,
        balance_decrement: &shadow_tx::BalanceDecrement,
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
                gas_used: constants::SHADOW_TX_GAS_USED,
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

    /// Distributes priority fees from shadow transactions to the block beneficiary.
    ///
    /// # Priority Fee Distribution Flow
    ///
    /// Shadow transactions support EIP-1559 style priority fees that are distributed
    /// to the block beneficiary (miner) BEFORE the shadow transaction is executed.
    ///
    /// ## Process:
    /// 1. Calculate total fee from `max_priority_fee_per_gas` (direct value, no gas multiplication)
    /// 2. Validate target has sufficient balance
    /// 3. Deduct fee from target account
    /// 4. Add fee to beneficiary account (create a new one if needed)
    /// 5. Commit state changes
    ///
    /// ## Early Returns:
    /// - No fee to distribute (fee is zero).
    ///   This is valid because enforcement of which shadow txs to include in the block is up to the consensus layer.
    /// - No target address in shadow transaction
    ///
    /// ## Error Cases:
    /// - Target account doesn't exist
    /// - Target has insufficient balance
    /// - Database operation failures
    fn distribute_priority_fee(
        &mut self,
        total_fee: U256,
        fee_payer_address: Option<Address>,
        beneficiary: Address,
        tx_hash: &FixedBytes<32>,
    ) -> Result<(), BlockExecutionError> {
        // Early return if no fee to distribute
        if total_fee.is_zero() {
            return Ok(());
        }

        // Early return if no target address
        let Some(target) = fee_payer_address else {
            tracing::trace!(
                "No target address for shadow transaction, skipping priority fee distribution"
            );
            return Ok(());
        };

        tracing::debug!(
            beneficiary = %beneficiary,
            target = %target,
            total_fee = %total_fee,
            "Distributing priority fee"
        );

        // Prepare fee transfer
        let (target_state, beneficiary_state) =
            self.prepare_fee_transfer(target, beneficiary, total_fee, tx_hash)?;

        // Commit the fee transfer
        self.commit_fee_transfer(
            target_state,
            beneficiary_state,
            target,
            beneficiary,
            total_fee,
        );

        Ok(())
    }

    /// Prepares the fee transfer by validating and creating state changes for both accounts.
    ///
    /// Returns the updated state for both target and beneficiary accounts.
    fn prepare_fee_transfer(
        &mut self,
        target: Address,
        beneficiary: Address,
        total_fee: U256,
        tx_hash: &FixedBytes<32>,
    ) -> Result<(Account, Account), BlockExecutionError> {
        let evm = self.inner.evm_mut();
        let db = evm.db_mut();

        // Deduct fee from target
        let target_account = Self::deduct_fee_from_target(db, target, total_fee, tx_hash)?;
        let target_state = Self::create_account_state(target_account, AccountStatus::Touched);

        // Add fee to beneficiary
        let (beneficiary_account, is_new) =
            Self::add_fee_to_beneficiary(db, beneficiary, total_fee)?;
        let mut status = AccountStatus::Touched;
        if is_new {
            status |= AccountStatus::Created;
        }
        let beneficiary_state = Self::create_account_state(beneficiary_account, status);

        Ok((target_state, beneficiary_state))
    }

    /// Deducts the fee from the target account after validating sufficient balance.
    ///
    /// # Errors
    /// - Returns `InvalidTransaction::NonceTooHigh` if target account doesn't exist
    /// - Returns `InvalidTransaction::LackOfFundForMaxFee` if insufficient balance
    fn deduct_fee_from_target(
        db: &mut State<DB>,
        target: Address,
        fee: U256,
        tx_hash: &FixedBytes<32>,
    ) -> Result<PlainAccount, BlockExecutionError> {
        // Load target account
        let target_state = db
            .load_cache_account(target)
            .map_err(|_| create_internal_error("Could not load target account"))?;

        // Validate account exists
        let Some(existing) = target_state.account.as_ref() else {
            tracing::warn!(
                target = %target,
                "Target account does not exist, cannot deduct priority fee"
            );
            return Err(create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(fee),
                    balance: Box::new(U256::ZERO),
                },
            ));
        };

        let mut account = existing.clone();

        // Validate sufficient balance
        if account.info.balance < fee {
            tracing::warn!(
                target = %target,
                balance = %account.info.balance,
                required_fee = %fee,
                "Target has insufficient balance for priority fee"
            );
            return Err(create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(fee),
                    balance: Box::new(account.info.balance),
                },
            ));
        }

        // Deduct fee
        account.info.balance = account.info.balance.saturating_sub(fee);

        tracing::trace!(
            target = %target,
            fee = %fee,
            "Deducting priority fee from target"
        );

        Ok(account)
    }

    /// Adds the fee to the beneficiary account, creating it if necessary.
    ///
    /// Returns tuple of (account, is_new_account).
    fn add_fee_to_beneficiary(
        db: &mut State<DB>,
        beneficiary: Address,
        fee: U256,
    ) -> Result<(PlainAccount, bool), BlockExecutionError> {
        // Load beneficiary account
        let beneficiary_state = db
            .load_cache_account(beneficiary)
            .map_err(|_| create_internal_error("Could not load beneficiary account"))?;

        if let Some(existing) = beneficiary_state.account.as_ref() {
            // Update existing account
            let mut account = existing.clone();
            let original_balance = account.info.balance;
            account.info.balance = account.info.balance.saturating_add(fee);

            tracing::trace!(
                beneficiary = %beneficiary,
                original_balance = %original_balance,
                new_balance = %account.info.balance,
                fee_amount = %fee,
                "Incrementing beneficiary balance with priority fee"
            );

            Ok((account, false))
        } else {
            // Create new account
            let mut account = PlainAccount::new_empty_with_storage(PlainStorage::default());
            account.info.balance = fee;

            tracing::debug!(
                beneficiary = %beneficiary,
                balance = %fee,
                "Creating new beneficiary account with priority fee"
            );

            Ok((account, true))
        }
    }

    /// Creates an Account state entry from a PlainAccount for database commit.
    fn create_account_state(account: PlainAccount, status: AccountStatus) -> Account {
        let storage = account
            .storage
            .iter()
            .map(|(key, val)| (*key, EvmStorageSlot::new(*val)))
            .collect();

        Account {
            info: account.info,
            storage,
            status,
        }
    }

    /// Commits the fee transfer state changes to the database.
    fn commit_fee_transfer(
        &mut self,
        target_state: Account,
        beneficiary_state: Account,
        target: Address,
        beneficiary: Address,
        total_fee: U256,
    ) {
        let mut state_changes = alloy_primitives::map::foldhash::HashMap::default();
        state_changes.insert(target, target_state);
        state_changes.insert(beneficiary, beneficiary_state);

        let db = self.inner.evm_mut().db_mut();
        db.commit(state_changes);

        tracing::trace!(
            beneficiary = %beneficiary,
            target = %target,
            fee = %total_fee,
            "Committed priority fee distribution"
        );
    }
}

/// Irys block assembler that ensures proper ordering and inclusion of shadow transactions.
///
/// This assembler wraps the standard Ethereum block assembler ensures that shadow transactions are properly ordered
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

/// Factory for creating Irys block executors with shadow transaction support.
///
/// This factory produces [`IrysBlockExecutor`] instances that can handle both
/// regular Ethereum transactions and Irys-specific shadow transactions. It wraps
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
        I: Inspector<<IrysEvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        let receipt_builder = self.inner.receipt_builder();
        IrysBlockExecutor {
            inner: EthBlockExecutor::new(evm, ctx, self.inner.spec(), receipt_builder),
            receipt_builder,
            shadow_tx_receipts: vec![],
        }
    }
}

/// Irys EVM configuration that integrates shadow transaction support.
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
pub struct IrysEvmFactory {}

impl IrysEvmFactory {
    pub fn new() -> Self {
        Self {}
    }
}

impl EvmFactory for IrysEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = IrysEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = revm::Context<BlockEnv, TxEnv, CfgEnv, DB>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        IrysEvm::new(
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
        IrysEvm::new(
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

use revm::{
    context::Evm as RevmEvm,
    context_interface::result::ResultAndState,
    handler::{instructions::EthInstructions, EthPrecompiles, PrecompileProvider},
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
    Context, ExecuteEvm as _, InspectEvm as _,
};

use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

type ShadowTransactionResult2<HaltReason> =
    Result<(Account, ExecutionResult<HaltReason>, bool), ExecutionResult<HaltReason>>;

#[expect(missing_debug_implementations)]
pub struct IrysEvm<DB: Database, I, PRECOMPILE = EthPrecompiles> {
    inner: RevmEvm<
        EthEvmContext<DB>,
        I,
        EthInstructions<EthInterpreter, EthEvmContext<DB>>,
        PRECOMPILE,
    >,
    inspect: bool,
    state: revm_primitives::map::foldhash::HashMap<Address, Account>,
}

impl<DB: Database, I, PRECOMPILE> IrysEvm<DB, I, PRECOMPILE> {
    /// Creates a new Irys EVM instance.
    ///
    /// The `inspect` argument determines whether the configured [`Inspector`] of the given
    /// [`RevmEvm`] should be invoked on [`Evm::transact`].
    pub fn new(
        evm: RevmEvm<
            EthEvmContext<DB>,
            I,
            EthInstructions<EthInterpreter, EthEvmContext<DB>>,
            PRECOMPILE,
        >,
        inspect: bool,
    ) -> Self {
        Self {
            inner: evm,
            inspect,
            state: Default::default(),
        }
    }

    /// Consumes self and return the inner EVM instance.
    pub fn into_inner(
        self,
    ) -> RevmEvm<EthEvmContext<DB>, I, EthInstructions<EthInterpreter, EthEvmContext<DB>>, PRECOMPILE>
    {
        self.inner
    }

    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &EthEvmContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut EthEvmContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I, PRECOMPILE> Deref for IrysEvm<DB, I, PRECOMPILE> {
    type Target = EthEvmContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, PRECOMPILE> DerefMut for IrysEvm<DB, I, PRECOMPILE> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I, PRECOMPILE> Evm for IrysEvm<DB, I, PRECOMPILE>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
    PRECOMPILE: PrecompileProvider<EthEvmContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type Precompiles = PRECOMPILE;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(&mut self, tx: Self::Tx) -> Result<ResultAndState, Self::Error> {
        // run this tx through our processing first, if it's not a shadow tx we return here and pass it on
        let tx = match self.process_shadow_tx(tx)? {
            Either::Left(res) => return Ok(res),
            Either::Right(tx) => tx,
        };

        if self.inspect {
            self.inner.set_tx(tx);
            self.inner.inspect_replay()
        } else {
            self.inner.transact(tx)
        }
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState, Self::Error> {
        let tx = TxEnv {
            caller,
            kind: TxKind::Call(contract),
            // Explicitly set nonce to 0 so revm does not do any nonce checks
            nonce: 0,
            gas_limit: 30_000_000,
            value: U256::ZERO,
            data,
            // Setting the gas price to zero enforces that no value is transferred as part of the
            // call, and that the call will not count against the block's gas limit
            gas_price: 0,
            // The chain ID check is not relevant here and is disabled if set to None
            chain_id: None,
            // Setting the gas priority fee to None ensures the effective gas price is derived from
            // the `gas_price` field, which we need to be zero
            gas_priority_fee: None,
            access_list: Default::default(),
            // blob fields can be None for this tx
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: 0,
            authorization_list: Default::default(),
        };

        let mut gas_limit = tx.gas_limit;
        let mut basefee = 0;
        let mut disable_nonce_check = true;

        // ensure the block gas limit is >= the tx
        core::mem::swap(&mut self.block.gas_limit, &mut gas_limit);
        // disable the base fee check for this call by setting the base fee to zero
        core::mem::swap(&mut self.block.basefee, &mut basefee);
        // disable the nonce check
        core::mem::swap(&mut self.cfg.disable_nonce_check, &mut disable_nonce_check);

        let mut res = self.transact(tx);

        // swap back to the previous gas limit
        core::mem::swap(&mut self.block.gas_limit, &mut gas_limit);
        // swap back to the previous base fee
        core::mem::swap(&mut self.block.basefee, &mut basefee);
        // swap back to the previous nonce check flag
        core::mem::swap(&mut self.cfg.disable_nonce_check, &mut disable_nonce_check);

        // NOTE: We assume that only the contract storage is modified. Revm currently marks the
        // caller and block beneficiary accounts as "touched" when we do the above transact calls,
        // and includes them in the result.
        //
        // We're doing this state cleanup to make sure that changeset only includes the changed
        // contract storage.
        if let Ok(res) = &mut res {
            res.state.retain(|addr, _| *addr == contract);
        }

        res
    }

    fn db_mut(&mut self) -> &mut Self::DB {
        &mut self.journaled_state.database
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context {
            block: block_env,
            cfg: cfg_env,
            journaled_state,
            ..
        } = self.inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn precompiles(&self) -> &Self::Precompiles {
        &self.inner.precompiles
    }

    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.inner.precompiles
    }

    fn inspector(&self) -> &Self::Inspector {
        &self.inner.inspector
    }

    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inspector
    }
}

impl<DB, I, PRECOMPILE> IrysEvm<DB, I, PRECOMPILE>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
    PRECOMPILE: PrecompileProvider<EthEvmContext<DB>, Output = InterpreterResult>,
{
    // type Error = <Self as Evm>::Error; unstable :c

    pub fn create_internal_error(msg: String) -> <Self as Evm>::Error {
        <Self as Evm>::Error::Custom(msg)
    }

    fn create_invalid_tx_error(
        _tx_hash: FixedBytes<32>,
        err: InvalidTransaction,
    ) -> <Self as Evm>::Error {
        <Self as Evm>::Error::Transaction(err)
    }

    // Process a `TxEvm`, checking to see if it's a valid shadow tx.
    // if the tx is not a shadow tx, we return the tx for normal processing.
    pub fn process_shadow_tx(
        &mut self,
        tx: <Self as Evm>::Tx,
    ) -> Result<Either<ResultAndState, <Self as Evm>::Tx>, <Self as Evm>::Error> {
        // figure out if this is a shadow tx

        // check that the call target is [`SHADOW_TX_DESTINATION_ADDR`]
        let to_address = match tx.kind {
            TxKind::Create => return Ok(Either::Right(tx)),
            TxKind::Call(address) => address,
        };
        let tx_envelope_input_buf = &tx.data;

        if to_address != *SHADOW_TX_DESTINATION_ADDR {
            trace!("Not a shadow tx");
            return Ok(Either::Right(tx));
        };

        // check that the tx data/input bytes start with the required prefix [`IRYS_SHADOW_EXEC`]
        if tx_envelope_input_buf
            .strip_prefix(IRYS_SHADOW_EXEC)
            .is_none()
        {
            trace!("Not a shadow tx");
            return Ok(Either::Right(tx));
        };

        // we've determined that this is/should be a shadow tx

        let shadow_tx = ShadowTransaction::decode(&mut &tx_envelope_input_buf[..])
            .map_err(|e| Self::create_internal_error(format!("failed to decode shadow tx: {e}")))?;

        tracing::trace!("executing shadow transaction");

        // Calculate and distribute priority fee to beneficiary BEFORE executing shadow tx
        // note: gas_priority_fee is `max_priority_fee_per_gas`, remapped in `impl FromRecoveredTx<TxEip1559> for TxEnv` in alloy-revm
        // no idea why the name is different though.
        let priority_fee = tx.gas_priority_fee.unwrap_or(0);
        let total_fee = U256::from(priority_fee);

        // Get the target address from the shadow transaction
        let fee_payer_address = match &shadow_tx {
            ShadowTransaction::V1 { packet } => packet.fee_payer_address(),
        };

        // Check if this is a block reward transaction
        let is_block_reward = matches!(
            &shadow_tx,
            ShadowTransaction::V1 {
                packet: shadow_tx::TransactionPacket::BlockReward(_),
                ..
            }
        );

        // Block reward transactions MUST have 0 priority fee
        if is_block_reward && !total_fee.is_zero() {
            tracing::error!(

                priority_fee = %total_fee,
                "Block reward transaction with non-zero priority fee"
            );
            return Err(<Self as Evm>::Error::Transaction(
                InvalidTransaction::PriorityFeeGreaterThanMaxFee,
            ));
        }

        let beneficiary = self.block().beneficiary;

        // TODO: temporary as TxEnv doesn't have tx hash right now
        // eventually we *should* be able to modify the associated type to allow for this.
        let tx_hash: &FixedBytes<32> = &[0; 32].into();

        // the EVM factory should produce a new struct for every invocation
        assert!(
            self.state.is_empty(),
            "Custom IrysEVM state should be empty, but got: {:?}",
            &self.state.iter().collect::<Vec<_>>()
        );

        self.distribute_priority_fee(total_fee, fee_payer_address, beneficiary, tx_hash)?;

        let (new_account_state, target) =
            self.process_shadow_transaction(&shadow_tx, tx_hash, beneficiary)?;

        // at this point, the shadow tx has been processed, and it was valid *enough*
        // that we should generate a receipt for it even in a failure state
        let execution_result = match new_account_state {
            Ok((mut account, execution_result, account_existed)) => {
                self.commit_account_change(target, account.clone());

                let state_acc = self.state.get(&target).unwrap().clone();

                account.status = AccountStatus::Touched;
                // let mut status = AccountStatus::Touched;
                if account.info.is_empty() {
                    // Existing account that is still empty after increment - don't touch it
                    // This handles the case where increment amount is 0 or results in 0 balance
                    account.status |= AccountStatus::SelfDestructed;
                } else if !account_existed {
                    // New account being created with non-zero balance - mark as created and touched
                    account.status |= AccountStatus::Created;
                } else {
                    account.status = AccountStatus::Touched
                }

                // ensure status changing logic as part of `commit_account_change` is sane
                assert_eq!(
                    state_acc.status, account.status,
                    "Invalid account status flags: from commit {:?}, from prev: {:?}",
                    &state_acc.status, &account.status
                );

                execution_result
            }
            Err(execution_result) => execution_result,
        };

        // instead of "committing" the new state (which we can't do from this type-erased context minus some shenannigans), we just need to return it as part of the return type
        // do not use the journal for state checkpointing/unwinding
        // it doesn't have cases for all the operations we need to do
        // (notably arbitrary add/remove of balance)

        let result_state = ResultAndState {
            result: execution_result,
            state: std::mem::take(&mut self.state),
        };

        // HACK so that inspecting using trace returns a valid frame
        // as we don't call the "traditional" EVM methods, the inspector returns a default, invalid, frame.
        if self.inspect && result_state.result.is_success() {
            // insert a fake frame with the success
            let mut frame = revm::handler::execution::create_init_frame(
                &tx,
                self.ctx().cfg().spec(),
                tx.gas_limit,
            );

            // tell the inspector that we're starting a frame
            revm::inspector::handler::frame_start(
                &mut self.inner.ctx,
                &mut self.inner.inspector,
                &mut frame,
            );

            // force the output to be a valid frame
            let mut frame_result =
                revm::handler::FrameResult::Call(revm::interpreter::CallOutcome {
                    result: InterpreterResult {
                        result: revm::interpreter::InstructionResult::Continue,
                        output: Bytes::new(),
                        gas: revm::interpreter::Gas::new(0),
                    },
                    memory_offset: Default::default(),
                });

            // inject the valid frame
            revm::inspector::handler::frame_end(
                &mut self.inner.ctx,
                &mut self.inner.inspector,
                &frame,
                &mut frame_result,
            );
        }

        Ok(Either::Left(result_state))
    }

    /// Loads an account from the underlying state, or the database if there are no preexisting changes.
    /// NOTE: you MUST commit an account change before calling `load_account` for the same account, otherwise you will not get the latest version.
    /// TODO: improve this ^, and make this function responsible for account _creation_, i.e if an account doesn't exist, we use `Account::new_not_existing()`, instead of deferring this to the caller.
    pub fn load_account(
        &mut self,
        address: Address,
    ) -> Result<Option<Account>, <Self as Evm>::Error> {
        if let Some(entry) = self.state.get(&address) {
            return Ok(Some(entry.clone()));
        };

        let ctx = self.ctx_mut();
        let db = ctx.db();
        Ok(db
            .basic(address)
            .map_err(<Self as Evm>::Error::Database)?
            .map(std::convert::Into::into))
    }

    /// "Commits" a changed account to internal state.
    /// automatically changes the status of the account based on it's status
    pub fn commit_account_change(&mut self, address: Address, mut account: Account) {
        account.mark_touch();
        if account.is_empty() {
            account.mark_selfdestruct();
        } else if account.is_loaded_as_not_existing() {
            account.mark_created();
        }
        self.state.insert(address, account);
    }

    /// Distributes priority fees from shadow transactions to the block beneficiary.
    ///
    /// # Priority Fee Distribution Flow
    ///
    /// Shadow transactions support EIP-1559 style priority fees that are distributed
    /// to the block beneficiary (miner) BEFORE the shadow transaction is executed.
    ///
    /// ## Process:
    /// 1. Calculate total fee from `max_priority_fee_per_gas` (direct value, no gas multiplication)
    /// 2. Validate target has sufficient balance
    /// 3. Deduct fee from target account
    /// 4. Add fee to beneficiary account (create a new one if needed)
    /// 5. Commit state changes
    ///
    /// ## Early Returns:
    /// - No fee to distribute (fee is zero).
    ///   This is valid because enforcement of which shadow txs to include in the block is up to the consensus layer.
    /// - No target address in shadow transaction
    ///
    /// ## Error Cases:
    /// - Target account doesn't exist
    /// - Target has insufficient balance
    /// - Database operation failures
    pub fn distribute_priority_fee(
        &mut self,
        total_fee: U256,
        fee_payer_address: Option<Address>,
        beneficiary: Address,
        tx_hash: &FixedBytes<32>,
    ) -> Result<(), <Self as Evm>::Error> {
        // Early return if no fee to distribute
        if total_fee.is_zero() {
            return Ok(());
        }

        // Early return if no target address
        let Some(target) = fee_payer_address else {
            tracing::trace!(
                "No target address for shadow transaction, skipping priority fee distribution"
            );
            return Ok(());
        };

        tracing::debug!(
            beneficiary = %beneficiary,
            target = %target,
            total_fee = %total_fee,
            "Distributing priority fee"
        );

        // Prepare fee transfer
        let (target_state, beneficiary_state) =
            self.prepare_fee_transfer(target, beneficiary, total_fee, tx_hash)?;

        let bef = self.load_account(beneficiary)?.unwrap();

        self.commit_account_change(fee_payer_address.unwrap(), target_state);
        self.commit_account_change(beneficiary, beneficiary_state.clone());

        let aft = self.load_account(beneficiary)?.unwrap();

        assert_ne!(bef, aft);
        assert_eq!(beneficiary_state, aft);

        Ok(())
    }

    /// Prepares the fee transfer by validating and creating state changes for both accounts.
    ///
    /// Returns the updated state for both target and beneficiary accounts.
    fn prepare_fee_transfer(
        &mut self,
        target: Address,
        beneficiary: Address,
        total_fee: U256,
        tx_hash: &FixedBytes<32>,
    ) -> Result<(Account, Account), <Self as Evm>::Error> {
        // Deduct fee from target
        let mut target_state = self.deduct_fee_from_target(target, total_fee, tx_hash)?;
        target_state.status = AccountStatus::Touched;

        // Add fee to beneficiary
        let (mut beneficiary_account, is_new) =
            self.add_fee_to_beneficiary(beneficiary, total_fee)?;
        let mut status = AccountStatus::Touched;
        if is_new {
            status |= AccountStatus::Created;
        }
        beneficiary_account.status = status;

        Ok((target_state, beneficiary_account))
    }

    /// Deducts the fee from the target account after validating sufficient balance.
    ///
    /// # Errors
    /// - Returns `InvalidTransaction::NonceTooHigh` if target account doesn't exist
    /// - Returns `InvalidTransaction::LackOfFundForMaxFee` if insufficient balance
    fn deduct_fee_from_target(
        &mut self,
        target: Address,
        fee: U256,
        tx_hash: &FixedBytes<32>,
    ) -> Result<Account, <Self as Evm>::Error> {
        // Load target account
        let target_state = self.load_account(target)?;

        // Validate account exists
        let Some(mut account) = target_state else {
            tracing::warn!(
                target = %target,
                "Target account does not exist, cannot deduct priority fee"
            );
            return Err(Self::create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(fee),
                    balance: Box::new(U256::ZERO),
                },
            ));
        };

        // Validate sufficient balance
        if account.info.balance < fee {
            tracing::warn!(
                target = %target,
                balance = %account.info.balance,
                required_fee = %fee,
                "Target has insufficient balance for priority fee"
            );
            return Err(Self::create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(fee),
                    balance: Box::new(account.info.balance),
                },
            ));
        }

        // Deduct fee
        account.info.balance = account.info.balance.saturating_sub(fee);

        tracing::trace!(
            target = %target,
            fee = %fee,
            "Deducting priority fee from target"
        );

        Ok(account)
    }

    /// Adds the fee to the beneficiary account, creating it if necessary.
    ///
    /// Returns tuple of (account, is_new_account).
    fn add_fee_to_beneficiary(
        &mut self,
        beneficiary: Address,
        fee: U256,
    ) -> Result<(Account, bool), <Self as Evm>::Error> {
        // Load beneficiary account

        let beneficiary_state = self.load_account(beneficiary)?;

        if let Some(mut account) = beneficiary_state {
            // Update existing account
            let original_balance = account.info.balance;
            account.info.balance = account.info.balance.saturating_add(fee);

            tracing::trace!(
                beneficiary = %beneficiary,
                original_balance = %original_balance,
                new_balance = %account.info.balance,
                fee_amount = %fee,
                "Incrementing beneficiary balance with priority fee"
            );

            Ok((account, false))
        } else {
            // Create new account
            let mut account = Account::new_not_existing();

            account.info.balance = fee;

            tracing::trace!(
                beneficiary = %beneficiary,
                balance = %fee,
                "Creating new beneficiary account with priority fee"
            );

            Ok((account, true))
        }
    }

    /// Main branching processing function, handles all shadow tx variants & their required operations
    fn process_shadow_transaction(
        &mut self,
        shadow_tx: &ShadowTransaction,
        tx_hash: &FixedBytes<32>,
        beneficiary: Address,
    ) -> Result<(ShadowTransactionResult2<<Self as Evm>::HaltReason>, Address), <Self as Evm>::Error>
    {
        let topic = shadow_tx.topic();

        match shadow_tx {
            shadow_tx::ShadowTransaction::V1 { packet, .. } => match packet {
                shadow_tx::TransactionPacket::Unstake(either_inc_dec)
                | shadow_tx::TransactionPacket::Unpledge(either_inc_dec) => match either_inc_dec {
                    shadow_tx::EitherIncrementOrDecrement::BalanceIncrement(balance_increment) => {
                        let log = Self::create_shadow_log(
                            balance_increment.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_increment.amount, 256),
                                DynSolValue::Address(balance_increment.target),
                            ],
                        );
                        let target = balance_increment.target;
                        let (plain_account, execution_result, account_existed) =
                            self.handle_balance_increment(log, balance_increment)?;
                        Ok((
                            Ok((plain_account, execution_result, account_existed)),
                            target,
                        ))
                    }
                    shadow_tx::EitherIncrementOrDecrement::BalanceDecrement(balance_decrement) => {
                        let log = Self::create_shadow_log(
                            balance_decrement.target,
                            vec![topic],
                            vec![
                                DynSolValue::Uint(balance_decrement.amount, 256),
                                DynSolValue::Address(balance_decrement.target),
                            ],
                        );
                        let target = balance_decrement.target;
                        let res = self.handle_balance_decrement(
                            log,
                            &balance_decrement.irys_ref,
                            balance_decrement,
                        )?;
                        Ok((
                            res.map(|(plain_account, execution_result)| {
                                (plain_account, execution_result, true)
                            }),
                            target,
                        ))
                    }
                },
                shadow_tx::TransactionPacket::BlockReward(block_reward_increment) => {
                    let log = Self::create_shadow_log(
                        beneficiary,
                        vec![topic],
                        vec![
                            DynSolValue::Uint(block_reward_increment.amount, 256),
                            DynSolValue::Address(beneficiary),
                        ],
                    );
                    let target = beneficiary;
                    let balance_increment = shadow_tx::BalanceIncrement {
                        amount: block_reward_increment.amount,
                        target: beneficiary,
                        irys_ref: alloy_primitives::FixedBytes::ZERO,
                    };
                    let (plain_account, execution_result, account_existed) =
                        self.handle_balance_increment(log, &balance_increment)?;
                    Ok((
                        Ok((plain_account, execution_result, account_existed)),
                        target,
                    ))
                }
                shadow_tx::TransactionPacket::Stake(balance_decrement)
                | shadow_tx::TransactionPacket::StorageFees(balance_decrement)
                | shadow_tx::TransactionPacket::Pledge(balance_decrement) => {
                    let log = Self::create_shadow_log(
                        balance_decrement.target,
                        vec![topic],
                        vec![
                            DynSolValue::Uint(balance_decrement.amount, 256),
                            DynSolValue::Address(balance_decrement.target),
                        ],
                    );
                    let target = balance_decrement.target;
                    let res = self.handle_balance_decrement(log, tx_hash, balance_decrement)?;
                    Ok((
                        res.map(|(plain_account, execution_result)| {
                            (plain_account, execution_result, true)
                        }),
                        target,
                    ))
                }
                shadow_tx::TransactionPacket::TermFeeReward(balance_increment)
                | shadow_tx::TransactionPacket::IngressProofReward(balance_increment) => {
                    let log = Self::create_shadow_log(
                        balance_increment.target,
                        vec![topic],
                        vec![
                            DynSolValue::Uint(balance_increment.amount, 256),
                            DynSolValue::Address(balance_increment.target),
                        ],
                    );
                    let target = balance_increment.target;
                    let (plain_account, execution_result, account_existed) =
                        self.handle_balance_increment(log, balance_increment)?;
                    Ok((
                        Ok((plain_account, execution_result, account_existed)),
                        target,
                    ))
                }
            },
        }
    }

    fn create_shadow_log(
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

    /// Handles shadow transaction that increases account balance
    fn handle_balance_increment(
        &mut self,
        log: Log,
        balance_increment: &shadow_tx::BalanceIncrement,
    ) -> Result<(Account, ExecutionResult<<Self as Evm>::HaltReason>, bool), <Self as Evm>::Error>
    {
        let account = self.load_account(balance_increment.target)?;

        // Get the existing account or create a new one if it doesn't exist
        let account_existed = account.is_some();
        let account_info = if let Some(mut account) = account {
            let original_balance = account.info.balance;
            // Add the incremented amount to the balance
            account.info.balance = account
                .info
                .balance
                .saturating_add(balance_increment.amount);

            tracing::trace!(
                target_address = %balance_increment.target,
                original_balance = %original_balance,
                increment_amount = %balance_increment.amount,
                final_balance = %account.info.balance,
                "Balance increment on existing account"
            );

            account
        } else {
            // Create a new account with the incremented balance
            let mut account = Account::new_not_existing();
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

        Ok((account_info, execution_result, account_existed))
    }

    /// Handles shadow transaction that decreases account balance
    #[expect(clippy::type_complexity, reason = "original trait definition")]
    fn handle_balance_decrement(
        &mut self,
        log: Log,
        tx_hash: &FixedBytes<32>,
        balance_decrement: &shadow_tx::BalanceDecrement,
    ) -> Result<
        Result<
            (Account, ExecutionResult<<Self as Evm>::HaltReason>),
            ExecutionResult<<Self as Evm>::HaltReason>,
        >,
        <Self as Evm>::Error,
    > {
        let account = self.load_account(balance_decrement.target)?;

        // Get the existing account or create a new one if it doesn't exist
        // handle a case when an account has never existed (0 balance, no data stored on it)
        // We don't even create a receipt in this case (eth does the same with native txs)
        let Some(mut new_account_info) = account else {
            tracing::warn!(
                "account {} does not exist for balance decrement",
                &balance_decrement.target
            );
            return Err(Self::create_invalid_tx_error(
                *tx_hash,
                InvalidTransaction::OverflowPaymentInTransaction,
            ));
        };
        if new_account_info.info.balance < balance_decrement.amount {
            tracing::warn!(?new_account_info.info.balance, ?balance_decrement.amount);
            return Ok(Err(ExecutionResult::Revert {
                gas_used: constants::SHADOW_TX_GAS_USED,
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

    pub fn create_success_result(log: Log) -> ExecutionResult<<Self as Evm>::HaltReason> {
        ExecutionResult::Success {
            reason: revm::context::result::SuccessReason::Return,
            gas_used: constants::SHADOW_TX_GAS_USED,
            gas_refunded: constants::SHADOW_TX_GAS_REFUNDED,
            logs: vec![log],
            output: Output::Call(Bytes::new()),
        }
    }
}
