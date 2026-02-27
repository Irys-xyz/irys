//! Payload component configuration for the Ethereum node.
//! Original impl: https://github.com/paradigmxyz/reth/blob/2b283ae83f6c68b4c851206f8cd01491f63bb608/crates/ethereum/node/src/payload.rs#L19

use crate::evm::ConfigurePdChunkSender;
use irys_types::chunk_provider::PdChunkSender;
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
#[derive(Clone)]
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
    /// PD chunk sender for querying readiness.
    pub pd_chunk_sender: PdChunkSender,
}

impl std::fmt::Debug for IrysPayloadBuilderBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrysPayloadBuilderBuilder")
            .field("max_pd_chunks_per_block", &self.max_pd_chunks_per_block)
            .field("hardforks", &self.hardforks)
            .field("pd_chunk_sender", &"<sender>")
            .finish()
    }
}

impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for IrysPayloadBuilderBuilder
where
    Types: NodeTypes<ChainSpec: EthereumHardforks, Primitives = EthPrimitives>,
    Node: FullNodeTypes<Types = Types>,
    Pool: TransactionPool<Transaction = EthPooledTransaction> + Unpin + 'static,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Types>,
            NextBlockEnvCtx = reth_evm::NextBlockEnvAttributes,
        > + ConfigurePdChunkSender
        + 'static,
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

        // Configure evm_config for payload building — use PdChunkManager for chunk fetching.
        // Reth's ComponentsBuilder already cloned this config before passing it here, so
        // mutating it only affects the payload builder's copy (the executor retains None).
        let evm_config = evm_config.with_pd_chunk_sender(self.pd_chunk_sender.clone());

        Ok(crate::payload::IrysPayloadBuilder::new(
            ctx.provider().clone(),
            pool,
            evm_config,
            EthereumBuilderConfig::new().with_gas_limit(gas_limit),
            self.max_pd_chunks_per_block,
            self.hardforks,
            self.pd_chunk_sender,
        ))
    }
}
