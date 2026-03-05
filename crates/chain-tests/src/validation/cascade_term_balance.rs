use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{
    hardfork_config::Cascade, BoundedFee, DataLedger, NodeConfig, UnixTimestamp, U256,
};

/// Start a node, fund two wallets, post data transactions to both the 30-day
/// and 1-year term ledgers, then verify that on-chain wallet balances are
/// decremented by exactly the term_fee charged for each transaction.
#[test_log::test(tokio::test)]
async fn heavy_term_ledger_balance_decrement() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 8,
            thirty_day_epoch_length: 2,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    // Fund two separate wallets
    let wallet_a = config.new_random_signer();
    let wallet_b = config.new_random_signer();
    config.fund_genesis_accounts(vec![&wallet_a, &wallet_b]);

    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let ctx = test_node.start_and_wait_for_packing("test", 30).await;

    // Mine past cascade activation
    let activation_height = num_blocks_in_epoch;
    while ctx.get_canonical_chain_height().await <= activation_height {
        ctx.mine_block().await?;
    }

    // Record initial balances after activation mining
    let pre_block = ctx.get_max_difficulty_block();
    let balance_a_before = ctx
        .get_balance(wallet_a.address(), pre_block.evm_block_hash)
        .await;
    let balance_b_before = ctx
        .get_balance(wallet_b.address(), pre_block.evm_block_hash)
        .await;

    assert!(
        balance_a_before > U256::zero(),
        "wallet_a must have a funded balance"
    );
    assert!(
        balance_b_before > U256::zero(),
        "wallet_b must have a funded balance"
    );

    let data = vec![9_u8; 96]; // 3 chunks of 32 bytes
    let data_size = data.len() as u64;

    // wallet_a posts a OneYear tx
    let price_1y = ctx.get_data_price(DataLedger::OneYear, data_size).await?;
    let tx_1y = wallet_a.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::OneYear,
        BoundedFee::new(price_1y.term_fee),
        None,
    )?;
    let tx_1y = wallet_a.sign_transaction(tx_1y)?;
    let tx_1y_cost = tx_1y.header.term_fee.get();
    ctx.ingest_data_tx(tx_1y.header.clone()).await?;
    ctx.wait_for_mempool(tx_1y.header.id, 30).await?;

    // wallet_b posts a ThirtyDay tx
    let mut data_30d = data.clone();
    data_30d[0] = 42; // unique data to get a different tx id
    let price_30d = ctx.get_data_price(DataLedger::ThirtyDay, data_size).await?;
    let tx_30d = wallet_b.create_transaction_with_fees(
        data_30d,
        ctx.get_anchor().await?,
        DataLedger::ThirtyDay,
        BoundedFee::new(price_30d.term_fee),
        None,
    )?;
    let tx_30d = wallet_b.sign_transaction(tx_30d)?;
    let tx_30d_cost = tx_30d.header.term_fee.get();
    ctx.ingest_data_tx(tx_30d.header.clone()).await?;
    ctx.wait_for_mempool(tx_30d.header.id, 30).await?;

    // Mine a block to include both transactions
    let inclusion_block = ctx.mine_block().await?;

    // Check balances after the inclusion block
    let balance_a_after = ctx
        .get_balance(wallet_a.address(), inclusion_block.evm_block_hash)
        .await;
    let balance_b_after = ctx
        .get_balance(wallet_b.address(), inclusion_block.evm_block_hash)
        .await;

    // Each wallet should have been debited by the term_fee of its transaction
    let spent_a = balance_a_before - balance_a_after;
    let spent_b = balance_b_before - balance_b_after;

    assert_eq!(
        spent_a, tx_1y_cost,
        "wallet_a balance should decrease by exactly the OneYear term_fee (spent={}, expected={})",
        spent_a, tx_1y_cost
    );
    assert_eq!(
        spent_b, tx_30d_cost,
        "wallet_b balance should decrease by exactly the ThirtyDay term_fee (spent={}, expected={})",
        spent_b, tx_30d_cost
    );

    // Verify balances are still positive (sanity check, not drained)
    assert!(
        balance_a_after > U256::zero(),
        "wallet_a should still have remaining balance"
    );
    assert!(
        balance_b_after > U256::zero(),
        "wallet_b should still have remaining balance"
    );

    ctx.stop().await;
    Ok(())
}

/// Post multiple transactions from the same wallet to both term ledgers and
/// verify cumulative balance decrement matches the sum of all term_fees.
#[test_log::test(tokio::test)]
async fn heavy_term_ledger_multi_tx_balance() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.block_migration_depth = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 8,
            thirty_day_epoch_length: 2,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let wallet = config.new_random_signer();
    config.fund_genesis_accounts(vec![&wallet]);

    let test_node = IrysNodeTest::new_genesis(config);
    StorageSubmodulesConfig::load_for_test(test_node.cfg.base_directory.clone(), 5)?;
    let ctx = test_node.start_and_wait_for_packing("test", 30).await;

    // Mine past cascade activation
    while ctx.get_canonical_chain_height().await <= num_blocks_in_epoch {
        ctx.mine_block().await?;
    }

    let pre_block = ctx.get_max_difficulty_block();
    let balance_before = ctx
        .get_balance(wallet.address(), pre_block.evm_block_hash)
        .await;

    let data = vec![9_u8; 96];
    let data_size = data.len() as u64;
    let mut total_cost = U256::zero();

    // Post 2 OneYear txs
    for i in 0_u8..2 {
        let mut tx_data = data.clone();
        tx_data[0] = i;
        let price = ctx.get_data_price(DataLedger::OneYear, data_size).await?;
        let tx = wallet.create_transaction_with_fees(
            tx_data,
            ctx.get_anchor().await?,
            DataLedger::OneYear,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = wallet.sign_transaction(tx)?;
        total_cost += tx.header.term_fee.get();
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }

    // Post 2 ThirtyDay txs
    for i in 0_u8..2 {
        let mut tx_data = data.clone();
        tx_data[0] = 100 + i;
        let price = ctx.get_data_price(DataLedger::ThirtyDay, data_size).await?;
        let tx = wallet.create_transaction_with_fees(
            tx_data,
            ctx.get_anchor().await?,
            DataLedger::ThirtyDay,
            BoundedFee::new(price.term_fee),
            None,
        )?;
        let tx = wallet.sign_transaction(tx)?;
        total_cost += tx.header.term_fee.get();
        ctx.ingest_data_tx(tx.header.clone()).await?;
        ctx.wait_for_mempool(tx.header.id, 30).await?;
    }

    // Mine a block to include all txs
    let inclusion_block = ctx.mine_block().await?;

    let balance_after = ctx
        .get_balance(wallet.address(), inclusion_block.evm_block_hash)
        .await;

    let spent = balance_before - balance_after;
    assert_eq!(
        spent, total_cost,
        "wallet balance should decrease by sum of all term_fees (spent={}, expected={})",
        spent, total_cost
    );

    ctx.stop().await;
    Ok(())
}
