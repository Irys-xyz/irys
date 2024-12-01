use alloy_core::primitives::{B256, U256};
use alloy_genesis::Genesis;
use alloy_provider::ProviderBuilder;
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::{
    node::NodeTestContext, setup, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_builder::{NodeBuilder, NodeHandle};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_node_ethereum::EthereumNode;
use reth_tasks::TaskManager;
use reth_tracing::tracing::info;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;

use alloy_network::EthereumWallet;
use alloy_sol_types::sol;
use futures::future::select;

// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysSimpleStorage,
    "/workspaces/irys-rs/fixtures/contracts/out/IrysSimpleStorage.sol/SimpleStorage.json"
);

#[tokio::test]
async fn can_run_eth_node_2() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = setup::<EthereumNode>(
        1,
        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(serde_json::from_str(include_str!("./genesis.json")).unwrap())
                .cancun_activated()
                .build(),
        ),
        false,
    )
    .await?;

    let mut node = nodes.pop().unwrap();

    // let raw_tx = TransactionTestContext::transfer_tx_bytes(1, wallet.inner.clone()).await;
    let rpc_url = format!(
        "http://{}",
        node.inner.rpc_server_handle().http_local_addr().unwrap()
    )
    .parse()
    .unwrap();

    let wallet2 = EthereumWallet::from(wallet.inner.clone());
    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet2)
        .on_http(rpc_url);

    let mut deployment_fut = Box::pin(IrysSimpleStorage::deploy(alloy_provider.clone()));
    let contract = loop {
        let race = select(
            &mut deployment_fut,
            Box::pin(sleep(Duration::from_millis(2_000))),
        )
        .await;
        match race {
            // provided future finished
            futures::future::Either::Left((res, _)) => break res?,
            // we need another block
            futures::future::Either::Right(_) => {
                info!("deployment timed out, creating new block..");
                let (payload, _) = node.advance_block(vec![], eth_payload_attributes).await?;
            }
        }
    };

    info!("contract address is {}", &contract.address());

    let value = U256::from(1337);
    let mut store_builder = contract.set(value);
    let store_call = store_builder /* .set_required_confirmations(4) */
        .send()
        .await?;

    let mut store_receipt_fut = Box::pin(store_call.get_receipt());

    let res = loop {
        let race = select(
            &mut store_receipt_fut,
            Box::pin(sleep(Duration::from_millis(2_000))),
        )
        .await;
        match race {
            // provided future finished
            futures::future::Either::Left((res, _)) => break res?,
            // we need another block
            futures::future::Either::Right(_) => {
                info!("tx timed out, creating new block..");
                let (payload, _) = node.advance_block(vec![], eth_payload_attributes).await?;
            }
        }
    };

    let stored_value = contract.get().call().await?._0;

    dbg!(res, stored_value);

    // // make the node advance
    // let tx_hash = node.rpc.inject_tx(raw_tx).await?;

    // // make the node advance
    // let (payload, _) = node.advance_block().await?;

    // let block_hash = payload.block().hash();
    // let block_number = payload.block().number;

    // // assert the block has been committed to the blockchain
    // node.assert_new_block(tx_hash, block_hash, block_number).await?;

    Ok(())
}

use reth::rpc::types::engine::PayloadAttributes;

use alloy_primitives::Address;
use reth_payload_builder::EthPayloadBuilderAttributes;

fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: None,
        parent_beacon_block_root: None,
        shadows: None,
    };
    EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
}
