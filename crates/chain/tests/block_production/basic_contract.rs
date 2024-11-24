use std::{fs::remove_dir_all, future::Future, time::Duration};

use alloy_core::primitives::U256;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use futures::future::select;
use irys_chain::{chain::start_for_testing, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_types::{block_production::SolutionContext, irys::IrysSigner, Address, H256};
use reth_primitives::GenesisAccount;
use tokio::time::sleep;
use tracing::info;

// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysERC20,
    "../../fixtures/IrysERC20/IrysERC20.json"
);

#[tokio::test]
async fn test_erc20() -> eyre::Result<()> {
    let mut config = IrysNodeConfig::default();
    let main_address = config.mining_signer.address();

    let account1 = IrysSigner::random_signer();

    config.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: U256::from(690000000000000000 as u128),
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

    if config.base_directory.exists() {
        remove_dir_all(&config.base_directory)?;
    }

    let node = start_for_testing(config.clone()).await?;

    let signer: PrivateKeySigner = config.mining_signer.signer.into();
    let wallet = EthereumWallet::from(signer);

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http("http://localhost:8080".parse()?);

    let mut deploy_fut = Box::pin(IrysERC20::deploy(alloy_provider, account1.address()));

    let contract =
        future_or_mine_on_timeout(node.clone(), &mut deploy_fut, Duration::from_millis(2_000))
            .await??;

    info!("Contract address is {:?}", contract.address());
    let main_balance = contract.balanceOf(main_address).call().await?._0;
    assert_eq!(main_balance, U256::from(10000000000000000000000 as u128));

    let transfer_call_builder = contract.transfer(account1.address(), U256::from(10));
    let transfer_call = transfer_call_builder.send().await?;
    let mut transfer_receipt_fut = Box::pin(transfer_call.get_receipt());

    let _ = future_or_mine_on_timeout(
        node.clone(),
        &mut transfer_receipt_fut,
        Duration::from_millis(2_000),
    )
    .await??;

    // check balance for account1
    let addr1_balance = contract.balanceOf(account1.address()).call().await?._0;
    let main_balance2 = contract.balanceOf(main_address).call().await?._0;

    assert_eq!(addr1_balance, U256::from(10));
    assert_eq!(
        main_balance2,
        U256::from(10000000000000000000000 - 10 as u128)
    );

    Ok(())
}

/// Waits for the provided future to resolve, and if it doesn't after `timeout_duration`,
/// triggers the building/mining of a block, and then waits again.
/// designed for use with calls that expect to be able to send and confirm a tx in a single exposed future
pub async fn future_or_mine_on_timeout<F, T>(
    node_ctx: IrysNodeCtx,
    mut future: F,
    timeout_duration: Duration,
) -> eyre::Result<T>
where
    F: Future<Output = T> + Unpin,
{
    loop {
        let race = select(&mut future, Box::pin(sleep(timeout_duration))).await;
        match race {
            // provided future finished
            futures::future::Either::Left((res, _)) => return Ok(res),
            // we need another block
            futures::future::Either::Right(_) => {
                info!("deployment timed out, creating new block..")
            }
        };
        let _ = node_ctx
            .actor_addresses
            .block_producer
            .send(SolutionContext {
                partition_hash: H256::random(),
                chunk_offset: 0,
                mining_address: Address::random(),
            })
            .await?
            .unwrap();
    }
}
