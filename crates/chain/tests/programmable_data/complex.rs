use std::{fs::remove_dir_all, future::Future, time::Duration};

use crate::programmable_data::basic::IrysProgrammableDataBasic::IrysProgrammableDataBasicCalls::get_storage;
use alloy_core::primitives::B256;
use alloy_core::primitives::{aliases::U208, U256};
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_network::EthereumWallet;
use alloy_provider::Provider;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use futures::future::select;
use irys_chain::{chain::start_for_testing, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_database::tables::ProgrammableDataChunkCache;
use irys_reth_node_bridge::precompile::irys_executor::IrysPrecompileOffsets;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{block_production::SolutionContext, irys::IrysSigner, Address, CHUNK_SIZE, H256};
use reth_db::cursor::DbCursorRO;
use reth_db::cursor::DbDupCursorRO;
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::{Database, DatabaseError, PlainStorageState};
use reth_primitives::{irys_primitives::range_specifier::RangeSpecifier, GenesisAccount};
use tokio::time::{sleep, Sleep};
use tracing::debug;
use tracing::info;
// use IrysProgrammableDataBasic::PdArgs;
// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysProgrammableDataBasic,
    "../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json"
);

#[tokio::test]
async fn test_programmable_data() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("test_programmable_data"), false);
    let mut config = IrysNodeConfig::default();
    config.base_directory = temp_dir.path().to_path_buf();
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
    let wallet = EthereumWallet::from(signer.clone());

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http("http://localhost:8080".parse()?);

    let mut deploy_fut = Box::pin(IrysProgrammableDataBasic::deploy(alloy_provider.clone()));

    let contract =
        future_or_mine_on_timeout(node.clone(), &mut deploy_fut, Duration::from_millis(2_000))
            .await??;

    let precompile_address: Address = IrysPrecompileOffsets::ProgrammableData.into();
    info!(
        "Contract address is {:?}, precompile address is {:?}",
        contract.address(),
        precompile_address
    );

    // insert some dummy data to the PD cache table
    // note: this logic is very dumbed down for the PoC
    let write_tx = node.db.tx_mut()?;
    //split the word "hirys world" across two ranges of chunks, with each char being the first byte in it's chunk.
    let words = ["hirys", "world!"];
    let mut range_specifiers: Vec<B256> = vec![];
    for (i, word) in words.iter().enumerate() {
        for (j, char) in word.chars().enumerate() {
            let mut chunk = [0u8; CHUNK_SIZE as usize];
            /*  let enc =  */
            char.encode_utf8(&mut chunk);
            let offset = ((10 * i) + j) as u32;
            info!(
                "char {}, char len {}, offset {}, slice {:?}",
                &char,
                &char.len_utf8(),
                &offset,
                // &enc,
                &chunk[0..10]
            );

            write_tx.put::<ProgrammableDataChunkCache>(offset, chunk.to_vec())?;
        }
        range_specifiers.push(
            RangeSpecifier {
                partition_index: U208::from(i),
                offset: 0,
                chunk_count: word.len() as u16,
            }
            .into(),
        );
    }
    let chk1 = write_tx.get::<ProgrammableDataChunkCache>(0)?.unwrap();
    info!("{:?}", &chk1[0..10]);

    write_tx.commit()?;

    // call with a range specifier index, position in the requested range to start from, and the number of chunks to read
    // let mut invocation_builder = contract.get_pd_chunks(vec![
    //     PdArgs {
    //         // read the first word
    //         range_index: 0,
    //         offset: 0,
    //         count: words.get(0).unwrap().len() as u16,
    //     },
    //     PdArgs {
    //         // read the second word
    //         range_index: 1,
    //         offset: 0,
    //         count: words.get(1).unwrap().len() as u16,
    //     },
    // ]);

    let mut invocation_builder = contract.get_pd_chunks(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9].into());

    invocation_builder = invocation_builder.access_list(
        vec![AccessListItem {
            address: precompile_address,
            storage_keys: range_specifiers,
        }]
        .into(),
    );

    let invocation_call = match invocation_builder.send().await {
        Ok(r) => Ok(r),
        Err(e) => {
            // sleep so I can debug in peace :p
            sleep(Duration::from_millis(100_000)).await;
            Err(e)
        }
    }?;

    let mut invocation_receipt_fut = Box::pin(invocation_call.get_receipt());

    let res = future_or_mine_on_timeout(
        node.clone(),
        &mut invocation_receipt_fut,
        Duration::from_millis(/* 100_000 */ 2_000),
    )
    .await??;

    // let get_storage_builder = contract.get_storage();
    // let str = get_storage_builder.send().await?;
    // // let r = str.get_receipt()

    // let mut storage_invokation_fut = Box::pin(str.get_receipt());

    // let res2 = future_or_mine_on_timeout(
    //     node.clone(),
    //     &mut storage_invokation_fut,
    //     Duration::from_millis(/* 100_000 */ 2_000),
    // )
    // .await??;

    // dbg!(res2);
    let str = contract.get_storage().call().await?._0;
    dbg!(str);

    let storage = alloy_provider
        .get_storage_at(*contract.address(), U256::from(1337))
        .latest()
        .await?;

    let storage2 = alloy_provider
        .get_storage_at(signer.address(), U256::from(1337))
        .latest()
        .await?;

    info!("storage: {:?}, storage2 {:?}", storage, storage2);
    let ro_tx = node.db.tx()?;

    let mut cursor = ro_tx.cursor_dup_read::<PlainStorageState>()?;

    let walked = cursor
        .walk_dup(None, None)?
        .collect::<Result<Vec<_>, DatabaseError>>()?;

    info!("walked: {:?}", walked);

    dbg!(res);
    // // let transfer_call = transfer_call_builder.send().await?;
    // let mut transfer_receipt_fut = Box::pin(transfer_call.get_receipt());

    // let _ = future_or_mine_on_timeout(
    //     node.clone(),
    //     &mut transfer_receipt_fut,
    //     Duration::from_millis(2_000),
    // )
    // .await??;

    // // check balance for account1
    // let addr1_balance = contract.balanceOf(account1.address()).call().await?._0;
    // let main_balance2 = contract.balanceOf(main_address).call().await?._0;

    // assert_eq!(addr1_balance, U256::from(10));
    // assert_eq!(
    //     main_balance2,
    //     U256::from(10000000000000000000000 - 10 as u128)
    // );

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
