use std::time::Duration;

use alloy_core::primitives::U256;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use irys_chain::{start, start_irys_node};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, storage_config, Config};
use reth_primitives::GenesisAccount;
use tracing::info;

use crate::utils::future_or_mine_on_timeout;
// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysERC20,
    "../../fixtures/contracts/out/IrysERC20.sol/IrysERC20.json"
);
#[tokio::test]
async fn serial_test_erc20() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_erc20"), false);
    let testnet_config = Config::testnet();
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let main_address = config.mining_signer.address();

    let account1 = IrysSigner::random_signer(testnet_config.irys_chain_id);

    config.extend_genesis_accounts(vec![
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

    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let signer: PrivateKeySigner = config.mining_signer.clone().into();

    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(EthereumWallet::from(signer))
        .on_http("http://localhost:8080/v1/execution-rpc".parse()?);

    let mut deploy_fut = Box::pin(IrysERC20::deploy(alloy_provider, account1.address()));

    let contract = future_or_mine_on_timeout(
        node.clone(),
        &mut deploy_fut,
        Duration::from_millis(2_000),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await??;

    info!("Contract address is {:?}", contract.address());
    let main_balance = contract.balanceOf(main_address).call().await?._0;
    assert_eq!(main_balance, U256::from(10000000000000000000000_u128));

    let transfer_call_builder = contract.transfer(account1.address(), U256::from(10));
    let transfer_call = transfer_call_builder.send().await?;
    let mut transfer_receipt_fut = Box::pin(transfer_call.get_receipt());

    let _ = future_or_mine_on_timeout(
        node.clone(),
        &mut transfer_receipt_fut,
        Duration::from_millis(2_000),
        node.vdf_steps_guard,
        &node.vdf_config,
        &node.storage_config,
    )
    .await??;

    // check balance for account1
    let addr1_balance = contract.balanceOf(account1.address()).call().await?._0;
    let main_balance2 = contract.balanceOf(main_address).call().await?._0;

    assert_eq!(addr1_balance, U256::from(10));
    assert_eq!(main_balance2, U256::from(10000000000000000000000 - 10_u128));

    Ok(())
}
