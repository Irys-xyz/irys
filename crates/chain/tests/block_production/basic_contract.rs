use std::time::Duration;

use alloy_core::primitives::U256;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use irys_types::{irys::IrysSigner, NodeConfig};
use reth_primitives::GenesisAccount;
use tracing::info;

use crate::utils::{future_or_mine_on_timeout, IrysNodeTest};
// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysERC20,
    "../../fixtures/contracts/out/IrysERC20.sol/IrysERC20.json"
);
#[tokio::test]
async fn heavy_test_erc20() -> eyre::Result<()> {
    let mut config = NodeConfig::testnet();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let main_address = config.miner_address();
    config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: U256::from(1),
                ..Default::default()
            },
        ),
    ]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    let signer: PrivateKeySigner = node.cfg.mining_key.clone().into();

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(EthereumWallet::from(signer))
        .on_http(
            format!(
                "http://127.0.0.1:{}/v1/execution-rpc",
                node.node_ctx.config.node_config.http.bind_port
            )
            .parse()?,
        );

    let mut deploy_fut = Box::pin(IrysERC20::deploy(alloy_provider, account1.address()));

    let contract = future_or_mine_on_timeout(
        node.node_ctx.clone(),
        &mut deploy_fut,
        Duration::from_millis(2_000),
    )
    .await??;

    info!("Contract address is {:?}", contract.address());
    let main_balance = contract.balanceOf(main_address).call().await?._0;
    assert_eq!(main_balance, U256::from(10000000000000000000000_u128));

    let transfer_call_builder = contract.transfer(account1.address(), U256::from(10));
    let transfer_call = transfer_call_builder.send().await?;
    let mut transfer_receipt_fut = Box::pin(transfer_call.get_receipt());

    let _ = future_or_mine_on_timeout(
        node.node_ctx.clone(),
        &mut transfer_receipt_fut,
        Duration::from_millis(2_000),
    )
    .await??;

    // check balance for account1
    let addr1_balance = contract.balanceOf(account1.address()).call().await?._0;
    let main_balance2 = contract.balanceOf(main_address).call().await?._0;

    assert_eq!(addr1_balance, U256::from(10));
    assert_eq!(main_balance2, U256::from(10000000000000000000000 - 10_u128));
    node.node_ctx.stop().await;
    Ok(())
}
