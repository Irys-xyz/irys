use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_types::{irys::IrysSigner, NodeConfig};
use tracing::info;

use crate::utils::IrysNodeTest;

#[test_log::test(tokio::test)]
async fn heavy_test_wait_until_height() {
    let irys_node = IrysNodeTest::default_async().start().await;
    let height = irys_node.get_canonical_chain_height().await;
    info!("height: {}", height);
    let steps = 2;
    let seconds = 60;
    irys_node.node_ctx.start_mining().unwrap();
    let _block_hash = irys_node
        .wait_until_height(height + steps, seconds)
        .await
        .unwrap();
    let height5 = irys_node.get_canonical_chain_height().await;
    assert!(height5 >= height + steps);
    irys_node.stop().await;
}

#[test_log::test(tokio::test)]
async fn heavy_test_mine() {
    let irys_node = IrysNodeTest::default_async().start().await;
    let height = irys_node.get_canonical_chain_height().await;
    info!("height: {}", height);
    let blocks = 4;
    irys_node.mine_blocks(blocks).await.unwrap();
    let next_height = irys_node.get_canonical_chain_height().await;
    assert_eq!(next_height, height + blocks as u64);
    let block = irys_node.get_block_by_height(next_height).await.unwrap();
    assert_eq!(block.height, next_height);
    irys_node.stop().await;
}

#[test_log::test(tokio::test)]
async fn heavy_test_mine_tx() {
    // output tracing
    let mut config = NodeConfig::testing();
    let account = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        account.address(),
        GenesisAccount {
            balance: U256::from(1_000_000_000_000_000_000_u64),
            ..Default::default()
        },
    )]);
    let irys_node = IrysNodeTest::new_genesis(config.clone()).start().await;

    let height = irys_node.get_canonical_chain_height().await;
    let data = "Hello, world!".as_bytes().to_vec();
    info!("height: {}", height);
    let tx = irys_node
        .post_publish_data_tx(&account, data)
        .await
        .unwrap();
    irys_node.mine_block().await.unwrap();
    let next_height = irys_node.get_canonical_chain_height().await;
    assert_eq!(next_height, height + 1_u64);

    // Wait for mempool to process BlockConfirmed and set included_height.
    let tx_header = irys_node
        .wait_for_tx_included(&tx.header.id, 10)
        .await
        .expect("included_height should be set after block confirmation");

    // Verify the transaction was included at the expected height.
    assert_eq!(
        tx_header.metadata().included_height,
        Some(next_height),
        "Transaction should be included at height {}",
        next_height
    );
    irys_node.stop().await;
}
