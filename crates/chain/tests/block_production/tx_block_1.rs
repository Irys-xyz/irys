use irys_testing_utils::initialize_tracing;
use irys_types::{NodeConfig, H256};

use crate::utils::IrysNodeTest;

#[actix_web::test]
#[should_panic]
/// This test is expected to panic
/// create_new_genesis_block() calls get_genesis_commitments()
/// This creates some initial commitments in the genesis block,
/// which are duplicated in this test and cause the panic
async fn commitment_directly_after_genesis_errors() -> () {
    initialize_tracing();
    // config variables
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 15;
    let genesis_block_hash = H256::zero();

    // setup config
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth
        .try_into()
        .expect("low value of usize to fit into u32");

    // genesis node / node_a
    let node_a = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    // post stake and pledge
    let _stake_tx = node_a.post_stake_commitment(genesis_block_hash).await;
    let _pledge_tx = node_a.post_pledge_commitment(genesis_block_hash).await;

    // mine block 1
    node_a.mine_block().await.expect("expect to fail"); // THIS WILL ERROR

    // confirm it was mined
    let _a_block1 = node_a
        .get_block_by_height(1)
        .await
        .expect("expected block 1 to be available");

    // gracefully shutdown node
    node_a.stop().await;

    // return () as we expect a panic
    ()
}
