use alloy_primitives::{Address, Bytes, U256};
use irys_types::reth_provider::IrysRethProvider;
use reth::{
    api::NextBlockEnvAttributes,
    builder::{
        components::{ExecutorBuilder, PayloadServiceBuilder},
        BuilderContext,
    },
    payload::{EthBuiltPayload, EthPayloadBuilderAttributes},
    primitives::revm_primitives::{BlockEnv, CfgEnvWithHandlerCfg, Env, PrecompileResult, TxEnv},
    revm::{
        handler::register::EvmHandler,
        inspector_handle_register,
        precompile::{Precompile, PrecompileSpecId},
        ContextPrecompiles, Database, Evm, EvmBuilder, GetInspector,
    },
    rpc::types::engine::PayloadAttributes,
    transaction_pool::TransactionPool,
};
use reth_chainspec::ChainSpec;
use reth_node_api::{
    ConfigureEvm, ConfigureEvmEnv, FullNodeTypes, NodeTypes, NodeTypesWithEngine, PayloadTypes,
};
use reth_node_ethereum::{node::EthereumPayloadBuilder, EthEvmConfig, EthExecutorProvider};
use reth_primitives::{Header, TransactionSigned};
use revm::{precompile::u64_to_address, ContextPrecompile};
use revm_primitives::StatefulPrecompile;
use std::sync::Arc;
use tracing::info;

use crate::precompile::programmable_data::PROGRAMMABLE_DATA_PRECOMPILE;

// TODO: sometimes the EVM is initialized with spec ID CANCUN, and sometimes with MERGE
// for now it doesn't matter much, but we do want to fix it eventually.
// edit: I think it's this: https://github.com/Irys-xyz/reth/blob/fabcd84e06a8e51b3bfb51c390ea136de9e5d3e2/crates/evm/src/lib.rs#L59
// for some reason the spec id is passed in after init
// but doing so doesn't update the precompiles..

type Precompiles = Vec<CustomPrecompileWithAddress>;

#[derive(Clone, Debug)]
pub struct CustomPrecompileWithAddress(pub Address, pub PrecompileFn);

impl From<(Address, PrecompileFn)> for CustomPrecompileWithAddress {
    fn from(value: (Address, PrecompileFn)) -> Self {
        CustomPrecompileWithAddress(value.0, value.1)
    }
}

impl From<CustomPrecompileWithAddress> for (Address, PrecompileFn) {
    fn from(value: CustomPrecompileWithAddress) -> Self {
        (value.0, value.1)
    }
}

impl CustomPrecompileWithAddress {
    /// Returns reference of address.
    #[inline]
    pub fn address(&self) -> &Address {
        &self.0
    }

    /// Returns reference of precompile.
    #[inline]
    pub fn precompile(&self) -> &PrecompileFn {
        &self.1
    }
}

#[derive(Debug, Clone)]
// provides context to the precompile
pub struct PrecompileStateProvider {
    pub provider: IrysRethProvider,
}

/// Custom EVM configuration
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysEvmConfig {
    inner: EthEvmConfig,
    precompile_state_provider: PrecompileStateProvider,
    precompiles: Precompiles,
}

impl IrysEvmConfig {
    /// Creates a new instance.
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        precompile_state_provider: PrecompileStateProvider,
        precompiles: Precompiles,
    ) -> Self {
        Self {
            inner: EthEvmConfig::new(chain_spec),
            precompile_state_provider,
            precompiles,
        }
    }

    /// Sets the precompiles to the EVM handler
    ///
    /// This will be invoked when the EVM is created via [ConfigureEvm::evm] or
    /// [ConfigureEvm::evm_with_inspector]
    ///
    /// This will use the default mainnet precompiles and wrap them with a cache.
    pub fn set_precompiles<EXT, DB>(
        handler: &mut EvmHandler<EXT, DB>,
        precompile_state_provider: PrecompileStateProvider,
        precompiles: Precompiles,
    ) where
        DB: Database,
    {
        // first we need the evm spec id, which determines the precompiles
        let spec_id = handler.cfg.spec_id;
        let spec_id2 = PrecompileSpecId::from_spec_id(spec_id);
        let mut loaded_precompiles: ContextPrecompiles<DB> = ContextPrecompiles::new(spec_id2);
        // loaded_precompiles.extend(precompiles);

        let wrapped_precompiles: Vec<(Address, ContextPrecompile<DB>)> = precompiles
            .iter()
            .map(|pwithaddr| {
                Self::wrap_precompile::<DB>(pwithaddr.clone(), precompile_state_provider.clone())
            })
            .collect();

        info!("extending with {} precompiles", &wrapped_precompiles.len());
        loaded_precompiles.extend(wrapped_precompiles);

        // install the precompiles
        handler.pre_execution.load_precompiles = Arc::new(move || loaded_precompiles.clone());
    }

    /// Given a [`CustomPrecompileWithAddress`] and cache for a specific precompile, create a new precompile
    /// that wraps the precompile with the provider.
    fn wrap_precompile<DB>(
        precompile: CustomPrecompileWithAddress,
        precompile_state_provider: PrecompileStateProvider,
    ) -> (Address, ContextPrecompile<DB>)
    where
        DB: Database,
    {
        let wrapped = WrappedPrecompile {
            precompile: precompile.1,
            precompile_state_provider,
        };

        (
            precompile.0,
            ContextPrecompile::Ordinary(Precompile::Stateful(Arc::new(wrapped))),
        )
    }
}

pub type PrecompileFn = fn(&Bytes, u64, &Env, &PrecompileStateProvider) -> PrecompileResult;

/// A custom precompile that contains the cache and precompile it wraps.
#[derive(Clone)]
pub struct WrappedPrecompile {
    /// The precompile to wrap.
    precompile: PrecompileFn,
    /// The cache to use.
    pub precompile_state_provider: PrecompileStateProvider,
}

impl StatefulPrecompile for WrappedPrecompile {
    // wrapper so it calls it like a normal precompile
    fn call(&self, bytes: &Bytes, gas_price: u64, env: &Env) -> PrecompileResult {
        (self.precompile)(bytes, gas_price, env, &self.precompile_state_provider)
    }
}

impl ConfigureEvmEnv for IrysEvmConfig {
    type Header = Header;

    fn fill_tx_env(&self, tx_env: &mut TxEnv, transaction: &TransactionSigned, sender: Address) {
        self.inner.fill_tx_env(tx_env, transaction, sender)
    }

    fn fill_tx_env_system_contract_call(
        &self,
        env: &mut Env,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) {
        self.inner
            .fill_tx_env_system_contract_call(env, caller, contract, data)
    }

    fn fill_cfg_env(
        &self,
        cfg_env: &mut CfgEnvWithHandlerCfg,
        header: &Self::Header,
        total_difficulty: U256,
    ) {
        self.inner.fill_cfg_env(cfg_env, header, total_difficulty)
    }

    fn next_cfg_and_block_env(
        &self,
        parent: &Self::Header,
        attributes: NextBlockEnvAttributes,
    ) -> (CfgEnvWithHandlerCfg, BlockEnv) {
        self.inner.next_cfg_and_block_env(parent, attributes)
    }
}

impl ConfigureEvm for IrysEvmConfig {
    type DefaultExternalContext<'a> = ();

    fn evm<DB: Database>(&self, db: DB) -> Evm<'_, Self::DefaultExternalContext<'_>, DB> {
        EvmBuilder::default()
            .with_db(db)
            // add additional precompiles
            .append_handler_register_box(Box::new(move |handler| {
                IrysEvmConfig::set_precompiles(
                    handler,
                    self.precompile_state_provider.clone(),
                    self.precompiles.clone(),
                )
            }))
            .build()
    }

    fn default_external_context<'a>(&self) -> Self::DefaultExternalContext<'a> {}

    // // custom for always-on tracing
    // type DefaultExternalContext<'a> = TracerEip3155;

    // fn evm<DB: Database>(&self, db: DB) -> Evm<'_, TracerEip3155, DB> {
    //     let inspector = self.default_external_context();
    //     EvmBuilder::default()
    //         .with_db(db)
    //         .with_external_context(inspector)
    //         // add additional precompiles
    //         .append_handler_register_box(Box::new(move |handler| {
    //             IrysEvmConfig::set_precompiles(
    //                 handler,
    //                 self.precompile_state_provider.clone(),
    //                 self.precompiles.clone(),
    //             )
    //         }))
    //         .append_handler_register(inspector_handle_register)
    //         .build()
    // }

    // fn default_external_context<'a>(&self) -> Self::DefaultExternalContext<'a> {
    //     TracerEip3155::new(Box::new(stderr())).without_summary()
    // }
    // // end of custom for always-on tracing

    fn evm_with_inspector<DB, I>(&self, db: DB, inspector: I) -> Evm<'_, I, DB>
    where
        DB: Database,
        I: GetInspector<DB>,
    {
        EvmBuilder::default()
            .with_db(db)
            .with_external_context(inspector)
            // add additional precompiles
            .append_handler_register_box(Box::new(move |handler| {
                IrysEvmConfig::set_precompiles(
                    handler,
                    self.precompile_state_provider.clone(),
                    self.precompiles.clone(),
                )
            }))
            .append_handler_register(inspector_handle_register)
            .build()
    }
}

/// Builds a regular ethereum block executor that uses the custom EVM.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysExecutorBuilder {
    /// The precompile cache to use for all executors.
    pub precompile_state_provider: PrecompileStateProvider,
}

impl<Node> ExecutorBuilder<Node> for IrysExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec>>,
{
    type EVM = IrysEvmConfig;
    type Executor = EthExecutorProvider<Self::EVM>;

    async fn build_evm(
        self,

        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<(Self::EVM, Self::Executor)> {
        let precompile_state_provider = PrecompileStateProvider {
            provider: ctx.irys_ext.clone().unwrap().provider,
        };
        let evm_config = IrysEvmConfig {
            inner: EthEvmConfig::new(ctx.chain_spec()),
            precompile_state_provider,
            precompiles: irys_precompiles(),
        };
        Ok((
            evm_config.clone(),
            EthExecutorProvider::new(ctx.chain_spec(), evm_config),
        ))
    }
}

/// Builds a regular ethereum block executor that uses the custom EVM.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct IrysPayloadBuilder {
    inner: EthereumPayloadBuilder,
}

impl<Types, Node, Pool> PayloadServiceBuilder<Node, Pool> for IrysPayloadBuilder
where
    Types: NodeTypesWithEngine<ChainSpec = ChainSpec>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool + Unpin + 'static,
    Types::Engine: PayloadTypes<
        BuiltPayload = EthBuiltPayload,
        PayloadAttributes = PayloadAttributes,
        PayloadBuilderAttributes = EthPayloadBuilderAttributes,
    >,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<reth::payload::PayloadBuilderHandle<Types::Engine>> {
        let precompile_state_provider = PrecompileStateProvider {
            provider: ctx.irys_ext.clone().unwrap().provider,
        };
        let evm_config = IrysEvmConfig {
            inner: EthEvmConfig::new(ctx.chain_spec()),
            precompile_state_provider,
            precompiles: irys_precompiles(),
        };

        self.inner.spawn(evm_config, ctx, pool)
    }
}

pub fn irys_precompiles() -> Precompiles {
    vec![PROGRAMMABLE_DATA_PRECOMPILE]
}

// reserve space for any future eth precompiles
const BASE_PRECOMPILE_OFFSET: u64 = 1337;

#[repr(u64)]
pub enum IrysPrecompileOffsets {
    ProgrammableData = BASE_PRECOMPILE_OFFSET,
}

impl IrysPrecompileOffsets {
    pub const fn to_address(self) -> Address {
        u64_to_address(self as u64)
    }
}

impl Into<Address> for IrysPrecompileOffsets {
    fn into(self) -> Address {
        self.to_address()
    }
}
