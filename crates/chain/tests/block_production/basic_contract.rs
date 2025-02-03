use std::{future::Future, time::Duration};

use alloy_core::primitives::U256;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use futures::future::select;
use irys_actors::block_producer::SolutionFoundMessage;
use irys_chain::{chain::start_for_testing, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, StorageConfig, VDFStepsConfig};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth_primitives::GenesisAccount;
use tokio::time::sleep;
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
async fn test_erc20() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_erc20"), false);
    let mut config = IrysNodeConfig::default();
    config.base_directory = temp_dir.path().to_path_buf();
    let main_address = config.mining_signer.address();

    let account1 = IrysSigner::random_signer();

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

    let node = start_for_testing(config.clone()).await?;

    let signer: PrivateKeySigner = config.mining_signer.signer.into();
    let wallet: EthereumWallet = EthereumWallet::from(signer);

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http("http://localhost:8080".parse()?);

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
