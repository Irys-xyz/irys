// Standard library imports
use core::convert::Infallible;

use alloy_rpc_types_engine::ExecutionData;
use either::Either;

use alloy_consensus::{Block, Header};
use alloy_dyn_abi::DynSolValue;
use alloy_eips::eip2718::EIP4844_TX_TYPE_ID;
use alloy_evm::block::{BlockExecutionError, BlockExecutor, ExecutableTx, OnStateHook};
use alloy_evm::eth::EthBlockExecutor;
use alloy_evm::{Database, Evm, FromRecoveredTx, FromTxWithEncoded};
use alloy_primitives::{keccak256, Address, Bytes, FixedBytes, Log, LogData, U256};
use std::sync::LazyLock;

use reth::primitives::{SealedBlock, SealedHeader};
use reth::providers::BlockExecutionResult;
use reth::revm::context::TxEnv;
use reth::revm::context::result::ExecutionResult;
use reth::revm::primitives::hardfork::SpecId;
use reth::revm::{Inspector, State};
use reth_ethereum_primitives::Receipt;
use reth_evm::block::{BlockExecutorFactory, BlockExecutorFor};
use reth_evm::eth::{EthBlockExecutionCtx, EthBlockExecutorFactory, EthEvmContext};
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_evm::precompiles::PrecompilesMap;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EthEvmFactory, EvmEnv, EvmFactory, NextBlockEnvAttributes,
};
use reth_evm_ethereum::{EthBlockAssembler, RethReceiptBuilder};

// External crate imports - Revm
use revm::context::result::{EVMError, HaltReason, InvalidTransaction, Output};
use revm::context::{BlockEnv, CfgEnv, ContextTr as _};
use revm::handler::EthFrame;
use revm::inspector::NoOpInspector;
use revm::precompile::{PrecompileSpecId, Precompiles};
use revm::state::{Account, AccountStatus};
use revm::{MainBuilder as _, MainContext as _};
use tracing::trace;

// External crate imports - Other
use irys_types::storage_pricing::TOKEN_SCALE;

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

/// Account address used to store the PD base fee per chunk in EVM state.
/// The balance of this account represents the current PD base fee.
pub static PD_BASE_FEE_ACCOUNT: LazyLock<Address> =
    LazyLock::new(|| Address::from_word(keccak256("irys_pd_base_fee_account")));

/// Account address used to store the IRYS/USD price in EVM state.
/// The balance of this account represents the current IRYS token price in USD (1e18 scale).
pub static IRYS_USD_PRICE_ACCOUNT: LazyLock<Address> =
    LazyLock::new(|| Address::from_word(keccak256("irys_usd_price_account")));

/// Account address used to track treasury balance in EVM state.
/// The balance of this account represents the current treasury holdings.
pub static TREASURY_ACCOUNT: LazyLock<Address> =
    LazyLock::new(|| Address::from_word(keccak256("irys_treasury_account")));

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
    type Result = alloy_evm::eth::EthTxResult<E::HaltReason, alloy_consensus::TxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_with_result_closure(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as Evm>::HaltReason>),
    ) -> Result<u64, BlockExecutionError> {
        self.inner.execute_transaction_with_result_closure(tx, f)
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

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        self.inner.commit_transaction(output)
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
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
    pub inner: EthEvmConfig<ChainSpec, EthEvmFactory>,
    pub executor_factory: IrysBlockExecutorFactory,
    pub assembler: IrysBlockAssembler,
}

impl ConfigureEngineEvm<ExecutionData> for IrysEvmConfig {
    fn evm_env_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<reth_evm::EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<reth_evm::ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl reth_evm::ExecutableTxIterator<Self>, Self::Error> {
        self.inner.tx_iterator_for_payload(payload)
    }
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

    fn evm_env(&self, header: &Header) -> Result<EvmEnv, Infallible> {
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
    ) -> Result<EthBlockExecutionCtx<'a>, Infallible> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<EthBlockExecutionCtx<'_>, Infallible> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysEvmFactory {
    context: PdContext,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
}

impl IrysEvmFactory {
    pub fn new(
        chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
        hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    ) -> Self {
        let context = PdContext::new(chunk_provider);
        Self {
            context,
            hardfork_config,
        }
    }

    /// Provides access to the PdContext for test setup.
    #[cfg(test)]
    pub fn context(&self) -> &PdContext {
        &self.context
    }

    /// Provides access to the hardfork config.
    pub fn hardfork_config(&self) -> &irys_types::hardfork_config::IrysHardforkConfig {
        &self.hardfork_config
    }

    /// Creates a new factory for testing with Sprite hardfork enabled from genesis.
    #[cfg(test)]
    pub fn new_for_testing(
        chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    ) -> Self {
        let hardfork_config = Arc::new(irys_types::config::ConsensusConfig::testing().hardforks);
        Self::new(chunk_provider, hardfork_config)
    }
}

impl EvmFactory for IrysEvmFactory {
    type Evm<DB: Database, I: Inspector<EthEvmContext<DB>>> = IrysEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = revm::Context<BlockEnv, TxEnv, CfgEnv, DB>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        let spec_id = input.cfg_env.spec;
        // Check if Sprite hardfork is active based on block timestamp
        let block_timestamp = irys_types::UnixTimestamp::from_secs(input.block_env.timestamp.to());
        let is_sprite_active = self.hardfork_config.is_sprite_active(block_timestamp);

        // Get minimum PD transaction cost if Sprite is active
        // Convert from irys_types::U256 to alloy_primitives::U256 via bytes
        let min_pd_transaction_cost_usd = self
            .hardfork_config
            .min_pd_transaction_cost_at(block_timestamp)
            .map(|amount| U256::from_be_bytes(amount.amount.to_be_bytes()));

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
            is_sprite_active,
            min_pd_transaction_cost_usd,
        );

        // Register Irys custom precompiles only if Sprite hardfork is active.
        if is_sprite_active {
            let pd_context = evm.pd_context().clone();
            register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context);
        }

        evm
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let spec_id = input.cfg_env.spec;
        // Check if Sprite hardfork is active based on block timestamp
        let block_timestamp = irys_types::UnixTimestamp::from_secs(input.block_env.timestamp.to());
        let is_sprite_active = self.hardfork_config.is_sprite_active(block_timestamp);

        // Get minimum PD transaction cost if Sprite is active
        // Convert from irys_types::U256 to alloy_primitives::U256 via bytes
        let min_pd_transaction_cost_usd = self
            .hardfork_config
            .min_pd_transaction_cost_at(block_timestamp)
            .map(|amount| U256::from_be_bytes(amount.amount.to_be_bytes()));

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
            is_sprite_active,
            min_pd_transaction_cost_usd,
        );

        // Register Irys custom precompiles only if Sprite hardfork is active.
        if is_sprite_active {
            let pd_context = evm.pd_context().clone();
            register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context);
        }

        evm
    }
}

use revm::{
    Context, ExecuteEvm as _, InspectEvm as _,
    context::Evm as RevmEvm,
    context_interface::result::ResultAndState,
    handler::{EthPrecompiles, PrecompileProvider, instructions::EthInstructions},
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
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
        EthFrame,
    >,
    inspect: bool,
    state: revm_primitives::map::foldhash::HashMap<Address, Account>,
    // Shared context for accessing Irys data
    context: PdContext,
    // Whether the Sprite hardfork is active (enables PD features)
    is_sprite_active: bool,
    // Minimum PD transaction cost in USD (1e18 scale), None if Sprite not active
    min_pd_transaction_cost_usd: Option<U256>,
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
            EthFrame,
        >,
        inspect: bool,
        context: PdContext,
        is_sprite_active: bool,
        min_pd_transaction_cost_usd: Option<U256>,
    ) -> Self {
        Self {
            inner: evm,
            inspect,
            state: Default::default(),
            context,
            is_sprite_active,
            min_pd_transaction_cost_usd,
        }
    }

    /// Returns whether the Sprite hardfork is active for this EVM instance.
    pub const fn is_sprite_active(&self) -> bool {
        self.is_sprite_active
    }

    /// Consumes self and return the inner EVM instance.
    pub fn into_inner(
        self,
    ) -> RevmEvm<
        EthEvmContext<DB>,
        I,
        EthInstructions<EthInterpreter, EthEvmContext<DB>>,
        PRECOMPILE,
        EthFrame,
    > {
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
    type BlockEnv = BlockEnv;
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

        // PD (Programmable Data) features are only available when Sprite hardfork is active
        if self.is_sprite_active {
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
                // Read the actual PD base fee from EVM state.
                let actual_per_chunk = self.read_pd_base_fee_per_chunk();
                // Reject if actual base fee exceeds user's max (EIP-1559 semantics).
                if actual_per_chunk > pd_header.max_base_fee_per_chunk {
                    tracing::debug!(
                        actual_per_chunk = %actual_per_chunk,
                        max_base_fee_per_chunk = %pd_header.max_base_fee_per_chunk,
                        "PD transaction rejected: actual base fee exceeds user's max"
                    );
                    return Err(EVMError::Transaction(
                        InvalidTransaction::GasPriceLessThanBasefee,
                    ));
                }
                let base_per_chunk = actual_per_chunk;
                let prio_per_chunk = pd_header.max_priority_fee_per_chunk;

                let base_total = chunks.saturating_mul(base_per_chunk);
                let prio_total = chunks.saturating_mul(prio_per_chunk);

                // Validate minimum PD transaction cost
                if let Some(min_cost_usd) = self.min_pd_transaction_cost_usd {
                    let irys_usd_price = self.read_irys_usd_price()?;

                    // Calculate minimum cost in IRYS tokens:
                    // min_cost_irys = (min_cost_usd * TOKEN_SCALE) / irys_usd_price
                    // Both min_cost_usd and irys_usd_price are in TOKEN_SCALE (1e18) scale
                    let scale = U256::from_be_bytes(TOKEN_SCALE.to_be_bytes());
                    let min_cost_irys = min_cost_usd.saturating_mul(scale) / irys_usd_price;

                    // Calculate actual total fees
                    let actual_total_fees = base_total.saturating_add(prio_total);

                    if actual_total_fees < min_cost_irys {
                        tracing::debug!(
                            min_cost_usd = %min_cost_usd,
                            irys_usd_price = %irys_usd_price,
                            min_cost_irys = %min_cost_irys,
                            actual_total_fees = %actual_total_fees,
                            chunks = %chunks,
                            "PD transaction rejected: fees below minimum cost"
                        );
                        // Use InvalidTransaction::GasPriceLessThanBasefee to signal that the
                        // transaction should be skipped during block building (not a fatal error).
                        // This causes the payload builder to skip this tx and continue.
                        // Note: We're repurposing this error type since there's no specific
                        // "PD fee too low" variant in revm.
                        return Err(EVMError::Transaction(
                            InvalidTransaction::GasPriceLessThanBasefee,
                        ));
                    }
                }

                // Fee payer is the transaction caller; priority recipient is block beneficiary.
                let fee_payer = tx.caller;
                let beneficiary = self.block().beneficiary;
                // Pre-Sprite: burn to Address::ZERO, Post-Sprite: send to treasury
                let treasury = if self.is_sprite_active {
                    *TREASURY_ACCOUNT
                } else {
                    Address::ZERO
                };

                // Pre-validate fee_payer has sufficient balance for total PD fees
                let total_pd_fees = base_total.saturating_add(prio_total);
                let fee_payer_account = self
                    .inner
                    .ctx
                    .journaled_state
                    .inner
                    .load_account(&mut self.inner.ctx.journaled_state.database, fee_payer)?;
                let fee_payer_balance = fee_payer_account.data.info.balance;

                if fee_payer_balance < total_pd_fees {
                    tracing::debug!(
                        fee_payer = %fee_payer,
                        required = %total_pd_fees,
                        available = %fee_payer_balance,
                        "PD transaction rejected: insufficient balance for PD fees"
                    );
                    return Err(EVMError::Transaction(
                        InvalidTransaction::LackOfFundForMaxFee {
                            fee: Box::new(total_pd_fees),
                            balance: Box::new(fee_payer_balance),
                        },
                    ));
                }

                // Strip the PD header from calldata before executing the tx logic.
                let stripped: Bytes = tx.data.slice(consumed..);
                let mut tx = tx;
                tx.data = stripped;

                let _checkpoint = self.inner.ctx.journaled_state.checkpoint();
                {
                    // Deduct fees from payer. Balance was pre-validated above, so OutOfFunds
                    // should not occur. OverflowPayment is theoretically possible but extremely
                    // unlikely (would require recipient balance near U256::MAX).
                    if let Some(err) = self.inner.ctx.journaled_state.inner.transfer(
                        &mut self.inner.ctx.journaled_state.database,
                        fee_payer,
                        treasury,
                        base_total,
                    )? {
                        return Err(EVMError::Custom(format!(
                            "PD base fee transfer failed unexpectedly: {err:?}"
                        )));
                    }
                    if let Some(err) = self.inner.ctx.journaled_state.inner.transfer(
                        &mut self.inner.ctx.journaled_state.database,
                        fee_payer,
                        beneficiary,
                        prio_total,
                    )? {
                        return Err(EVMError::Custom(format!(
                            "PD priority fee transfer failed unexpectedly: {err:?}"
                        )));
                    }
                }
                self.inner.ctx.journaled_state.checkpoint_commit();

                let res = if self.inspect {
                    self.inner.inspect_tx(tx)
                } else {
                    self.inner.transact(tx)
                }?;

                return Ok(res);
            }
        }

        if self.inspect {
            self.inner.inspect_tx(tx)
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

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.ctx.journaled_state.database,
            &self.inner.inspector,
            &self.inner.precompiles,
        )
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
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
            Ok((account, execution_result, _account_existed)) => {
                self.commit_account_change(target, account);

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
            state: std::mem::take(&mut self.state).into_iter().collect(),
        };

        // HACK so that inspecting using trace returns a valid frame
        // as we don't call the "traditional" EVM methods, the inspector returns a default, invalid, frame.
        if self.inspect && result_state.result.is_success() {
            // create the initial frame
            let mut frame = revm::handler::execution::create_init_frame(
                &tx,
                None, // No precompiled bytecode for shadow transactions
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
                        result: revm::interpreter::InstructionResult::Return,
                        output: Bytes::new(),
                        gas: revm::interpreter::Gas::new(0),
                    },
                    memory_offset: Default::default(),
                    was_precompile_called: false,
                    precompile_call_logs: Vec::new(),
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

    /// Reads the current PD base fee per chunk from the EVM state.
    /// The fee is stored as the balance of the PD_BASE_FEE_ACCOUNT.
    pub fn read_pd_base_fee_per_chunk(&mut self) -> U256 {
        match self.load_account(*PD_BASE_FEE_ACCOUNT) {
            Ok(Some(acc)) => acc.info.balance,
            _ => U256::ZERO,
        }
    }

    /// Reads the current IRYS/USD price from the EVM state.
    /// The price is stored as the balance of the IRYS_USD_PRICE_ACCOUNT.
    /// Returns the price in 1e18 scale (e.g., 1e18 = $1.00).
    ///
    /// # Errors
    /// Returns an error if the price account doesn't exist or has zero balance.
    pub fn read_irys_usd_price(&mut self) -> Result<U256, <Self as Evm>::Error> {
        let price = match self.load_account(*IRYS_USD_PRICE_ACCOUNT)? {
            Some(acc) => acc.info.balance,
            None => U256::ZERO,
        };

        if price.is_zero() {
            return Err(Self::create_internal_error(
                "IRYS/USD price not set in EVM state".to_string(),
            ));
        }

        Ok(price)
    }

    /// Reads the current treasury balance from the EVM state.
    /// The balance is stored as the balance of the TREASURY_ACCOUNT.
    pub fn read_treasury_balance(&mut self) -> U256 {
        match self.load_account(*TREASURY_ACCOUNT) {
            Ok(Some(acc)) => acc.info.balance,
            _ => U256::ZERO,
        }
    }

    /// Increments the treasury balance by the specified amount.
    /// Creates the treasury account if it doesn't exist.
    fn increment_treasury_balance(&mut self, amount: U256) -> Result<(), <Self as Evm>::Error> {
        let target = *TREASURY_ACCOUNT;
        let mut account = if let Some(acc) = self.load_account(target)? {
            acc
        } else {
            Account::new_not_existing(0)
        };
        account.info.balance = account.info.balance.saturating_add(amount);
        self.commit_account_change(target, account);
        Ok(())
    }

    /// Decrements the treasury balance by the specified amount.
    /// Returns an error if treasury has insufficient balance.
    fn decrement_treasury_balance(&mut self, amount: U256) -> Result<(), <Self as Evm>::Error> {
        let target = *TREASURY_ACCOUNT;
        let account = self.load_account(target)?;
        let Some(mut account) = account else {
            return Err(Self::create_internal_error(format!(
                "Treasury account does not exist, cannot decrement by {}",
                amount
            )));
        };
        if account.info.balance < amount {
            return Err(Self::create_internal_error(format!(
                "Insufficient treasury balance: {} < {}",
                account.info.balance, amount
            )));
        }
        account.info.balance = account.info.balance.saturating_sub(amount);
        self.commit_account_change(target, account);
        Ok(())
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
        let db = ctx.db_mut();
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
                target, fee, account.info.balance
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
            let mut account = Account::new_not_existing(0);

            account.info.balance = fee;

            tracing::trace!(
                account.beneficiary = %beneficiary,
                account.balance = %fee,
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
                        self.handle_balance_increment(log, balance_increment, true)?;
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
                    // BlockReward is minted from consensus, not from treasury
                    let (plain_account, execution_result, account_existed) =
                        self.handle_balance_increment(log, &balance_increment, false)?;
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
                    // These are refund/reward transactions - funds come from treasury
                    let (plain_account, execution_result, account_existed) =
                        self.handle_balance_increment(log, balance_increment, true)?;
                    Ok((
                        Ok((plain_account, execution_result, account_existed)),
                        target,
                    ))
                }
                shadow_tx::TransactionPacket::PdBaseFeeUpdate(update) => {
                    // PdBaseFeeUpdate is a PD feature - only allow when Sprite hardfork is active.
                    // This provides defense-in-depth; the consensus layer already guards generation.
                    if !self.is_sprite_active {
                        tracing::warn!(
                            "PdBaseFeeUpdate shadow transaction received before Sprite hardfork is active"
                        );
                        return Err(Self::create_internal_error(
                            "PdBaseFeeUpdate not allowed before Sprite hardfork".to_string(),
                        ));
                    }

                    // Write the per-chunk base fee into the PD base fee account balance.
                    let target = *PD_BASE_FEE_ACCOUNT;

                    // Load existing or create new account
                    let existed;
                    let mut account = if let Some(acc) = self.load_account(target)? {
                        existed = true;
                        acc
                    } else {
                        existed = false;
                        Account::new_not_existing(0)
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
                shadow_tx::TransactionPacket::IrysUsdPriceUpdate(update) => {
                    // IrysUsdPriceUpdate is a PD feature - only allow when Sprite hardfork is active.
                    // This provides defense-in-depth; the consensus layer already guards generation.
                    if !self.is_sprite_active {
                        tracing::warn!(
                            "IrysUsdPriceUpdate shadow transaction received before Sprite hardfork is active"
                        );
                        return Err(Self::create_internal_error(
                            "IrysUsdPriceUpdate not allowed before Sprite hardfork".to_string(),
                        ));
                    }

                    // Write the IRYS/USD price into the price account balance.
                    let target = *IRYS_USD_PRICE_ACCOUNT;

                    // Load existing or create new account
                    let existed;
                    let mut account = if let Some(acc) = self.load_account(target)? {
                        existed = true;
                        acc
                    } else {
                        existed = false;
                        Account::new_not_existing(0)
                    };
                    account.info.balance = update.price;

                    let log = Self::create_shadow_log(
                        target,
                        vec![topic],
                        vec![DynSolValue::Uint(update.price, 256)],
                    );
                    let execution_result = Self::create_success_result(log);
                    Ok((Ok((account, execution_result, existed)), target))
                }
                shadow_tx::TransactionPacket::TreasuryDeposit(deposit) => {
                    // TreasuryDeposit is only allowed when Sprite hardfork is active.
                    // This provides defense-in-depth; the consensus layer already guards generation.
                    if !self.is_sprite_active {
                        tracing::warn!(
                            "TreasuryDeposit shadow transaction received before Sprite hardfork is active"
                        );
                        return Err(Self::create_internal_error(
                            "TreasuryDeposit not allowed before Sprite hardfork".to_string(),
                        ));
                    }

                    // TreasuryDeposit is a protocol-level operation to add funds to treasury
                    let target = *TREASURY_ACCOUNT;

                    // Load existing or create new treasury account
                    let existed;
                    let mut account = if let Some(acc) = self.load_account(target)? {
                        existed = true;
                        acc
                    } else {
                        existed = false;
                        Account::new_not_existing(0)
                    };
                    account.info.balance = account.info.balance.saturating_add(deposit.amount);

                    let log = Self::create_shadow_log(
                        target,
                        vec![topic],
                        vec![DynSolValue::Uint(deposit.amount, 256)],
                    );
                    let execution_result = Self::create_success_result(log);
                    Ok((Ok((account, execution_result, existed)), target))
                }
                shadow_tx::TransactionPacket::UpdateRewardAddress(update_reward_address_debit) => {
                    // Fee-only via priority fee (already processed). Emit a log only.
                    let log = Self::create_shadow_log(
                        update_reward_address_debit.target,
                        vec![topic],
                        vec![
                            DynSolValue::Address(update_reward_address_debit.target),
                            DynSolValue::Address(update_reward_address_debit.new_reward_address),
                        ],
                    );
                    let target = update_reward_address_debit.target;
                    let execution_result = Self::create_success_result(log);
                    Ok((Err(execution_result), target))
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

    /// Handles shadow transaction that increases account balance.
    ///
    /// If `deduct_from_treasury` is true, the funds come from the treasury account.
    /// This is used for refund transactions (UnstakeRefund, UnpledgeRefund, PermFeeRefund)
    /// and reward transactions (TermFeeReward, IngressProofReward).
    ///
    /// If `deduct_from_treasury` is false, the funds are minted from consensus.
    /// This is used for BlockReward transactions.
    fn handle_balance_increment(
        &mut self,
        log: Log,
        balance_increment: &shadow_tx::BalanceIncrement,
        deduct_from_treasury: bool,
    ) -> Result<(Account, ExecutionResult<<Self as Evm>::HaltReason>, bool), <Self as Evm>::Error>
    {
        // If deducting from treasury and Sprite is active, validate and deduct first.
        // Pre-Sprite: funds are effectively minted from nowhere (old behavior).
        if deduct_from_treasury && self.is_sprite_active {
            self.decrement_treasury_balance(balance_increment.amount)?;
        }

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
            let mut account = Account::new_not_existing(0);
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

        // Transfer decremented funds to treasury (only post-Sprite)
        if self.is_sprite_active {
            self.increment_treasury_balance(balance_decrement.amount)?;
        }

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
    use crate::shadow_tx::{
        IrysUsdPriceUpdate, PdBaseFeeUpdate, ShadowTransaction, TransactionPacket,
    };
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

    use alloy_primitives::aliases::U200;
    use irys_types::range_specifier::{ChunkRangeSpecifier, PdAccessListArgSerde as _};
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
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
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
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);

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
        let (mut node, ctx) = ctx.get_single_node()?;

        let payer = ctx.normal_signer.address();
        let beneficiary = ctx.block_producer_a.address();
        let treasury = *TREASURY_ACCOUNT;

        // Fee values must exceed min_pd_transaction_cost ($0.01 USD at $1/IRYS = 0.01 tokens = 10^16 wei)
        // With 3 chunks: total fees = 3 * (base + prio) >= 10^16, so per chunk >= 3.33e15
        let pd_base_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens per chunk
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

        // Set IRYS/USD price (required for PD fee validation)
        let irys_usd_price = U256::from(1_000_000_000_000_000_000_u128); // $1.00 in 1e18 scale
        let irys_price_update = ShadowTransaction::new_v1(
            TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                price: irys_usd_price,
            }),
            solution_hash,
        );
        let irys_price_signed = sign_shadow_tx(
            irys_price_update,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        advance_block(&mut node, vec![pd_fee_update_signed, irys_price_signed]).await?;

        // Capture initial balances and nonce
        let payer_initial = get_balance(&node.inner, payer);
        let beneficiary_initial = get_balance(&node.inner, beneficiary);
        let sink_initial = get_balance(&node.inner, treasury);
        let payer_initial_nonce = get_nonce(&node.inner, payer);

        // Build and submit a PD-header transaction: 3 chunks
        let pd_priority_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens per chunk
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: pd_priority_fee,
            max_base_fee_per_chunk: pd_base_fee,
        };
        let user_calldata = Bytes::from(vec![0xAA, 0xBB]);
        let input = prepend_pd_header_v1_to_calldata(&header, &user_calldata);

        // PD access list: 3 storage keys at PD precompile address
        use crate::test_utils::chunk_spec_with_params;

        let key1 = B256::from(chunk_spec_with_params([0; 25], 0, 1).encode());
        let key2 = B256::from(chunk_spec_with_params([0; 25], 1, 1).encode());
        let key3 = B256::from(chunk_spec_with_params([0; 25], 2, 1).encode());
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
        let payload = advance_block(&mut node, vec![]).await?;

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
        let pd_base_total = U256::from(3_u64) * pd_base_fee;
        let pd_prio_total = U256::from(3_u64) * pd_priority_fee;

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
        let (mut node, ctx) = ctx.get_single_node()?;

        let payer = ctx.normal_signer.address();

        // Set PD base fee - must exceed min_pd_transaction_cost ($0.01 at $1/IRYS)
        // With 2 chunks: total >= 10^16, so per chunk needs ~5e15
        let pd_base_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens
        let pd_priority_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens
        let solution_hash = FixedBytes::<32>::from_slice(&[0x44; 32]);
        let pd_fee_update = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: pd_base_fee,
            }),
            solution_hash,
        );
        let pd_fee_update_signed =
            sign_shadow_tx(pd_fee_update, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Set IRYS/USD price (required for PD fee validation)
        let irys_usd_price = U256::from(1_000_000_000_000_000_000_u128); // $1.00 in 1e18 scale
        let irys_price_update = ShadowTransaction::new_v1(
            TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                price: irys_usd_price,
            }),
            solution_hash,
        );
        let irys_price_signed = sign_shadow_tx(
            irys_price_update,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        advance_block(&mut node, vec![pd_fee_update_signed, irys_price_signed]).await?;

        // Capture pre-simulation state
        let balance_before = get_balance(&node.inner, payer);
        let nonce_before = get_nonce(&node.inner, payer);
        println!(
            "Pre-simulation: balance={}, nonce={}",
            balance_before, nonce_before
        );

        // Build single PD transaction with 2 chunks
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: pd_priority_fee,
            max_base_fee_per_chunk: pd_base_fee,
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
        let (mut node, ctx) = ctx.get_single_node()?;

        let payer = ctx.normal_signer.address();
        let beneficiary = ctx.block_producer_a.address();
        let treasury = *TREASURY_ACCOUNT;

        // Set PD base fee - must exceed min_pd_transaction_cost ($0.01 at $1/IRYS)
        // With 3 chunks: total >= 10^16, so per chunk needs ~3.33e15
        let pd_base_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens
        let pd_priority_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens
        let solution_hash = FixedBytes::<32>::from_slice(&[0x44; 32]);
        let pd_fee_update = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: pd_base_fee,
            }),
            solution_hash,
        );
        let pd_fee_update_signed =
            sign_shadow_tx(pd_fee_update, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Set IRYS/USD price (required for PD fee validation)
        let irys_usd_price = U256::from(1_000_000_000_000_000_000_u128); // $1.00 in 1e18 scale
        let irys_price_update = ShadowTransaction::new_v1(
            TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                price: irys_usd_price,
            }),
            solution_hash,
        );
        let irys_price_signed = sign_shadow_tx(
            irys_price_update,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        advance_block(&mut node, vec![pd_fee_update_signed, irys_price_signed]).await?;

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
            max_priority_fee_per_chunk: pd_priority_fee,
            max_base_fee_per_chunk: pd_base_fee,
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
        let payload = advance_block(&mut node, vec![]).await?;

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
        let pd_base_total = U256::from(num_chunks) * pd_base_fee;
        let pd_prio_total = U256::from(num_chunks) * pd_priority_fee;

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

    /// Test that PD transactions are correctly rejected when actual base fee exceeds
    /// user's max, and automatically included when base fee is lowered.
    ///
    /// This verifies EIP-1559-style mempool retry semantics for PD transactions:
    /// 1. TX rejected when actual_per_chunk > max_base_fee_per_chunk (stays in mempool)
    /// 2. SAME TX automatically included when base fee drops below user's max
    #[test_log::test(tokio::test)]
    async fn test_pd_tx_rejected_then_accepted_on_fee_decrease() -> eyre::Result<()> {
        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;
        let solution_hash = FixedBytes::<32>::from_slice(&[0x11; 32]);

        // Set VERY HIGH IRYS/USD price to make min_pd_transaction_cost negligible
        // min_cost = $0.01 / $1,000,000 = 0.00000001 tokens (effectively zero)
        let irys_usd_price = U256::from(1_000_000_000_000_000_000_000_000_u128); // $1,000,000
        let irys_price_update = ShadowTransaction::new_v1(
            TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                price: irys_usd_price,
            }),
            solution_hash,
        );
        let irys_price_signed = sign_shadow_tx(
            irys_price_update,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        // Set HIGH base fee that exceeds user's max
        let high_base_fee = U256::from(10_000_000_000_000_000_u64); // 0.01 tokens per chunk
        let pd_fee_update_high = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: high_base_fee,
            }),
            solution_hash,
        );
        let pd_fee_high_signed = sign_shadow_tx(
            pd_fee_update_high,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        // User sets max_base_fee BELOW the actual fee
        let user_max_base_fee = U256::from(5_000_000_000_000_000_u64); // 0.005 tokens (< 0.01)
        let pd_priority_fee = U256::from(5_000_000_000_000_000_u64);

        // Build PD transaction header
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: pd_priority_fee,
            max_base_fee_per_chunk: user_max_base_fee, // TOO LOW!
        };

        // Create and submit PD transaction (1 chunk)
        use crate::test_utils::chunk_spec_with_params;

        let payer = ctx.normal_signer.address();
        let payer_nonce = get_nonce(&node.inner, payer);
        let input = prepend_pd_header_v1_to_calldata(&header, &Bytes::from(vec![0xAA]));
        let key1 = B256::from(chunk_spec_with_params([0; 25], 0, 1).encode());
        let access_list = alloy_eips::eip2930::AccessList(vec![AlItem {
            address: irys_types::precompile::PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![key1],
        }]);
        let tx_raw = TxEip1559 {
            access_list,
            input,
            max_priority_fee_per_gas: 0,
            nonce: payer_nonce,
            ..tx_eip1559_base()
        };
        let pooled = sign_tx(tx_raw, &ctx.normal_signer).await;
        let tx_hash = *pooled.hash();
        node.inner
            .pool
            .add_transaction(TransactionOrigin::Local, pooled)
            .await?;

        // Mine block - TX should be REJECTED (not included, stays in mempool)
        let payload1 = advance_block(
            &mut node,
            vec![pd_fee_high_signed, irys_price_signed.clone()],
        )
        .await?;
        let block1 = payload1.block();
        let tx_in_block1 = block1
            .body()
            .transactions
            .iter()
            .any(|tx| *tx.hash() == tx_hash);
        assert!(
            !tx_in_block1,
            "TX should NOT be included when max_base_fee ({}) < actual_base_fee ({})",
            user_max_base_fee, high_base_fee
        );

        // Lower the base fee below user's max
        let low_base_fee = U256::from(3_000_000_000_000_000_u64); // 0.003 tokens (< 0.005)
        let pd_fee_update_low = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: low_base_fee,
            }),
            solution_hash,
        );
        let pd_fee_low_signed = sign_shadow_tx(
            pd_fee_update_low,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        // Mine block with lowered fee - SAME TX should now be picked up from mempool
        let payload2 = advance_block(&mut node, vec![pd_fee_low_signed]).await?;
        let block2 = payload2.block();
        let tx_in_block2 = block2
            .body()
            .transactions
            .iter()
            .any(|tx| *tx.hash() == tx_hash);
        assert!(
            tx_in_block2,
            "TX should be automatically included from mempool when max_base_fee ({}) >= actual_base_fee ({})",
            user_max_base_fee, low_base_fee
        );

        // Verify nonce was consumed (tx was executed)
        let payer_final_nonce = get_nonce(&node.inner, payer);
        assert_eq!(
            payer_final_nonce,
            payer_nonce + 1,
            "Nonce should increment after transaction execution"
        );

        Ok(())
    }

    /// Test that PD transactions are correctly rejected when user has insufficient balance
    /// for PD fees, and automatically included when the account is funded via shadow transaction.
    ///
    /// This verifies EIP-1559-style mempool retry semantics for PD fee balance validation:
    /// 1. TX rejected when balance < total PD fees (stays in mempool)
    /// 2. Account funded via UnstakeRefund shadow transaction
    /// 3. SAME TX automatically included from mempool after funding
    #[test_log::test(tokio::test)]
    async fn test_pd_tx_rejected_then_accepted_after_funding() -> eyre::Result<()> {
        use crate::shadow_tx::BalanceIncrement;
        use crate::shadow_tx::TransactionPacket;
        use crate::shadow_tx::TreasuryDeposit;

        let ctx = TestContext::new().await?;
        let (mut node, ctx) = ctx.get_single_node()?;
        let solution_hash = FixedBytes::<32>::from_slice(&[0x22; 32]);

        // Seed treasury with enough funds for the UnstakeRefund (10M tokens)
        let treasury_seed_amount = U256::from(10_000_000_000_000_000_000_000_000_u128); // 10M tokens
        let treasury_deposit = ShadowTransaction::new_v1(
            TransactionPacket::TreasuryDeposit(TreasuryDeposit {
                amount: treasury_seed_amount,
            }),
            solution_hash,
        );
        let treasury_deposit_signed =
            sign_shadow_tx(treasury_deposit, &ctx.block_producer_a, 0).await?;
        advance_block(&mut node, vec![treasury_deposit_signed]).await?;

        // Use target_account which may have zero or low initial balance
        // We'll set PD fees high enough to exceed any reasonable balance
        let payer = ctx.normal_signer.address();
        let payer_initial_balance = get_balance(&node.inner, payer);
        let payer_nonce = get_nonce(&node.inner, payer);

        // Set IRYS/USD price (required for PD fee validation)
        // Use high price to make min_pd_transaction_cost negligible
        let irys_usd_price = U256::from(1_000_000_000_000_000_000_000_000_u128); // $1,000,000
        let irys_price_update = ShadowTransaction::new_v1(
            TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                price: irys_usd_price,
            }),
            solution_hash,
        );
        let irys_price_signed = sign_shadow_tx(
            irys_price_update,
            &ctx.block_producer_a,
            DEFAULT_PRIORITY_FEE,
        )
        .await?;

        // Set a HIGH actual PD base fee that exceeds the payer's balance when multiplied by chunks
        // payer_initial_balance is ~1M tokens, so set per-chunk fee to 0.6M tokens
        // With 2 chunks: 2 * 0.6M = 1.2M > 1M balance
        let excessive_base_fee_per_chunk = payer_initial_balance
            .saturating_mul(U256::from(6_u64))
            .checked_div(U256::from(10_u64))
            .unwrap(); // 60% of balance per chunk

        let pd_fee_update = ShadowTransaction::new_v1(
            TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                per_chunk: excessive_base_fee_per_chunk,
            }),
            solution_hash,
        );
        let pd_fee_update_signed =
            sign_shadow_tx(pd_fee_update, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Priority fee per chunk (small relative to base fee)
        let pd_priority_fee = U256::from(1_000_000_000_000_000_u64); // 0.001 tokens per chunk

        // Build PD transaction header - user is willing to pay the high fees
        let header = PdHeaderV1 {
            max_priority_fee_per_chunk: pd_priority_fee,
            max_base_fee_per_chunk: excessive_base_fee_per_chunk, // User accepts the high fee
        };

        // Create PD transaction with 2 chunks (total cost = 2 * excessive_fee + 2 * prio)
        use crate::test_utils::chunk_spec_with_params;

        let input = prepend_pd_header_v1_to_calldata(&header, &Bytes::from(vec![0xBB]));
        let key1 = B256::from(chunk_spec_with_params([0; 25], 0, 1).encode());
        let key2 = B256::from(chunk_spec_with_params([0; 25], 1, 1).encode());
        let access_list = alloy_eips::eip2930::AccessList(vec![AlItem {
            address: irys_types::precompile::PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![key1, key2],
        }]);
        let tx_raw = TxEip1559 {
            access_list,
            input,
            max_priority_fee_per_gas: 0,
            nonce: payer_nonce,
            ..tx_eip1559_base()
        };
        let pooled = sign_tx(tx_raw, &ctx.normal_signer).await;
        let tx_hash = *pooled.hash();

        // Submit to mempool - should be accepted (mempool doesn't check PD fee balance)
        node.inner
            .pool
            .add_transaction(TransactionOrigin::Local, pooled)
            .await?;

        // Mine block 1 - TX should be REJECTED due to insufficient balance for PD fees
        let payload1 = advance_block(
            &mut node,
            vec![pd_fee_update_signed, irys_price_signed.clone()],
        )
        .await?;
        let block1 = payload1.block();
        let tx_in_block1 = block1
            .body()
            .transactions
            .iter()
            .any(|tx| *tx.hash() == tx_hash);
        assert!(
            !tx_in_block1,
            "TX should NOT be included when balance ({}) < total PD fees (2 * {})",
            payer_initial_balance, excessive_base_fee_per_chunk
        );

        // Verify payer nonce unchanged (tx not executed)
        let payer_nonce_after_block1 = get_nonce(&node.inner, payer);
        assert_eq!(
            payer_nonce_after_block1, payer_nonce,
            "Nonce should NOT change when TX is rejected"
        );

        // Fund the payer account via UnstakeRefund shadow transaction
        // Need to add enough to cover: 2 chunks * (base_fee + prio_fee) + gas
        // Total PD fees = 2 * excessive_base_fee_per_chunk + 2 * pd_priority_fee
        // Add some extra for safety
        let funding_amount = excessive_base_fee_per_chunk
            .saturating_mul(U256::from(3_u64))
            .saturating_add(pd_priority_fee.saturating_mul(U256::from(2_u64)));

        let funding_tx = ShadowTransaction::new_v1(
            TransactionPacket::UnstakeRefund(BalanceIncrement {
                amount: funding_amount,
                target: payer,
                irys_ref: FixedBytes::ZERO,
            }),
            solution_hash,
        );
        let funding_tx_signed =
            sign_shadow_tx(funding_tx, &ctx.block_producer_a, DEFAULT_PRIORITY_FEE).await?;

        // Mine block 2 with funding - SAME TX should now be picked up from mempool
        let payload2 = advance_block(&mut node, vec![funding_tx_signed]).await?;
        let block2 = payload2.block();
        let tx_in_block2 = block2
            .body()
            .transactions
            .iter()
            .any(|tx| *tx.hash() == tx_hash);
        assert!(
            tx_in_block2,
            "TX should be automatically included from mempool after account is funded"
        );

        // Verify nonce was consumed (tx was executed)
        let payer_final_nonce = get_nonce(&node.inner, payer);
        assert_eq!(
            payer_final_nonce,
            payer_nonce + 1,
            "Nonce should increment after transaction execution"
        );

        // Verify balance was affected (PD fees deducted)
        let payer_final_balance = get_balance(&node.inner, payer);
        assert!(
            payer_final_balance < payer_initial_balance + funding_amount,
            "Balance should be reduced by PD fees after execution"
        );

        Ok(())
    }
}
