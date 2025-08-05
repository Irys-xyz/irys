use crate::utils::IrysNodeTest;
use eyre::Result;
use irys_actors::block_validation::MAX_FUTURE_TIMESTAMP_DRIFT_MILLISECONDS;
use irys_reward_curve::HalvingCurve;
use irys_types::NodeConfig;
use std::time::{SystemTime, UNIX_EPOCH};

/// This test ensures that if we attempt to submit a block with a timestamp
/// too far in the future, the node rejects it.
#[actix_web::test]
async fn heavy_test_future_block_rejection() -> Result<()> {
    // 1. Start a node to create a block, and the na second node to consume that block,
    //    both with with default config
    let genesis_config = NodeConfig::testing();
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    // disable gossip
    genesis_node.gossip_disable();

    let peer1_signer = genesis_config.new_random_signer();
    let peer1_config = genesis_node.testing_peer_with_signer(&peer1_signer);

    // Start the peers: No packing on the peers, they don't have partition assignments yet
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;
    peer1_node.start_public_api().await;

    // mine a block
    let block_1 = genesis_node.mine_block().await?;

    // 2. Modify block with a timestamp too far in the future
    //    i.e. just outside the exceptable drift
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let future_timestamp = now_ms + MAX_FUTURE_TIMESTAMP_DRIFT_MILLISECONDS + 10_000; // too far into the future

    // creating artificially future-dated block header
    let mut invalid_block = block_1;
    invalid_block.timestamp = future_timestamp;

    // manually adjust reward to match new timestamp so only timestamp validation fails
    let previous_block = genesis_node.get_block_by_height(0).await?;
    let consensus_config = genesis_config.consensus_config();
    let reward_curve = HalvingCurve {
        inflation_cap: consensus_config.block_reward_config.inflation_cap,
        half_life_secs: consensus_config.block_reward_config.half_life_secs.into(),
    };
    let reward =
        reward_curve.reward_between(previous_block.timestamp / 1000, future_timestamp / 1000)?;
    invalid_block.reward_amount = reward.amount;

    // resign block so that signature remains valid after tampering
    let signer = genesis_config.signer();
    signer.sign_block_header(&mut invalid_block)?;

    // 3. ask node to accept and validate block
    let block_validation_result = genesis_node
        .send_block_to_peer(&peer1_node, &invalid_block)
        .await;

    // block should be rejected
    assert!(
        block_validation_result.is_err(),
        "Expected block to be rejected due to BlockValidationError"
    );
    // specifically, block should be rejected due to timestamp being too far in the future
    assert!(format!("{:?}", block_validation_result).contains("too far in the future"));

    // 4. Shut down the nodes
    genesis_node.stop().await;
    peer1_node.stop().await;
    Ok(())
}
