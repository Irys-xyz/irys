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

