use clap::Parser;
use irys_reth::IrysEthereumNode;
use irys_types::NodeConfig;
use reth::{beacon_consensus::EthBeaconConsensus, chainspec::EthereumChainSpecParser};
use reth_chainspec::ChainSpec;
use reth_cli_commands::stage::unwind::Command;
use reth_node_ethereum::EthExecutorProvider;
use std::sync::Arc;

pub async fn unwind_to(
    config: NodeConfig,
    chainspec: Arc<ChainSpec>,
    height: u64,
) -> eyre::Result<()> {
    // hack to run unwind

    let mut cmd = Command::<EthereumChainSpecParser>::parse_from([
        "reth",
        "--datadir",
        config.reth_data_dir().to_str().unwrap(),
        "to-block",
        &height.to_string(),
    ]);

    cmd.env.chain = chainspec.clone();

    let components = |spec: Arc<ChainSpec>| {
        (
            EthExecutorProvider::ethereum(spec.clone()),
            EthBeaconConsensus::new(spec),
        )
    };

    cmd.execute::<IrysEthereumNode, _, _>(components).await?;

    Ok(())
}
