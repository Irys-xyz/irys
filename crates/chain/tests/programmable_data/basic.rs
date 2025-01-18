use std::{future::Future, time::Duration};

use alloy_core::primitives::{aliases::U208, U256};
use alloy_eips::eip2930::AccessListItem;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use bytes::Buf;
use futures::future::select;
use irys_actors::block_producer::SolutionFoundMessage;
use irys_chain::{chain::start_for_testing, IrysNodeCtx};
use irys_database::tables::ProgrammableDataChunkCache;
use irys_reth_node_bridge::precompile::irys_executor::IrysPrecompileOffsets;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{block_production::SolutionContext, irys::IrysSigner, Address, CONFIG};
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::Database;
use reth_primitives::{irys_primitives::range_specifier::RangeSpecifier, GenesisAccount};
use tokio::time::sleep;
use tracing::info;
use IrysProgrammableDataBasic::ProgrammableDataArgs;

// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysProgrammableDataBasic,
    "../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json"
);

#[tokio::test]
async fn test_programmable_data_basic() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_programmable_data_basic"), false);
    let mut config = irys_config::IrysNodeConfig {
        base_directory: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
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
    let wallet = EthereumWallet::from(signer.clone());

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http("http://localhost:8080".parse()?);

    let deploy_builder =
        IrysProgrammableDataBasic::deploy_builder(alloy_provider.clone()).gas(29506173);

    let mut deploy_fut = Box::pin(deploy_builder.deploy());

    let contract_address =
        future_or_mine_on_timeout(node.clone(), &mut deploy_fut, Duration::from_millis(500))
            .await??;

    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider.clone());

    let precompile_address: Address = IrysPrecompileOffsets::ProgrammableData.into();
    info!(
        "Contract address is {:?}, precompile address is {:?}",
        contract.address(),
        precompile_address
    );

    // insert some dummy data to the PD cache table
    // note: this logic is very dumbed down for the PoC
    let write_tx = node.db.tx_mut()?;
    let mut chunk = [0u8; CONFIG.chunk_size as usize];

    let message = "Hirys, world!";

    message
        .as_bytes()
        .copy_to_slice(&mut chunk[0..message.len()]);

    write_tx.put::<ProgrammableDataChunkCache>(0, chunk.to_vec())?;
    write_tx.commit()?;

    // call with a range specifier index, position in the requested range to start from, and the number of chunks to read
    let mut invocation_builder = contract.read_pd_chunk_into_storage(
        ProgrammableDataArgs {
            range_index: 0,
            offset: 0,
            count: 1,
        },
        message.len() as u8,
    );

    // set the access list (this lets the node know in advance what chunks you want to access)
    invocation_builder = invocation_builder.access_list(
        vec![AccessListItem {
            address: precompile_address,
            storage_keys: vec![RangeSpecifier {
                partition_index: U208::from(0),
                offset: 0,
                chunk_count: 1_u16,
            }
            .into()],
        }]
        .into(),
    );

    let invocation_call = invocation_builder.send().await?;
    let mut invocation_receipt_fut = Box::pin(invocation_call.get_receipt());
    let _res = future_or_mine_on_timeout(
        node.clone(),
        &mut invocation_receipt_fut,
        Duration::from_millis(500),
    )
    .await??;

    let stored_bytes = contract.get_storage().call().await?._0;
    let stored_message = String::from_utf8(stored_bytes.to_vec())?;

    println!(
        "Original string: {}, stored string: {}",
        &message, &stored_message
    );

    assert_eq!(&message, &stored_message);

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
            .send(SolutionFoundMessage(SolutionContext::default()))
            .await?
            .unwrap();
    }
}
