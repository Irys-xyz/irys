//! Payload service component for the node builder.
//! Original impl: https://github.com/paradigmxyz/reth/blob/2b283ae83f6c68b4c851206f8cd01491f63bb608/crates/node/builder/src/components/payload.rs#L1

use crate::{BuilderContext, FullNodeTypes};
use reth::builder::components::{PayloadBuilderBuilder, PayloadServiceBuilder};
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_chain_state::CanonStateSubscriptions;
use reth_node_api::NodeTypes;
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_transaction_pool::TransactionPool;

/// Irys payload service builder that spawns a [`BasicPayloadJobGenerator`]
#[derive(Debug, Default, Clone)]
pub struct IyrsPayloadServiceBuilder<PB>(PB);

impl<PB> IyrsPayloadServiceBuilder<PB> {
    /// Create a new [`IyrsPayloadServiceBuilder`].
    pub const fn new(payload_builder_builder: PB) -> Self {
        Self(payload_builder_builder)
    }
}

impl<Node, Pool, PB, EvmConfig> PayloadServiceBuilder<Node, Pool, EvmConfig>
    for IyrsPayloadServiceBuilder<PB>
where
    Node: FullNodeTypes,
    Pool: TransactionPool,
    EvmConfig: Send,
    PB: PayloadBuilderBuilder<Node, Pool, EvmConfig>,
{
    async fn spawn_payload_builder_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: EvmConfig,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypes>::Payload>> {
        let payload_builder = self.0.build_payload_builder(ctx, pool, evm_config).await?;

        let conf = ctx.config().builder.clone();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval)
            .deadline(conf.deadline)
            .max_payload_tasks(conf.max_payload_tasks);

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            ctx.task_executor().clone(),
            payload_job_config,
            payload_builder,
        );
        let (payload_service, payload_service_handle) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_service_handle)
    }
}
