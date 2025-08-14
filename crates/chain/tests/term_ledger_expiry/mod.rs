use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use irys_types::{
    fee_distribution::TermFeeCharges, irys::IrysSigner, DataLedger, NodeConfig, U256,
};
use tracing::info;

#[test_log::test(actix_web::test)]
async fn heavy_submit_ledger_expiry_single_partition() -> eyre::Result<()> {
    // Configure test parameters
    let chunk_size = 32; // Use 32 byte chunks to match test utilities
    let num_chunks_in_partition = 5; // Very small partition - only 5 chunks to fill it quickly
    let submit_ledger_epoch_length = 2; // Expires after 1 epoch
    let num_blocks_in_epoch = 3; // Short epochs for faster testing

    // Setup node configuration
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = num_blocks_in_epoch;

    // Configure 3 partitions (though we'll only use the first one)
    // Note: num_capacity_partitions is not directly configurable, determined by storage requirements

    // Create test signer with sufficient balance
    let main_signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        main_signer.address(),
        GenesisAccount {
            balance: U256::from(10_000_000_000_000_000_000_u128).into(), // 10 IRYS
            ..Default::default()
        },
    )]);

    // Start the test node and wait for packing to be ready
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("test", 30)
        .await;

    // Get the miner address (using the main signer as the miner for this test)
    let miner_address = main_signer.address();

    // Record initial miner balance
    let genesis_block = node.get_block_by_height(0).await?;
    let initial_balance = U256::from_be_bytes(
        node.get_balance(miner_address, genesis_block.evm_block_hash)
            .to_be_bytes(),
    );
    info!("Initial miner balance: {}", initial_balance);

    // Create and post transactions to fill the first partition and start a second one
    // With num_chunks_in_partition = 5, we need at least 6 transactions to trigger a second slot
    let num_transactions = 7; // Post 7 transactions to ensure second slot is created
    let mut tx_ids = Vec::new();
    let mut transactions = Vec::new();
    let mut total_term_fees = U256::from(0);
    let mut total_perm_fees = U256::from(0);

    for i in 0..num_transactions {
        info!("Posting transaction {}", i);

        // Create transaction data - just 32 bytes
        let data = vec![i as u8; 32];

        // Post transaction to Submit ledger
        let anchor = genesis_block.block_hash;
        let tx = node.post_data_tx(anchor, data, &main_signer).await;

        // Track fees for verification
        total_term_fees = total_term_fees.saturating_add(tx.header.term_fee);
        if let Some(perm_fee) = tx.header.perm_fee {
            total_perm_fees = total_perm_fees.saturating_add(perm_fee);
        }
        tx_ids.push(tx.header.id);
        transactions.push(tx.clone());

        // Wait for transaction to be in mempool
        node.wait_for_mempool(tx.header.id, 10).await?;
    }

    info!(
        "Posted 5 transactions with total term_fees: {}, perm_fees: {}",
        total_term_fees, total_perm_fees
    );

    // Mine first block to include all transactions
    let block1 = node.mine_block().await?;

    // Verify all transactions were included
    let tx_ids_map = block1.get_data_ledger_tx_ids();
    dbg!(&tx_ids_map);
    let submit_txs = tx_ids_map
        .get(&DataLedger::Submit)
        .expect("Submit ledger should have transactions");

    assert_eq!(
        submit_txs.len(),
        num_transactions,
        "All {} transactions should be included",
        num_transactions
    );
    for tx_id in &tx_ids {
        assert!(
            submit_txs.contains(tx_id),
            "Transaction {:?} should be included",
            tx_id
        );
    }

    info!("All transactions included in block 1");

    // Calculate expected fee distribution
    // Block producer gets 5% of term_fee immediately, 95% goes to treasury
    // On expiry, treasury portion is distributed to miners
    let consensus_config = config.consensus_config();
    let mut expected_expiry_fees = U256::from(0);

    // Only the first 5 transactions (in slot 0) should expire
    let expired_tx_count = 5;
    for _ in 0..expired_tx_count {
        // Each transaction's term_fee distribution
        let tx_term_fee = total_term_fees / U256::from(num_transactions); // Assuming equal fees
        let term_charges = TermFeeCharges::new(tx_term_fee, &consensus_config)?;

        // On expiry, the miner gets the treasury portion
        // Since there's only one miner for this partition, they get all of it
        expected_expiry_fees = expected_expiry_fees.saturating_add(term_charges.term_fee_treasury);
    }

    info!("Expected expiry fees for miner: {}", expected_expiry_fees);

    // Get balance after first block (includes block producer rewards)
    let balance_after_block1 = U256::from_be_bytes(
        node.get_balance(miner_address, block1.evm_block_hash)
            .to_be_bytes(),
    );
    info!("Balance after block 1: {}", balance_after_block1);

    // Mine blocks to trigger Submit ledger expiry
    // Submit ledger expires after submit_ledger_epoch_length epochs
    // IMPORTANT: The ledger needs at least 2 slots for expiry to work (slot 0 can't expire if it's the only slot)
    // We need to mine enough blocks to:
    // 1. Create a second slot (happens when the first slot fills up or at epoch boundaries)
    // 2. Then wait for the expiry period

    // With submit_ledger_epoch_length = 1 and num_blocks_in_epoch = 3:
    // - Blocks 0-2: First epoch (slot 0 gets filled)
    // - Block 3: New epoch starts (slot 1 should be created)
    // - After block 3: slot 0 can expire (it's no longer the last slot)

    // We need to mine more blocks to ensure a second slot is created
    // Let's mine to block 6 to ensure we're well past the expiry point
    let expiry_block_height = (submit_ledger_epoch_length * num_blocks_in_epoch * 2 + 1) as u64; // Double the epochs to ensure slot creation
    let blocks_to_mine = (expiry_block_height - 1) as usize; // -1 because we already mined block 1

    info!(
        "Mining {} more blocks to reach expiry at block {}",
        blocks_to_mine, expiry_block_height
    );

    // Mine blocks one by one to track ledger changes
    for height in 2..=expiry_block_height {
        node.mine_block().await?;
        let block = node.get_block_by_height(height).await?;
        let tx_ids = block.get_data_ledger_tx_ids();
        info!(
            "Block {}: Submit: {} txs, Publish: {} txs",
            height,
            tx_ids
                .get(&DataLedger::Submit)
                .map(|v| v.len())
                .unwrap_or(0),
            tx_ids
                .get(&DataLedger::Publish)
                .map(|v| v.len())
                .unwrap_or(0)
        );
    }

    node.wait_until_height(expiry_block_height, 30).await?;

    let expiry_block = node.get_block_by_height(expiry_block_height).await?;
    info!("Reached expiry block at height {}", expiry_block_height);

    // Check what's in the expiry block's ledgers
    let expiry_tx_ids = expiry_block.get_data_ledger_tx_ids();
    info!(
        "Expiry block ledgers: Submit: {:?}, Publish: {:?}",
        expiry_tx_ids.get(&DataLedger::Submit),
        expiry_tx_ids.get(&DataLedger::Publish)
    );

    // Get final balance after expiry
    let final_balance = U256::from_be_bytes(
        node.get_balance(miner_address, expiry_block.evm_block_hash)
            .to_be_bytes(),
    );
    info!("Final miner balance: {}", final_balance);

    // Calculate actual fee received from expiry
    // We need to account for block production rewards between block1 and expiry
    // For simplicity, we'll check that the balance increased by at least the expected expiry fees
    let balance_increase = final_balance.saturating_sub(balance_after_block1);
    info!("Balance increased by: {}", balance_increase);

    // The balance increase should include:
    // 1. Expected expiry fees from the 5 transactions
    // 2. Block production rewards for mining blocks 2 through expiry_block_height
    // Since we can't easily predict exact block rewards, we'll verify the minimum increase
    assert!(
        balance_increase >= expected_expiry_fees,
        "Miner should receive at least {} from expiry fees, but only got {}",
        expected_expiry_fees,
        balance_increase
    );

    info!("✅ Submit ledger expiry fee distribution verified successfully!");

    // Clean up
    node.node_ctx.stop().await;

    Ok(())
}
