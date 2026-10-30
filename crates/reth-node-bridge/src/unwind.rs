use alloy_evm::EthEvmFactory;
use alloy_primitives::B256;
use clap::Parser as _;
use irys_types::NodeConfig;
use reth::{beacon_consensus::EthBeaconConsensus, chainspec::EthereumChainSpecParser};
use reth_chainspec::ChainSpec;
use reth_cli_commands::stage::unwind::Command;
use reth_node_ethereum::{EthEvmConfig, EthereumNode};
use std::sync::Arc;

/// Unwinds reth so that `target` (an EVM block hash) becomes its tip.
///
/// Targets by hash, not number: an Irys block's height and its EVM block's
/// number are not guaranteed to be equal, so unwinding to the Irys height can
/// remove the very EVM block the startup FCU then points at.
pub async fn unwind_to(
    config: &NodeConfig,
    chainspec: Arc<ChainSpec>,
    target: B256,
    runtime: reth::tasks::Runtime,
) -> eyre::Result<()> {
    // hack to run unwind - uses standard EthereumNode since unwinding doesn't need Irys-specific logic

    let mut cmd = Command::<EthereumChainSpecParser>::parse_from([
        "reth",
        "--datadir",
        config.reth_data_dir().to_str().unwrap(),
        "to-block",
        &target.to_string(),
    ]);

    cmd.env.chain = chainspec.clone();

    let components = |spec: Arc<ChainSpec>| {
        (
            EthEvmConfig::new_with_evm_factory(spec.clone(), EthEvmFactory::default()),
            Arc::new(EthBeaconConsensus::new(spec)),
        )
    };

    cmd.execute::<EthereumNode, _, _>(components, runtime)
        .await?;

    Ok(())
}
