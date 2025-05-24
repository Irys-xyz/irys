use alloy_primitives::B256;
use reth_beacon_consensus::EthBeaconConsensus;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_config::{config::StageConfig, PruneConfig};
use reth_consensus::Consensus;
use reth_downloaders::{bodies::noop::NoopBodiesDownloader, headers::noop::NoopHeaderDownloader};
use reth_evm::noop::NoopBlockExecutorProvider;
use reth_exex::ExExManagerHandle;
use reth_node_api::NodeTypes;
use reth_node_builder::{NodeTypesWithDB, NodeTypesWithEngine};
use reth_provider::{ChainSpecProvider, ProviderFactory};
use reth_prune::PruneModes;
use reth_stages::{
    sets::{DefaultStages, OfflineStages},
    stages::ExecutionStage,
    ExecutionStageThresholds, Pipeline, StageSet,
};
use reth_static_file::StaticFileProducer;
use std::sync::Arc;
use tokio::sync::watch;

// copied from ext/reth/crates/cli/commands/src/stage/unwind.rs

pub fn build_pipeline<N>(
    offline: bool,
    stage_conf: StageConfig,
    prune_config: Option<PruneConfig>,
    provider_factory: ProviderFactory<N>,
) -> Result<Pipeline<N>, eyre::Error>
where
    N: NodeTypesWithEngine + NodeTypesWithDB,
    <N as NodeTypes>::ChainSpec: EthChainSpec + EthereumHardforks,
{
    let consensus: Arc<dyn Consensus> =
        Arc::new(EthBeaconConsensus::new(provider_factory.chain_spec()));

    let prune_modes = prune_config
        .clone()
        .map(|prune| prune.segments)
        .unwrap_or_default();

    let (tip_tx, tip_rx) = watch::channel(B256::ZERO);

    // Unwinding does not require a valid executor
    let executor = NoopBlockExecutorProvider::default();

    let builder = if offline {
        Pipeline::<N>::builder().add_stages(
            OfflineStages::new(executor, stage_conf, PruneModes::default())
                .builder()
                .disable(reth_stages::StageId::SenderRecovery),
        )
    } else {
        Pipeline::<N>::builder().with_tip_sender(tip_tx).add_stages(
            DefaultStages::new(
                provider_factory.clone(),
                tip_rx,
                Arc::clone(&consensus),
                NoopHeaderDownloader::default(),
                NoopBodiesDownloader::default(),
                executor.clone(),
                stage_conf.clone(),
                prune_modes.clone(),
            )
            .set(ExecutionStage::new(
                executor,
                ExecutionStageThresholds {
                    max_blocks: None,
                    max_changes: None,
                    max_cumulative_gas: None,
                    max_duration: None,
                },
                stage_conf.execution_external_clean_threshold(),
                prune_modes,
                ExExManagerHandle::empty(),
            )),
        )
    };

    let pipeline = builder.build(
        provider_factory.clone(),
        StaticFileProducer::new(provider_factory, PruneModes::default()),
    );
    Ok(pipeline)
}
