use crate::utils::IrysNodeTest;
use irys_testing_utils::initialize_tracing;
use irys_types::{
    irys::IrysSigner,
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
    CommitmentTransaction, NodeConfig, UnixTimestamp,
};
use tracing::info;

/// Test that verifies the treasury field is correctly tracked across blocks
#[tokio::test]
async fn heavy_test_treasury_tracking() -> eyre::Result<()> {
    initialize_tracing();

    // ===== SETUP =====
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().block_migration_depth = 5;

    let user1_signer = IrysSigner::random_signer(&config.consensus_config());
    let user2_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&user1_signer, &user2_signer]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", 10)
        .await;

    let consensus_config = config.consensus_config();

    // ===== ACTION =====
    // Collect all blocks and calculate expected treasury changes

    // Block 0: Genesis
    let block0 = node.get_block_by_height(0).await?;

    // Block 1: Stake commitment
    let stake_tx = {
        let mut commitment =
            CommitmentTransaction::new_stake(&consensus_config, node.get_anchor().await?);
        user1_signer.sign_commitment(&mut commitment)?;
        commitment
    };
    node.post_commitment_tx(&stake_tx).await?;
    node.wait_for_mempool(stake_tx.id(), 10).await?;
    node.mine_block().await?;
    let block1 = node.get_block_by_height(1).await?;

    // Block 2: Pledge commitment
    let pledge_tx = node.post_pledge_commitment_with_signer(&user1_signer).await;
    node.wait_for_mempool(pledge_tx.id(), 10).await?;
    node.mine_block().await?;
    let block2 = node.get_block_by_height(2).await?;

    // Block 3: Data transaction (1KB)
    let data_tx1 = node
        .post_data_tx(node.get_anchor().await?, vec![1_u8; 1024], &user2_signer)
        .await;
    node.wait_for_mempool(data_tx1.header.id(), 10).await?;
    node.mine_block().await?;
    let block3 = node.get_block_by_height(3).await?;

    // Block 4: Multiple transactions (stake + 2KB data tx)
    let stake_tx2 = {
        let mut commitment =
            CommitmentTransaction::new_stake(&consensus_config, node.get_anchor().await?);
        user2_signer.sign_commitment(&mut commitment)?;
        commitment
    };
    node.post_commitment_tx(&stake_tx2).await?;

    let data_tx2 = node
        .post_data_tx(node.get_anchor().await?, vec![2_u8; 2048], &user1_signer)
        .await;

    node.wait_for_mempool(stake_tx2.id(), 10).await?;
    node.wait_for_mempool(data_tx2.header.id, 10).await?;
    node.mine_block().await?;
    let block4 = node.get_block_by_height(4).await?;

    // Block 5: Empty block
    node.mine_block().await?;
    let block5 = node.get_block_by_height(5).await?;

    // ===== ASSERT =====

    // Calculate exact expected treasury values for each block

    // Block 0: Genesis treasury from initial commitments
    // Genesis has 1 stake + 3 pledges (with decay)
    let genesis_stake = consensus_config.stake_value.amount;
    let genesis_pledge_0 =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 0);
    let genesis_pledge_1 =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 1);
    let genesis_pledge_2 =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 2);
    let expected_genesis_treasury =
        genesis_stake + genesis_pledge_0 + genesis_pledge_1 + genesis_pledge_2;

    assert_eq!(
        block0.treasury, expected_genesis_treasury,
        "Genesis treasury should equal sum of initial commitments"
    );

    // Block 1: Genesis treasury + stake commitment
    let stake1_value = consensus_config.stake_value.amount;
    let expected_block1_treasury = expected_genesis_treasury + stake1_value;

    assert_eq!(
        block1.treasury, expected_block1_treasury,
        "Block 1 treasury should be genesis + stake value"
    );

    // Block 2: Previous treasury + pledge commitment
    // User1 has 1 stake, so this is pledge at index 0
    let pledge1_value =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 0);
    let expected_block2_treasury = expected_block1_treasury + pledge1_value;

    assert_eq!(
        block2.treasury, expected_block2_treasury,
        "Block 2 treasury should be previous + pledge value"
    );

    // Block 3: Previous treasury + data tx fees
    let number_of_ingress_proofs_total = node
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let data_tx1_fees = {
        let term_charges = TermFeeCharges::new(data_tx1.header.term_fee, &consensus_config)?;
        let publish_charges = PublishFeeCharges::new(
            data_tx1.header.perm_fee.unwrap(),
            data_tx1.header.term_fee,
            &consensus_config,
            number_of_ingress_proofs_total,
        )?;
        term_charges.term_fee_treasury + publish_charges.perm_fee_treasury
    };
    let expected_block3_treasury = expected_block2_treasury + data_tx1_fees;

    assert_eq!(
        block3.treasury, expected_block3_treasury,
        "Block 3 treasury should be previous + data tx fees"
    );

    // Block 4: Previous treasury + stake2 + data tx2 fees
    let stake2_value = consensus_config.stake_value.amount;
    let data_tx2_fees = {
        let term_charges = TermFeeCharges::new(data_tx2.header.term_fee, &consensus_config)?;
        let publish_charges = PublishFeeCharges::new(
            data_tx2.header.perm_fee.unwrap(),
            data_tx2.header.term_fee,
            &consensus_config,
            number_of_ingress_proofs_total,
        )?;
        term_charges.term_fee_treasury + publish_charges.perm_fee_treasury
    };
    let expected_block4_treasury = expected_block3_treasury + stake2_value + data_tx2_fees;

    assert_eq!(
        block4.treasury, expected_block4_treasury,
        "Block 4 treasury should be previous + stake + data tx fees"
    );

    // Block 5: Empty block - treasury unchanged
    let expected_block5_treasury = expected_block4_treasury;

    assert_eq!(
        block5.treasury, expected_block5_treasury,
        "Block 5 treasury should remain unchanged (empty block)"
    );

    info!("Treasury tracking test completed: all assertions passed");

    node.stop().await;
    Ok(())
}

/// Test that verifies treasury is correctly initialized from genesis commitments
#[tokio::test]
async fn heavy_test_genesis_treasury_calculation() -> eyre::Result<()> {
    initialize_tracing();

    // Configure test network
    let config = NodeConfig::testing();

    // Start the node
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", 10)
        .await;

    // Get the genesis block
    let genesis_block = node.get_block_by_height(0).await?;

    // Calculate expected treasury from genesis commitments
    // In test environment with 3 storage submodules:
    // - 1 stake commitment
    // - 3 pledge commitments with decay

    let consensus_config = config.consensus_config();

    // Stake value is constant from config
    let stake_value = consensus_config.stake_value.amount;

    // Calculate pledge values using the same function as production code
    // Pledge indices are 0, 1, 2 for the 3 pledges
    let pledge_0_value =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 0);
    let pledge_1_value =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 1);
    let pledge_2_value =
        CommitmentTransaction::calculate_pledge_value_at_count(&consensus_config, 2);

    let expected_treasury = stake_value + pledge_0_value + pledge_1_value + pledge_2_value;

    info!("Genesis block treasury: {}", genesis_block.treasury);
    info!("Expected treasury: {}", expected_treasury);
    info!("Stake value: {}", stake_value);
    info!("Pledge 0 value: {}", pledge_0_value);
    info!("Pledge 1 value: {}", pledge_1_value);
    info!("Pledge 2 value: {}", pledge_2_value);

    // Verify treasury exactly matches expected value
    assert_eq!(
        genesis_block.treasury, expected_treasury,
        "Genesis treasury should exactly match the sum of all commitment values"
    );

    // Get commitment ledger to verify the treasury matches commitment values
    let commitment_tx_ids = genesis_block.commitment_tx_ids();
    info!(
        "Genesis has {} commitment transactions",
        commitment_tx_ids.len()
    );

    // In test setup, we expect 4 commitments (1 stake + 3 pledges)
    assert_eq!(
        commitment_tx_ids.len(),
        4,
        "Genesis should have 1 stake and 3 pledge commitments"
    );

    // Clean up
    node.stop().await;

    Ok(())
}
