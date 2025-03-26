use alloy_core::primitives::U256;
use irys_config::IrysNodeConfig;
use irys_types::{irys::IrysSigner, Config};
use reth_primitives::GenesisAccount;
use tracing::info;

use crate::utils::{
    add_tx, get_height, get_tx_header, mine, mine_one, start_node, start_node_config,
    wait_until_height,
};

#[actix::test]
async fn test_wait_until_height() {
    let (node_ctx, _tmp_dir) = start_node("test_wait_until_height").await;
    let height = get_height(&node_ctx);
    info!("height: {}", height);
    let steps = 2;
    let seconds = 20;
    node_ctx.actor_addresses.set_mining(true).unwrap();
    wait_until_height(&node_ctx, height + steps, seconds).await;
    let height5 = get_height(&node_ctx);
    assert_eq!(height5, height + steps);
    node_ctx.stop().await;
}

#[actix::test]
async fn test_mine() {
    let (node_ctx, _tmp_dir) = start_node("test_mine").await;
    let height = get_height(&node_ctx);
    info!("height: {}", height);
    let blocks = 4;
    mine(&node_ctx, blocks).await.unwrap();
    let next_height = get_height(&node_ctx);
    assert_eq!(next_height, height + blocks as u64);
    node_ctx.stop().await;    
}

#[actix::test]
async fn test_mine_tx() {
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

    let (node_ctx, _tmp_dir) = start_node_config(
        "test_mine_tx",
        Some(testnet_config.clone()),
        Some(node_config),
    )
    .await;
    let height = get_height(&node_ctx);
    let data = "Hello, world!".as_bytes().to_vec();
    info!("height: {}", height);
    let tx = add_tx(&node_ctx, &account, data).await.unwrap();
    mine_one(&node_ctx).await.unwrap();
    let next_height = get_height(&node_ctx);
    assert_eq!(next_height, height + 1 as u64);
    let tx_header = get_tx_header(&node_ctx, &tx.header.id).unwrap();
    assert_eq!(tx_header, tx.header);
    node_ctx.stop().await;
}
