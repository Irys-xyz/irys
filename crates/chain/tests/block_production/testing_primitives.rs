use alloy_core::primitives::U256;
use irys_types::irys::IrysSigner;
use reth_primitives::GenesisAccount;
use tracing::info;

use crate::utils::IrysNodeTest;

#[actix::test]
async fn heavy_test_wait_until_height() {
    let irys_node = IrysNodeTest::default_async().await.start().await;
    let height = irys_node.get_height().await;
    info!("height: {}", height);
    let steps = 2;
    let seconds = 60;
    irys_node.node_ctx.actor_addresses.set_mining(true).unwrap();
    irys_node
        .wait_until_height(height + steps, seconds)
        .await
        .unwrap();
    let height5 = irys_node.get_height().await;
    assert_eq!(height5, height + steps);
    irys_node.stop().await;
}

#[actix::test]
async fn heavy_test_mine() {
    let irys_node = IrysNodeTest::default_async().await.start().await;
    let height = irys_node.get_height().await;
    info!("height: {}", height);
    let blocks = 4;
    irys_node.mine_blocks(blocks).await.unwrap();
    let next_height = irys_node.get_height().await;
    assert_eq!(next_height, height + blocks as u64);
    let block = irys_node.get_block_by_height(next_height).await.unwrap();
    assert_eq!(block.height, next_height);
    irys_node.stop().await;
}

#[actix::test]
async fn heavy_test_mine_tx() {
    let mut irys_node = IrysNodeTest::default_async().await;
    let account = IrysSigner::random_signer(&irys_node.cfg.config);
    irys_node
        .cfg
        .irys_node_config
        .extend_genesis_accounts(vec![(
            account.address(),
            GenesisAccount {
                balance: U256::from(1000),
                ..Default::default()
            },
        )]);
    let irys_node = irys_node.start().await;

    let height = irys_node.get_height().await;
    let data = "Hello, world!".as_bytes().to_vec();
    info!("height: {}", height);
    let tx = irys_node
        .create_submit_data_tx(&account, data)
        .await
        .unwrap();
    irys_node.mine_block().await.unwrap();
    let next_height = irys_node.get_height().await;
    assert_eq!(next_height, height + 1 as u64);
    let tx_header = irys_node.get_tx_header(&tx.header.id).unwrap();
    assert_eq!(tx_header, tx.header);
    irys_node.stop().await;
}
