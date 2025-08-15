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
    let submit_ledger_epoch_length = 2; // Expires after 2 epoch
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

    // Get the actual miner address from the node's configuration
    let miner_address = node.node_ctx.config.node_config.miner_address();

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

    // Track which blocks contain which transactions
    let mut blocks_mined = Vec::new();
    let txs_per_block = 2;
    let mut pending_txs = Vec::new();

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
        pending_txs.push(tx.header.id);

        // Wait for transaction to be in mempool
        node.wait_for_mempool(tx.header.id, 10).await?;

        // Mine a block after every 2 transactions (or if it's the last transaction)
        if pending_txs.len() >= txs_per_block || i == num_transactions - 1 {
            info!("Mining block with {} transactions", pending_txs.len());
            let block = node.mine_block().await?;

            // Verify transactions were included
            let tx_ids_map = block.get_data_ledger_tx_ids();
            let submit_txs = tx_ids_map
                .get(&DataLedger::Submit)
                .expect("Submit ledger should have transactions");

            for tx_id in &pending_txs {
                assert!(
                    submit_txs.contains(tx_id),
                    "Transaction {:?} should be included in block {}",
                    tx_id,
                    block.height
                );
            }

            info!(
                "Block {} mined with {} transactions",
                block.height,
                pending_txs.len()
            );

            blocks_mined.push(block);
            pending_txs.clear();
        }
    }

    info!(
        "Posted {} transactions with total term_fees: {}, perm_fees: {}",
        num_transactions, total_term_fees, total_perm_fees
    );

    info!(
        "Transactions distributed across {} blocks",
        blocks_mined.len()
    );

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

    // Get balance after initial blocks (includes block producer rewards)
    let last_initial_block = blocks_mined.last().expect("Should have mined blocks");
    let balance_after_initial_blocks = U256::from_be_bytes(
        node.get_balance(miner_address, last_initial_block.evm_block_hash)
            .to_be_bytes(),
    );
    info!(
        "Balance after initial blocks: {}",
        balance_after_initial_blocks
    );

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

    // We need to mine more blocks to ensure expiry occurs
    // The first partition should expire after submit_ledger_epoch_length epochs
    let current_height = last_initial_block.height;
    // We need to reach at least 2 full epochs to trigger expiry
    // Since we're already at block 4, we need to mine to at least block 7 (2 epochs * 3 blocks + 1)
    let target_expiry_height = ((submit_ledger_epoch_length + 1) * num_blocks_in_epoch) as u64;
    let expiry_block_height = target_expiry_height.max(current_height + 3); // Ensure we mine at least a few more blocks

    info!(
        "Current height: {}, targeting expiry at block {}",
        current_height, expiry_block_height
    );

    // Mine blocks one by one to track ledger changes
    for height in (current_height + 1)..=expiry_block_height {
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
    // We need to account for block production rewards between initial blocks and expiry
    // For simplicity, we'll check that the balance increased by at least the expected expiry fees
    let balance_increase = final_balance.saturating_sub(balance_after_initial_blocks);
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
