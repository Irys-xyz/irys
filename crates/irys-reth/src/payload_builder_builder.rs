//! Payload component configuration for the Ethereum node.
//! Original impl: https://github.com/paradigmxyz/reth/blob/2b283ae83f6c68b4c851206f8cd01491f63bb608/crates/ethereum/node/src/payload.rs#L19

use irys_types::hardfork_config::IrysHardforkConfig;
use reth_chainspec::{EthChainSpec as _, EthereumHardforks};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeTypes, NodeTypes, PrimitivesTy};
use reth_node_builder::{
    BuilderContext, PayloadBuilderConfig as _, PayloadTypes, components::PayloadBuilderBuilder,
};
use reth_transaction_pool::{EthPooledTransaction, TransactionPool};
use std::sync::Arc;

use crate::{IrysBuiltPayload, IrysPayloadAttributes, IrysPayloadBuilderAttributes};

/// A basic ethereum payload service.
#[derive(Clone, Debug)]
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
}

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for IrysPayloadBuilderBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Unpin + 'static,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Types>,
            NextBlockEnvCtx = reth_evm::NextBlockEnvAttributes,
        > + 'static,
    Types::Payload: PayloadTypes<
        BuiltPayload = IrysBuiltPayload,
        PayloadAttributes = IrysPayloadAttributes,
        PayloadBuilderAttributes = IrysPayloadBuilderAttributes,
    >,
{
    type PayloadBuilder = crate::payload::IrysPayloadBuilder<Pool, Node::Provider, Evm>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let conf = ctx.payload_builder_config();
        let chain = ctx.chain_spec().chain();
        let gas_limit = conf.gas_limit_for(chain);

        Ok(crate::payload::IrysPayloadBuilder::new(
            ctx.provider().clone(),
            pool,
            evm_config,
            EthereumBuilderConfig::new().with_gas_limit(gas_limit),
            self.max_pd_chunks_per_block,
            self.hardforks,
        ))
    }
}
