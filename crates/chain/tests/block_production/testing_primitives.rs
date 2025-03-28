<<<<<<< HEAD
use alloy_core::primitives::U256;
use irys_config::IrysNodeConfig;
use irys_types::{irys::IrysSigner, Config};
use reth_primitives::GenesisAccount;
use tracing::info;

use crate::utils::IrysNodeTest;

#[actix::test]
async fn heavy_test_wait_until_height() {
    let irys_node = IrysNodeTest::new("test_wait_until_height").await;
    let height = irys_node.get_height();
    info!("height: {}", height);
    let steps = 2;
    let seconds = 60;
    irys_node.node_ctx.actor_addresses.set_mining(true).unwrap();
    irys_node.wait_until_height(height + steps, seconds).await;
    let height5 = irys_node.get_height();
    assert_eq!(height5, height + steps);
    irys_node.stop().await;
}

#[actix::test]
async fn heavy_test_mine() {
    let irys_node = IrysNodeTest::new("test_wait_until_height").await;
    let height = irys_node.get_height();
    info!("height: {}", height);
    let blocks = 4;
    irys_node.mine_blocks(blocks).await.unwrap();
    let next_height = irys_node.get_height();
    assert_eq!(next_height, height + blocks as u64);
    let block = irys_node.get_block_by_height(next_height, false).unwrap();
    assert_eq!(block.height, next_height);
    irys_node.stop().await;
}

#[actix::test]
async fn heavy_test_mine_tx() {
    let testnet_config = Config::testnet();
    let mut node_config = IrysNodeConfig::new(&testnet_config);
    let account = IrysSigner::random_signer(&testnet_config);
    node_config.extend_genesis_accounts(vec![(
        account.address(),
        GenesisAccount {
            balance: U256::from(1000),
            ..Default::default()
        },
    )]);

    let irys_node = IrysNodeTest::new_with_config(
        "test_mine_tx",
        Some(testnet_config.clone()),
        Some(node_config),
    )
    .await;
    let height = irys_node.get_height();
    let data = "Hello, world!".as_bytes().to_vec();
    info!("height: {}", height);
    let tx = irys_node
        .create_submit_data_tx(&account, data)
        .await
        .unwrap();
    irys_node.mine_block().await.unwrap();
    let next_height = irys_node.get_height();
    assert_eq!(next_height, height + 1 as u64);
    let tx_header = irys_node.get_tx_header(&tx.header.id).unwrap();
    assert_eq!(tx_header, tx.header);
    irys_node.stop().await;
}
=======
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{Config, StorageConfig};
use tracing::info;

use crate::utils::{get_height, mine, wait_until_height};


async fn start_node() -> IrysNodeCtx {
    let temp_dir = setup_tracing_and_temp_dir(Some("test"), false);
    let testnet_config = Config::testnet();
    let storage_config = StorageConfig::new(&testnet_config);
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    start_irys_node(
        config,
        storage_config,
        testnet_config.clone(),
    )
    .await
    .unwrap()
}

#[actix::test]
async fn test_wait_until_height() {
    let node_ctx = start_node().await;    
    let height = get_height(&node_ctx);
    info!("height: {}", height);
    let steps = 2;
    let seconds = 10;
    node_ctx.actor_addresses.set_mining(true).unwrap();
    wait_until_height(&node_ctx, height + steps, seconds).await;
    let height5 = get_height(&node_ctx);
    assert_eq!(height5, height + steps);
}

#[actix::test]
async fn test_mine() {
    let node_ctx = start_node().await;    
    let height = get_height(&node_ctx);
    info!("height: {}", height);
    let blocks = 2;
    mine(&node_ctx, blocks).await;
    let height5 = get_height(&node_ctx);
    assert_eq!(height5, height + blocks as u64);
}

>>>>>>> master
