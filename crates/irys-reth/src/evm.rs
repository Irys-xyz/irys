// Standard library imports
use core::convert::Infallible;

use alloy_consensus::{Block, Header};
use alloy_dyn_abi::DynSolValue;
use alloy_eips::eip2718::EIP4844_TX_TYPE_ID;
use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
use alloy_evm::eth::EthBlockExecutor;
use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{keccak256, Address, Bytes, FixedBytes, Log, LogData, U256};

use reth::primitives::{SealedBlock, SealedHeader};
use reth::providers::BlockExecutionResult;
use reth::revm::context::result::ExecutionResult;
use reth::revm::context::TxEnv;
use reth::revm::either::Either;
use reth::revm::primitives::hardfork::SpecId;
use reth::revm::{Inspector, State};
use reth_ethereum_primitives::Receipt;
use reth_evm::block::{BlockExecutorFactory, BlockExecutorFor, CommitChanges};
use reth_evm::eth::{EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmContext};
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_evm::precompiles::PrecompilesMap;
use reth_evm::{ConfigureEvm, EthEvmFactory, EvmEnv, EvmFactory, NextBlockEnvAttributes};
use reth_evm_ethereum::{EthBlockAssembler, RethReceiptBuilder};

use revm::context::result::{EVMError, HaltReason, InvalidTransaction, Output};
use revm::context::{BlockEnv, Cfg as _, CfgEnv, ContextTr as _};
use revm::inspector::NoOpInspector;
use revm::precompile::{PrecompileSpecId, Precompiles};
use revm::state::{Account, AccountStatus};
use revm::{MainBuilder as _, MainContext as _};
use std::sync::LazyLock;
use tracing::{trace, warn};

use super::*;
use crate::precompiles::pd::context::PdContext;
use crate::precompiles::pd::precompile::register_irys_precompiles_if_active;
use crate::shadow_tx::{self, ShadowTransaction};

/// Constants for shadow transaction processing
mod constants {
    /// Gas used for shadow transactions (always 0)
    pub(super) const SHADOW_TX_GAS_USED: u64 = 0;

    /// Gas refunded for shadow transactions (always 0)
    pub(super) const SHADOW_TX_GAS_REFUNDED: u64 = 0;
}

/// Magic account used to store PD base fee per chunk in its balance field.
/// This is set by a dedicated shadow transaction and read during PD fee charging.
pub static PD_BASE_FEE_ACCOUNT: LazyLock<Address> =
    LazyLock::new(|| Address::from_word(keccak256("irys_pd_base_fee_account")));

/// Irys block executor that handles execution of both regular and shadow transactions.
#[derive(Debug)]
pub struct IrysBlockExecutor<'a, Evm> {
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

    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        on_result_f: impl FnOnce(
            &ExecutionResult<<Self::Evm as Evm>::HaltReason>,
        ) -> reth_evm::block::CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        self.inner
            .execute_transaction_with_commit_condition(tx, on_result_f)
    }

    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<Receipt>), BlockExecutionError> {
        self.inner.finish()
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
#[derive(Debug, Clone)]
pub struct IrysBlockExecutorFactory {
    inner: EthBlockExecutorFactory<RethReceiptBuilder, Arc<ChainSpec>, IrysEvmFactory>,
}

impl IrysBlockExecutorFactory {
    /// Creates a new [`EthBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`RethReceiptBuilder`].
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

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysEvmFactory {
    context: PdContext,
}

impl IrysEvmFactory {
    pub fn new(chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>) -> Self {
        let context = PdContext::new(chunk_provider);
        Self { context }
    }

    /// Provides access to the PdContext for test setup.
    #[cfg(test)]
    pub fn context(&self) -> &PdContext {
        &self.context
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
        let mut evm = IrysEvm::new(
            revm::Context::mainnet()
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_db(db)
                .build_mainnet_with_inspector(NoOpInspector {})
                .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                    PrecompileSpecId::from_spec_id(spec_id),
                ))),
            false,
            self.context.clone_for_new_evm(),
        );

        // Register Irys custom precompiles depending on active hardfork (Frontier+).
        let pd_context = evm.pd_context().clone();
        register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context);

        evm
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        let mut evm = IrysEvm::new(
            revm::Context::mainnet()
                .with_block(input.block_env)
                .with_cfg(input.cfg_env)
                .with_db(db)
                .build_mainnet_with_inspector(inspector)
                .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                    PrecompileSpecId::from_spec_id(spec_id),
                ))),
            true,
            self.context.clone_for_new_evm(),
        );

        // Register Irys custom precompiles depending on active hardfork (Frontier+).
        let pd_context = evm.pd_context().clone();
        register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context);

        evm
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
    // Shared context for accessing Irys data
    context: PdContext,
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
        context: PdContext,
    ) -> Self {
        Self {
            inner: evm,
            inspect,
            state: Default::default(),
            context,
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

    /// Provides a reference to the PD context.
    pub const fn pd_context(&self) -> &PdContext {
        &self.context
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
        // Reject blob-carrying transactions (EIP-4844) at execution time.
        // We keep Cancun active but explicitly disable blobs/sidecars.
        if !tx.blob_hashes.is_empty()
            || tx.max_fee_per_blob_gas != 0
            || tx.tx_type == EIP4844_TX_TYPE_ID
        {
            tracing::debug!(
                tx.blob_hashes_len = tx.blob_hashes.len(),
                tx.max_fee_per_blob_gas = tx.max_fee_per_blob_gas,
                tx.tx_type = tx.tx_type,
                "Rejecting blob-carrying transaction: EIP-4844 not supported"
            );
            return Err(<Self as Evm>::Error::Transaction(
                InvalidTransaction::Eip4844NotSupported,
            ));
        }

        // run this tx through our processing first, if it's not a shadow tx we return here and pass it on
        let tx = match self.process_shadow_tx(tx)? {
            Either::Left(res) => return Ok(res),
            Either::Right(tx) => tx,
        };

        // Update EVM's PdContext with transaction access_list for PD precompile
        // Each EVM has its own PdContext (via clone), so this is thread-safe
        let access_list = tx.access_list.to_vec();
        self.context.update_access_list(access_list);
        tracing::debug!(
            access_list_items = tx.access_list.len(),
            "Updated PdContext with transaction access_list for this EVM instance"
        );

        // Detect PD metadata header in calldata and handle it specially (strip + charge fees).
        // Layout: [magic][version:u16][borsh header][rest]
        if let Some((pd_header, consumed)) =
            crate::pd_tx::detect_and_decode_pd_header(&tx.data).expect("pd header parse error")
        {
            // Compute PD fees based on header values. Always derive PD chunk count from access list.
            let chunks_u64 = crate::pd_tx::sum_pd_chunks_in_access_list(&tx.access_list);
            let chunks = U256::from(chunks_u64);
            // Read the actual PD base fee from EVM state and cap by user's max.
            let actual_per_chunk = self.read_pd_base_fee_per_chunk();
            let base_per_chunk = if actual_per_chunk > pd_header.max_base_fee_per_chunk {
                // TODO: reject the tx!!!
                pd_header.max_base_fee_per_chunk
            } else {
                actual_per_chunk
            };
            let prio_per_chunk = pd_header.max_priority_fee_per_chunk;

            let base_total = chunks.saturating_mul(base_per_chunk);
            let prio_total = chunks.saturating_mul(prio_per_chunk);

            // Fee payer is the transaction caller; priority recipient is block beneficiary.
            let fee_payer = tx.caller;
            let beneficiary = self.block().beneficiary;
            // TODO: Burn/treasury sink placeholder. Ideally we bring the treasury into EVM state.
            // that will also make accounting easier on irys side
            let treasury = Address::ZERO;

            // Strip the PD header from calldata before executing the tx logic.
            let stripped: Bytes = tx.data.slice(consumed..);
            let mut tx = tx;
            tx.data = stripped;

            let _checkpoint = self.inner.ctx.journaled_state.checkpoint();
            {
                // deduct fees from payer
                // TODO: if user does not have enough fees then we extract all the funds we can from him
                if let Some(_err) = self.inner.ctx.journaled_state.inner.transfer(
                    &mut self.inner.ctx.journaled_state.database,
                    fee_payer,
                    treasury,
                    base_total,
                )? {
                    return Err(EVMError::Custom(
                        "insufficient balance for PD base fees".to_string(),
                    ));
                }
                if let Some(_err) = self.inner.ctx.journaled_state.inner.transfer(
                    &mut self.inner.ctx.journaled_state.database,
                    fee_payer,
                    beneficiary,
                    prio_total,
                )? {
                    return Err(EVMError::Custom(
                        "insufficient balance for PD priority fees".to_string(),
                    ));
                }
            }
            self.inner.ctx.journaled_state.checkpoint_commit();

            let res = if self.inspect {
                self.inner.set_tx(tx);
                self.inner.inspect_replay()
            } else {
                self.inner.transact(tx)
            }?;

            return Ok(res);
        }

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

        // Attempt unified shadow-tx detection + decoding
        let to_address = match tx.kind {
            TxKind::Create => return Ok(Either::Right(tx)),
            TxKind::Call(address) => address,
        };
        let tx_envelope_input_buf = &tx.data;

        let shadow_tx = match shadow_tx::detect_and_decode_from_parts(
            Some(to_address),
            tx_envelope_input_buf,
        ) {
            Ok(Some(tx)) => tx,
            Ok(None) => {
                trace!("Not a shadow tx");
                return Ok(Either::Right(tx));
            }
            Err(e) => {
                return Err(Self::create_internal_error(format!(
                    "failed to decode shadow tx: {e}"
                )));
            }
        };

        tracing::trace!("executing shadow transaction");

        // Calculate and distribute priority fee to beneficiary BEFORE executing shadow tx
        // note: gas_priority_fee is `max_priority_fee_per_gas`, remapped in `impl FromRecoveredTx<TxEip1559> for TxEnv` in alloy-revm
        // no idea why the name is different though.
        let priority_fee = tx.gas_priority_fee.unwrap_or(0);
        let total_fee = U256::from(priority_fee);

        // Get the target address from the shadow transaction
        let fee_payer_address = match &shadow_tx {
            ShadowTransaction::V1 { packet, .. } => packet.fee_payer_address(),
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
                tx.priority_fee = %total_fee,
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

        self.distribute_priority_fee(total_fee, fee_payer_address, beneficiary)?;

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
                if state_acc.status != account.status {
                    warn!("Potentially invalid account status flags: from commit {:?}, from prev: {:?}", &state_acc.status, &account.status);
                }

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
            // create the initial frame
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

            // inject the valid frame as the frame result
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

        // Early return for same-address transfer (no-op)
        if target == beneficiary {
            // Validate account exists (but don't check balance since no transfer occurs)
            let account_state = self.load_account(target)?;
            if account_state.is_none() {
                return Err(Self::create_internal_error(format!(
                    "Shadow transaction priority fee failed: target account does not exist for same-address transfer. Target: {target}, Fee: {total_fee}"
                )));
            }

            tracing::trace!(
                fee.address = %target,
                fee.target = %target,
                fee.total_fee = %total_fee,
                "Priority fee payer and beneficiary are the same address, no-op transfer"
            );
            return Ok(());
        }

        tracing::debug!(
            fee.beneficiary = %beneficiary,
            fee.target = %target,
            fee.total_fee = %total_fee,
            "Distributing priority fee"
        );

        // Prepare fee transfer
        let (target_state, beneficiary_state) =
            self.prepare_fee_transfer(target, beneficiary, total_fee)?;

        let bef = self.load_account(beneficiary)?.unwrap();

        self.commit_account_change(target, target_state);
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
    ) -> Result<(Account, Account), <Self as Evm>::Error> {
        // Deduct fee from target
        let mut target_state = self.deduct_fee_from_target(target, total_fee)?;
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
    /// - Returns an internal error if target account doesn't exist
    /// - Returns an internal error if insufficient balance
    fn deduct_fee_from_target(
        &mut self,
        target: Address,
        fee: U256,
    ) -> Result<Account, <Self as Evm>::Error> {
        // Load target account
        let target_state = self.load_account(target)?;

        // Validate account exists
        let Some(mut account) = target_state else {
            tracing::warn!(
                fee.target = %target,
                "Target account does not exist, cannot deduct priority fee"
            );
            return Err(Self::create_internal_error(format!(
                "Shadow transaction priority fee failed: target account does not exist. Target: {target}, Required fee: {fee}"
            )));
        };

        // Validate sufficient balance
        if account.info.balance < fee {
            tracing::warn!(
                fee.target = %target,
                account.balance = %account.info.balance,
                fee.required = %fee,
                "Target has insufficient balance for priority fee"
            );
            return Err(Self::create_internal_error(format!(
                "Shadow transaction priority fee failed: insufficient balance. Target: {}, Required fee: {}, Available balance: {}",
                target,
                fee,
                account.info.balance
            )));
        }

        // Deduct fee
        account.info.balance = account.info.balance.saturating_sub(fee);

        tracing::trace!(
            fee.target = %target,
            fee.amount = %fee,
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
                fee.beneficiary = %beneficiary,
                account.original_balance = %original_balance,
                account.new_balance = %account.info.balance,
                account.fee_amount = %fee,
                "Incrementing beneficiary balance with priority fee"
            );

            Ok((account, false))
        } else {
            // Create new account
            let mut account = Account::new_not_existing();

            account.info.balance = fee;

            tracing::trace!(
                account.beneficiary = %beneficiary,
                account.balance = %fee,
                "Creating new beneficiary account with priority fee"
            );

            Ok((account, true))
        }
    }

    /// Reads the PD base fee per chunk from the dedicated EVM account's balance.
    /// Returns 0 if the account does not exist.
    fn read_pd_base_fee_per_chunk(&mut self) -> U256 {
        match self.load_account(*PD_BASE_FEE_ACCOUNT) {
            Ok(Some(acc)) => acc.info.balance,
            _ => U256::ZERO,
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
                shadow_tx::TransactionPacket::UnstakeRefund(balance_increment) => {
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
                shadow_tx::TransactionPacket::UnstakeDebit(unstake_debit) => {
                    // Fee-only via priority fee (already processed). Emit a log only.
                    let log = Self::create_shadow_log(
                        unstake_debit.target,
                        vec![topic],
                        vec![DynSolValue::Address(unstake_debit.target)],
                    );
                    let target = unstake_debit.target;
                    let execution_result = Self::create_success_result(log);
                    Ok((Err(execution_result), target))
                }
                shadow_tx::TransactionPacket::Unpledge(unpledge_debit) => {
                    // Fee-only via priority fee (already processed). Emit a log only.
                    let log = Self::create_shadow_log(
                        unpledge_debit.target,
                        vec![topic],
                        vec![DynSolValue::Address(unpledge_debit.target)],
                    );
                    let target = unpledge_debit.target;
                    let execution_result = Self::create_success_result(log);
                    Ok((Err(execution_result), target))
                }
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
                | shadow_tx::TransactionPacket::IngressProofReward(balance_increment)
                | shadow_tx::TransactionPacket::PermFeeRefund(balance_increment)
                | shadow_tx::TransactionPacket::UnpledgeRefund(balance_increment) => {
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
                shadow_tx::TransactionPacket::PdBaseFeeUpdate(update) => {
                    // Write the per-chunk base fee into the PD base fee account balance.
                    let target = *PD_BASE_FEE_ACCOUNT;

                    // Load existing or create new account
                    let existed;
                    let mut account = if let Some(acc) = self.load_account(target)? {
                        existed = true;
                        acc
                    } else {
                        existed = false;
                        Account::new_not_existing()
                    };
                    account.info.balance = update.per_chunk;

                    let log = Self::create_shadow_log(
                        target,
                        vec![topic],
                        vec![DynSolValue::Uint(update.per_chunk, 256)],
                    );
                    let execution_result = Self::create_success_result(log);
                    Ok((Ok((account, execution_result, existed)), target))
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
                account.target_address = %balance_increment.target,
                account.original_balance = %original_balance,
                account.increment_amount = %balance_increment.amount,
                account.final_balance = %account.info.balance,
                "Balance increment on existing account"
            );

            account
        } else {
            // Create a new account with the incremented balance
            let mut account = Account::new_not_existing();
            account.info.balance = balance_increment.amount;

            tracing::debug!(
                account.target_address = %balance_increment.target,
                account.increment_amount = %balance_increment.amount,
                account.final_balance = %account.info.balance,
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
            return Err(Self::create_internal_error(format!(
                "Shadow transaction failed: insufficient balance for decrement. Target: {}, Required: {}, Available: {}, Reference: {:?}",
                balance_decrement.target,
                balance_decrement.amount,
                new_account_info.info.balance,
                balance_decrement.irys_ref
            )));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pd_tx::{prepend_pd_header_v1_to_calldata, PdHeaderV1};
    use crate::shadow_tx::{PdBaseFeeUpdate, ShadowTransaction, TransactionPacket};
    use crate::test_utils::{
        advance_block, get_balance, get_nonce, sign_shadow_tx, sign_tx, TestContext,
        DEFAULT_PRIORITY_FEE,
    };
    use alloy_consensus::TxEip1559;
    use alloy_eips::eip2930::AccessListItem as AlItem;
    use alloy_primitives::{Address, B256, U256};
    use alloy_primitives::{Bytes, FixedBytes};
    use reth::rpc::api::eth::EthApiServer as _;
    use reth_evm::EvmEnv;
    use reth_provider::ReceiptProvider as _;

    use reth_transaction_pool::{PoolTransaction as _, TransactionOrigin, TransactionPool as _};
    use revm::context::result::{EVMError, InvalidTransaction};
    use revm::context::{BlockEnv, CfgEnv, TxEnv};
    use revm::database_interface::EmptyDB;

    fn tx_request_base() -> alloy_rpc_types::TransactionRequest {
        alloy_rpc_types::TransactionRequest {
            chain_id: Some(1),
            max_fee_per_gas: Some(1_000_000_000),
            max_priority_fee_per_gas: Some(1_000_000),
            gas: Some(100_000),
            ..Default::default()
        }
    }

    fn tx_eip1559_base() -> TxEip1559 {
        TxEip1559 {
            chain_id: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            to: TxKind::Call(Address::random()),
            value: U256::ZERO,
            nonce: 0,
            access_list: Default::default(),
            input: Bytes::new(),
        }
    }

    /// Helper to build PD access list with N chunks
    fn build_pd_access_list(num_chunks: usize) -> alloy_eips::eip2930::AccessList {
        use alloy_primitives::aliases::U200;
        use irys_types::range_specifier::{ChunkRangeSpecifier, PdAccessListArgSerde as _};
        let keys: Vec<_> = (0..num_chunks)
            .map(|i| {
                let spec = ChunkRangeSpecifier {
                    partition_index: U200::ZERO,
                    offset: i as u32,
                    chunk_count: 1,
                };
                B256::from(spec.encode())
            })
            .collect();
        alloy_eips::eip2930::AccessList(vec![AlItem {
            address: irys_types::precompile::PD_PRECOMPILE_ADDRESS,
            storage_keys: keys,
        }])
    }

    /// Ensure EVM layer rejects EIP-4844 blob-carrying transactions regardless of mempool filters.
    #[test]
    fn evm_rejects_eip4844_blob_fields_in_transact_raw() {
        // Build minimal EVM env with Cancun spec enabled
        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        cfg_env.chain_id = 1;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        // Create a TxEnv that carries EIP-4844 blob fields
        let tx = TxEnv {
            caller: Address::random(),
            kind: TxKind::Call(Address::random()),
            nonce: 0,
            gas_limit: 21_000,
            gas_price: 0,
            value: U256::ZERO,
            chain_id: Some(1),
            blob_hashes: vec![B256::ZERO],
            max_fee_per_blob_gas: 1,
            ..TxEnv::default()
        };

        let res = evm.transact_raw(tx);
        assert!(
            matches!(
                res,
                Err(EVMError::Transaction(
                    InvalidTransaction::Eip4844NotSupported
                ))
            ),
            "expected EIP-4844 not supported, got: {:?}",
            res
        );
    }

    /// Ensure a regular non-shadow, non-blob transaction executes successfully at the EVM layer.
    #[test]
    fn evm_processes_normal_tx_success() {
        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new(mock_chunk_provider);

        // Cancun spec, chain id 1, zero basefee and ample gas limit
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        cfg_env.chain_id = 1;
        let block_env = BlockEnv {
            gas_limit: 30_000_000,
            basefee: 0,
            ..Default::default()
        };

        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        // Simple call with zero value and zero gas price
        let tx = TxEnv {
            caller: Address::random(),
            kind: TxKind::Call(Address::random()),
            nonce: 0,
            gas_limit: 21_000,
            value: U256::ZERO,
            data: Bytes::new(),
            gas_price: 0,
            chain_id: Some(1),
            gas_priority_fee: None,
            access_list: Default::default(),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: 0,
            authorization_list: Default::default(),
        };

        let res = evm.transact_raw(tx);
        assert!(res.is_ok(), "expected Ok, got: {:?}", res);
        let result = res.unwrap().result;
        assert!(result.is_success(), "expected success, got: {:?}", result);
    }

    /// Ensure a PD-shaped transaction with header charges base and priority fees end-to-end via node execution.
    /// Tests with different transfer values (0, 1, 2, 3) to ensure proper accounting across all scenarios.
    #[rstest::rstest]
    #[case(0)]
    #[case(1)]
    #[case(2)]
    #[case(3)]
    #[test_log::test(tokio::test)]
    async fn evm_pd_header_tx_charges_fees(#[case] transfer_value: u64) -> eyre::Result<()> {
        // Spin up a single node and set PD base fee via shadow transaction
        let ctx = TestContext::new().await?;
        let ((mut node, shadow_tx_store), ctx) = ctx.get_single_node()?;

        let payer = ctx.normal_signer.address();
        let beneficiary = ctx.block_producer_a.address();
        let treasury = Address::ZERO;

        let pd_base_fee = U256::from(7_u64);
        let solution_hash = FixedBytes::<32>::from_slice(&[0x11; 32]);
        let pd_fee_update = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: pd_base_fee,
            }),
            solution_hash,
        );
        // Priority fee ignored for PdBaseFeeUpdate (no payer address)
        let pd_fee_update_signed =
            sign_shadow_tx(pd_fee_update, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        advance_block(&mut node, &shadow_tx_store, vec![pd_fee_update_signed]).await?;

        // Capture initial balances and nonce
        let payer_initial = get_balance(&node.inner, payer);
        let beneficiary_initial = get_balance(&node.inner, beneficiary);
        let sink_initial = get_balance(&node.inner, treasury);
        let payer_initial_nonce = get_nonce(&node.inner, payer);

        // Build and submit a PD-header transaction: 3 chunks, prio=10, base<=7
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: U256::from(10_u64),
            max_base_fee_per_chunk: U256::from(7_u64),
        };
        let user_calldata = Bytes::from(vec![0xAA, 0xBB]);
        let input = prepend_pd_header_v1_to_calldata(&header, &user_calldata);

        // PD access list: 3 storage keys at PD precompile address
        use alloy_primitives::aliases::U200;
        use irys_types::range_specifier::{ChunkRangeSpecifier, PdAccessListArgSerde as _};

        let make_key = |offset: u32| {
            let spec = ChunkRangeSpecifier {
                partition_index: U200::ZERO,
                offset,
                chunk_count: 1,
            };
            B256::from(spec.encode())
        };

        let key1 = make_key(0);
        let key2 = make_key(1);
        let key3 = make_key(2);
        let access_list = alloy_eips::eip2930::AccessList(vec![AlItem {
            address: irys_types::precompile::PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![key1, key2, key3],
        }]);

        // Compose EIP-1559 tx with zero priority gas fee so only PD fees affect balances
        let value = U256::from(transfer_value);
        let tx_raw = TxEip1559 {
            access_list,
            input,
            max_priority_fee_per_gas: 0,
            nonce: payer_initial_nonce,
            value,
            ..tx_eip1559_base()
        };
        let pooled = sign_tx(tx_raw, &ctx.normal_signer).await;
        let tx_hash = *pooled.hash();
        let _add_result = node
            .inner
            .pool
            .add_transaction(TransactionOrigin::Local, pooled)
            .await?;

        // Mine a block including the PD-header tx
        let payload = advance_block(&mut node, &shadow_tx_store, vec![]).await?;

        // Query the receipt to get actual gas used
        let receipt = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .receipt_by_hash(tx_hash)
            .unwrap()
            .expect("Receipt should exist for mined transaction");
        let gas_used = receipt.cumulative_gas_used;

        // Get the block basefee
        let block = payload.block();
        let block_basefee = block.header().base_fee_per_gas.unwrap_or(0);

        // Calculate PD fees
        let pd_base_total = U256::from(3_u64) * pd_base_fee; // 3 chunks * 7 = 21
        let pd_prio_total = U256::from(3_u64) * U256::from(10_u64); // 3 chunks * 10 = 30

        // Calculate EVM gas costs
        // Note: max_priority_fee_per_gas = 0 in this test, so only base fee matters
        let evm_base_cost = U256::from(gas_used) * U256::from(block_basefee);
        let evm_priority_cost = U256::ZERO; // max_priority_fee_per_gas is 0

        // Total expected deduction: PD fees + EVM gas + tx.value
        let expected_total =
            pd_base_total + pd_prio_total + evm_base_cost + evm_priority_cost + value;

        // Validate payer balance deduction
        let payer_final = get_balance(&node.inner, payer);
        let deducted = payer_initial.saturating_sub(payer_final);
        assert_eq!(
            deducted,
            expected_total,
            "Payer balance deduction mismatch. Expected: {} (PD base: {} + PD prio: {} + EVM base: {} + EVM prio: {}), Got: {}",
            expected_total,
            pd_base_total,
            pd_prio_total,
            evm_base_cost,
            evm_priority_cost,
            deducted
        );

        // Beneficiary should receive: PD priority fees + EVM priority tips
        // In this test, EVM priority tip = 0, so only PD priority fees
        let beneficiary_final = get_balance(&node.inner, beneficiary);
        let expected_beneficiary_gain = pd_prio_total + evm_priority_cost;
        assert_eq!(
            beneficiary_final,
            beneficiary_initial + expected_beneficiary_gain,
            "Beneficiary balance mismatch. Expected gain: {} (PD prio: {} + EVM prio: {}), Got: {}",
            expected_beneficiary_gain,
            pd_prio_total,
            evm_priority_cost,
            beneficiary_final.saturating_sub(beneficiary_initial)
        );

        // Sink should receive: PD base fees
        let sink_final = get_balance(&node.inner, treasury);
        let expected_sink_gain = pd_base_total;
        assert_eq!(
            sink_final,
            sink_initial + expected_sink_gain,
            "Sink balance mismatch. Expected gain: {} (PD base: {} + EVM base: {}), Got: {}",
            expected_sink_gain,
            pd_base_total,
            evm_base_cost,
            sink_final.saturating_sub(sink_initial)
        );

        // Verify nonce was incremented
        let payer_final_nonce = get_nonce(&node.inner, payer);
        assert_eq!(
            payer_final_nonce,
            payer_initial_nonce + 1,
            "Nonce should increment after transaction execution. Expected: {}, Got: {}",
            payer_initial_nonce + 1,
            payer_final_nonce
        );

        Ok(())
    }

    /// Validates that batch simulation via eth_simulateV1 does not modify chain state.
    ///
    /// This test simulates a PD transaction using the batch simulation API and verifies
    /// that account balances and nonces remain unchanged after simulation completes.
    #[test_log::test(tokio::test)]
    async fn test_pd_tx_batch_simulation_doesnt_modify_state() -> eyre::Result<()> {
        use alloy_rpc_types::simulate::{SimBlock, SimulatePayload};

        // Spin up a single node and set PD base fee
        let ctx = TestContext::new().await?;
        let ((mut node, shadow_tx_store), ctx) = ctx.get_single_node()?;

        let payer = ctx.normal_signer.address();

        // Set PD base fee to a known value
        let pd_base_fee = U256::from(100_u64);
        let solution_hash = FixedBytes::<32>::from_slice(&[0x44; 32]);
        let pd_fee_update = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: pd_base_fee,
            }),
            solution_hash,
        );
        let pd_fee_update_signed =
            sign_shadow_tx(pd_fee_update, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        advance_block(&mut node, &shadow_tx_store, vec![pd_fee_update_signed]).await?;

        // Capture pre-simulation state
        let balance_before = get_balance(&node.inner, payer);
        let nonce_before = get_nonce(&node.inner, payer);
        println!(
            "Pre-simulation: balance={}, nonce={}",
            balance_before, nonce_before
        );

        // Build single PD transaction with 2 chunks
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: U256::from(50_u64),
            max_base_fee_per_chunk: U256::from(100_u64),
        };
        let input = prepend_pd_header_v1_to_calldata(&header, &Bytes::from(vec![0xAA]));

        let tx_request = alloy_rpc_types::TransactionRequest {
            from: Some(payer),
            to: Some(TxKind::Call(Address::random())),
            value: Some(U256::from(100_u64)),
            input: alloy_rpc_types::TransactionInput::new(input),
            nonce: Some(nonce_before),
            access_list: Some(build_pd_access_list(2)),
            ..tx_request_base()
        };

        // Build simulation payload
        let payload = SimulatePayload {
            block_state_calls: vec![SimBlock {
                block_overrides: None,
                state_overrides: None,
                calls: vec![tx_request],
            }],
            trace_transfers: true,
            validation: true,
            return_full_transactions: false,
        };

        // Get the eth API and simulate
        let eth_api = node.rpc.inner.eth_api();
        let simulation_result = eth_api
            .simulate_v1(payload, Some(alloy_rpc_types::BlockId::latest()))
            .await;

        // Validate simulation succeeded
        let simulated_blocks = simulation_result?;
        assert_eq!(simulated_blocks.len(), 1, "Expected 1 simulated block");
        assert_eq!(simulated_blocks[0].calls.len(), 1, "Expected 1 call result");
        assert!(
            simulated_blocks[0].calls[0].status,
            "Simulation should succeed"
        );

        // Verify state unchanged
        let balance_after = get_balance(&node.inner, payer);
        let nonce_after = get_nonce(&node.inner, payer);
        println!(
            "Post-simulation: balance={}, nonce={}",
            balance_after, nonce_after
        );

        assert_eq!(
            balance_before, balance_after,
            "Balance changed after simulation"
        );
        assert_eq!(nonce_before, nonce_after, "Nonce changed after simulation");

        Ok(())
    }

    /// Validates that PD fees are deducted even when the transaction execution fails.
    ///
    /// This test ensures that PD fees are charged upfront regardless of transaction success:
    /// 1. Creates a PD transaction that calls a non-existent contract (will fail)
    /// 2. Executes the transaction in a block (not simulation)
    /// 3. Verifies the transaction failed
    /// 4. Verifies PD fees were still deducted from payer
    /// 5. Verifies PD fees were distributed to beneficiary and treasury
    /// 6. Verifies nonce was incremented (even failed txs increment nonce)
    ///
    /// This is critical for the protocol's fee model - PD fees must be charged
    /// regardless of execution outcome to prevent spam attacks.
    #[test_log::test(tokio::test)]
    async fn test_pd_tx_failed_execution_still_charges_fees() -> eyre::Result<()> {
        // Spin up a single node and set PD base fee
        let ctx = TestContext::new().await?;
        let ((mut node, shadow_tx_store), ctx) = ctx.get_single_node()?;

        let payer = ctx.normal_signer.address();
        let beneficiary = ctx.block_producer_a.address();
        let treasury = Address::ZERO;

        // Set PD base fee to a known value
        let pd_base_fee = U256::from(100_u64);
        let solution_hash = FixedBytes::<32>::from_slice(&[0x44; 32]);
        let pd_fee_update = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: pd_base_fee,
            }),
            solution_hash,
        );
        let pd_fee_update_signed =
            sign_shadow_tx(pd_fee_update, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;
        advance_block(&mut node, &shadow_tx_store, vec![pd_fee_update_signed]).await?;

        // Capture initial state
        let payer_initial_balance = get_balance(&node.inner, payer);
        let payer_initial_nonce = get_nonce(&node.inner, payer);
        let beneficiary_initial_balance = get_balance(&node.inner, beneficiary);
        let treasury_initial_balance = get_balance(&node.inner, treasury);

        println!(
            "Initial: payer={}, beneficiary={}, treasury={}, nonce={}",
            payer_initial_balance,
            beneficiary_initial_balance,
            treasury_initial_balance,
            payer_initial_nonce
        );

        // Build PD transaction header (3 chunks with fees)
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: U256::from(50_u64),
            max_base_fee_per_chunk: U256::from(100_u64),
        };

        // Reverting initcode: PUSH1 0 PUSH1 0 REVERT (0x60006000fd)
        let reverting_initcode = Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd]);
        let input = prepend_pd_header_v1_to_calldata(&header, &reverting_initcode);

        let access_list = build_pd_access_list(3);

        // CREATE transaction with reverting init code (will fail)
        let nonce = payer_initial_nonce;
        let tx_raw = TxEip1559 {
            access_list,
            input,
            max_priority_fee_per_gas: 0, // Zero priority fee to simplify calculations
            nonce,
            to: TxKind::Create,
            ..tx_eip1559_base()
        };
        let pooled = sign_tx(tx_raw, &ctx.normal_signer).await;
        let tx_hash = *pooled.hash();

        // Submit transaction to mempool
        let _add_result = node
            .inner
            .pool
            .add_transaction(TransactionOrigin::Local, pooled)
            .await?;

        // Mine block including the transaction
        let payload = advance_block(&mut node, &shadow_tx_store, vec![]).await?;

        // Query the receipt
        let receipt = node
            .inner
            .provider
            .consistent_provider()
            .unwrap()
            .receipt_by_hash(tx_hash)
            .unwrap()
            .expect("Receipt should exist for mined transaction");

        let gas_used = receipt.cumulative_gas_used;
        let tx_success = receipt.success;
        println!("TX failed: {}, gas_used: {}", !tx_success, gas_used);

        // CRITICAL ASSERTION: Transaction should have FAILED
        assert!(
            !tx_success,
            "Transaction should have failed due to reverting init code in CREATE transaction"
        );

        // Get block basefee for gas cost calculation
        let block = payload.block();
        let block_basefee = block.header().base_fee_per_gas.unwrap_or(0);

        // Calculate expected PD fees
        let num_chunks = 3_u64;
        let pd_base_total = U256::from(num_chunks) * pd_base_fee; // 3 * 100 = 300
        let pd_prio_total = U256::from(num_chunks) * U256::from(50_u64); // 3 * 50 = 150

        // Calculate EVM gas costs
        let evm_base_cost = U256::from(gas_used) * U256::from(block_basefee);
        let evm_priority_cost = U256::ZERO; // max_priority_fee_per_gas is 0

        // Note: Failed transactions don't transfer value but still charge fees
        let expected_total_deduction =
            pd_base_total + pd_prio_total + evm_base_cost + evm_priority_cost;

        // Capture final state
        let payer_final_balance = get_balance(&node.inner, payer);
        let payer_final_nonce = get_nonce(&node.inner, payer);
        let beneficiary_final_balance = get_balance(&node.inner, beneficiary);
        let treasury_final_balance = get_balance(&node.inner, treasury);

        let actual_deduction = payer_initial_balance.saturating_sub(payer_final_balance);

        println!(
            "Final: payer={}, beneficiary={}, treasury={}, nonce={}",
            payer_final_balance,
            beneficiary_final_balance,
            treasury_final_balance,
            payer_final_nonce
        );
        println!(
            "Fees: PD_base={}, PD_prio={}, EVM={}, expected={}, actual={}",
            pd_base_total, pd_prio_total, evm_base_cost, expected_total_deduction, actual_deduction
        );

        // Verify fees charged correctly despite transaction failure
        assert_eq!(
            actual_deduction, expected_total_deduction,
            "Payer deduction: expected {}, got {}",
            expected_total_deduction, actual_deduction
        );

        let beneficiary_gain =
            beneficiary_final_balance.saturating_sub(beneficiary_initial_balance);
        assert_eq!(
            beneficiary_gain, pd_prio_total,
            "Beneficiary gain: expected {}, got {}",
            pd_prio_total, beneficiary_gain
        );

        let treasury_gain = treasury_final_balance.saturating_sub(treasury_initial_balance);
        assert_eq!(
            treasury_gain, pd_base_total,
            "Treasury gain: expected {}, got {}",
            pd_base_total, treasury_gain
        );

        assert_eq!(
            payer_final_nonce,
            payer_initial_nonce + 1,
            "Nonce should increment despite failure"
        );

        println!("Fees charged correctly despite failure");

        Ok(())
    }
}
