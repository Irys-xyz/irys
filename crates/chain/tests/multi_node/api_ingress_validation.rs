use crate::utils::*;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_actors::mempool_service::TxIngressError;
use irys_types::{BoundedFee, DataLedger, NodeConfig};

/// Test that API rejects data transactions with insufficient term fee
/// This validates the EMA pricing check that uses `ema_for_public_pricing()`
#[test_log::test(tokio::test)]
async fn test_api_rejects_underpriced_term_fee() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create test data
    let data = vec![42_u8; 96]; // 3 chunks of 32 bytes each
    let data_size = data.len() as u64;

    // Get the correct price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await
        .expect("Failed to get price");

    tracing::info!(
        "Expected fees - perm_fee: {}, term_fee: {}",
        price_info.perm_fee,
        price_info.term_fee
    );

    // Create transaction with INSUFFICIENT term_fee (10% below expected)
    let insufficient_term_fee = BoundedFee::new((price_info.term_fee * 9) / 10);

    let tx = signer
        .create_publish_transaction(
            data,
            genesis_node.get_anchor().await?,
            price_info.perm_fee.into(), // perm_fee is correct
            insufficient_term_fee,      // term_fee is too low
        )
        .expect("Expect to create a storage transaction");
    let tx = signer
        .sign_transaction(tx)
        .expect("to sign the storage transaction");

    tracing::info!(
        "Attempting to ingest tx with insufficient term_fee: {} (expected: {})",
        insufficient_term_fee,
        price_info.term_fee
    );

    // Try to ingest via API - should be rejected
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    // Assert that it was rejected with the appropriate error
    match result {
        Err(AddTxError::TxIngress(TxIngressError::FundMisalignment(msg))) => {
            assert!(
                msg.contains("Insufficient term fee"),
                "Expected 'Insufficient term fee' error, got: {}",
                msg
            );
            tracing::info!("Transaction correctly rejected: {}", msg);
        }
        Ok(_) => {
            panic!("Expected transaction to be rejected but it was accepted");
        }
        Err(e) => {
            panic!(
                "Expected TxIngressError::FundMisalignment with 'Insufficient term fee', got: {:?}",
                e
            );
        }
    }

    genesis_node.stop().await;
    Ok(())
}

/// Test that API rejects data transactions with insufficient perm fee
/// This validates the EMA pricing check for permanent storage fees
#[test_log::test(tokio::test)]
async fn test_api_rejects_underpriced_perm_fee() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create test data
    let data = vec![42_u8; 96]; // 3 chunks of 32 bytes each
    let data_size = data.len() as u64;

    // Get the correct price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await
        .expect("Failed to get price");

    tracing::info!(
        "Expected fees - perm_fee: {}, term_fee: {}",
        price_info.perm_fee,
        price_info.term_fee
    );

    // Create transaction with INSUFFICIENT perm_fee (10% below expected)
    let insufficient_perm_fee = BoundedFee::new((price_info.perm_fee * 9) / 10);

    let tx = signer
        .create_publish_transaction(
            data,
            genesis_node.get_anchor().await?,
            insufficient_perm_fee,      // perm_fee is too low
            price_info.term_fee.into(), // term_fee is correct
        )
        .expect("Expect to create a storage transaction");
    let tx = signer
        .sign_transaction(tx)
        .expect("to sign the storage transaction");

    tracing::info!(
        "Attempting to ingest tx with insufficient perm_fee: {} (expected: {})",
        insufficient_perm_fee,
        price_info.perm_fee
    );

    // Try to ingest via API - should be rejected
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    // Assert that it was rejected with the appropriate error
    match result {
        Err(AddTxError::TxIngress(TxIngressError::FundMisalignment(msg))) => {
            assert!(
                msg.contains("Insufficient perm fee"),
                "Expected 'Insufficient perm fee' error, got: {}",
                msg
            );
            tracing::info!("Transaction correctly rejected: {}", msg);
        }
        Ok(_) => {
            panic!("Expected transaction to be rejected but it was accepted");
        }
        Err(e) => {
            panic!(
                "Expected TxIngressError::FundMisalignment with 'Insufficient perm fee', got: {:?}",
                e
            );
        }
    }

    genesis_node.stop().await;
    Ok(())
}

/// Test that API rejects data transactions when user has insufficient funds
#[test_log::test(tokio::test)]
async fn test_api_rejects_insufficient_funds() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();

    // Create a signer but fund it with INSUFFICIENT amount
    let signer = genesis_config.new_random_signer();

    // Fund with only 1 token (way too little for any transaction)
    let insufficient_balance = U256::from(1);
    genesis_config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: insufficient_balance,
            ..Default::default()
        },
    )]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create test data
    let data = vec![42_u8; 96]; // 3 chunks of 32 bytes each
    let data_size = data.len() as u64;

    // Get the required price (which will be much more than our balance)
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await
        .expect("Failed to get price");

    let required_cost = price_info.perm_fee + price_info.term_fee;

    tracing::info!(
        "Account balance: {}, Required cost: {} (perm: {}, term: {})",
        insufficient_balance,
        required_cost,
        price_info.perm_fee,
        price_info.term_fee
    );

    let required_cost_u256: U256 = required_cost.into();
    assert!(
        insufficient_balance < required_cost_u256,
        "Balance should be insufficient for the transaction"
    );

    // Create transaction with correct fees but insufficient funds
    let tx = signer
        .create_publish_transaction(
            data,
            genesis_node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .expect("Expect to create a storage transaction");
    let tx = signer
        .sign_transaction(tx)
        .expect("to sign the storage transaction");

    tracing::info!("Attempting to ingest tx with insufficient account balance");

    // Try to ingest via API - should be rejected due to insufficient funds
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    // Assert that it was rejected with Unfunded error
    match result {
        Err(AddTxError::TxIngress(TxIngressError::Unfunded(tx_id))) => {
            tracing::info!(
                "Transaction {} correctly rejected with Unfunded error",
                tx_id
            );
        }
        Ok(_) => {
            panic!(
                "Expected transaction to be rejected due to insufficient funds but it was accepted"
            );
        }
        Err(e) => {
            panic!("Expected TxIngressError::Unfunded, got: {:?}", e);
        }
    }

    genesis_node.stop().await;
    Ok(())
}

/// Test that API rejects when user has zero balance
/// Edge case test for the balance validation
#[test_log::test(tokio::test)]
async fn test_api_rejects_zero_balance() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();

    // Create a signer with ZERO balance
    let signer = genesis_config.new_random_signer();

    genesis_config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::ZERO,
            ..Default::default()
        },
    )]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create test data
    let data = vec![42_u8; 32]; // Single chunk

    // Get the required price
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    tracing::info!(
        "Account balance: 0, Required cost: {} (perm: {}, term: {})",
        price_info.perm_fee + price_info.term_fee,
        price_info.perm_fee,
        price_info.term_fee
    );

    // Create transaction
    let tx = signer
        .create_publish_transaction(
            data,
            genesis_node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .expect("Expect to create a storage transaction");
    let tx = signer
        .sign_transaction(tx)
        .expect("to sign the storage transaction");

    // Try to ingest via API - should be rejected
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    // Assert that it was rejected with Unfunded error
    match result {
        Err(AddTxError::TxIngress(TxIngressError::Unfunded(tx_id))) => {
            tracing::info!(
                "Transaction {} correctly rejected with Unfunded error",
                tx_id
            );
        }
        Ok(_) => {
            panic!("Expected transaction to be rejected due to zero balance but it was accepted");
        }
        Err(e) => {
            panic!("Expected TxIngressError::Unfunded, got: {:?}", e);
        }
    }

    genesis_node.stop().await;
    Ok(())
}

/// Test that API accepts transactions with EXACT required fees
/// Validates that the boundary condition works correctly
#[test_log::test(tokio::test)]
async fn test_api_accepts_exact_fees() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create test data
    let data = vec![42_u8; 96];
    let data_size = data.len() as u64;

    // Get the correct price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await
        .expect("Failed to get price");

    tracing::info!(
        "Using EXACT fees - perm_fee: {}, term_fee: {}",
        price_info.perm_fee,
        price_info.term_fee
    );

    // Create transaction with EXACT required fees
    let tx = signer
        .create_publish_transaction(
            data,
            genesis_node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .expect("Expect to create a storage transaction");
    let tx = signer
        .sign_transaction(tx)
        .expect("to sign the storage transaction");

    // Ingest via API - should be accepted
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    // Assert that it was accepted
    assert!(
        result.is_ok(),
        "Expected transaction with exact fees to be accepted, got: {:?}",
        result.err()
    );

    tracing::info!("Transaction with exact fees correctly accepted");

    // Wait for it to appear in mempool
    genesis_node
        .wait_for_mempool(tx.header.id, 10)
        .await
        .expect("Transaction should be in mempool");

    genesis_node.stop().await;
    Ok(())
}

/// Test that API accepts transactions with HIGHER than required fees
/// Users should be able to pay more if they want to
#[test_log::test(tokio::test)]
async fn test_api_accepts_higher_fees() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create test data
    let data = vec![42_u8; 96];
    let data_size = data.len() as u64;

    // Get the correct price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data_size)
        .await
        .expect("Failed to get price");

    // Use 150% of required fees
    let higher_perm_fee = BoundedFee::new((price_info.perm_fee * 15) / 10);
    let higher_term_fee = BoundedFee::new((price_info.term_fee * 15) / 10);

    tracing::info!(
        "Using HIGHER fees - perm_fee: {} (required: {}), term_fee: {} (required: {})",
        higher_perm_fee,
        price_info.perm_fee,
        higher_term_fee,
        price_info.term_fee
    );

    // Create transaction with HIGHER fees
    let tx = signer
        .create_publish_transaction(
            data,
            genesis_node.get_anchor().await?,
            higher_perm_fee,
            higher_term_fee,
        )
        .expect("Expect to create a storage transaction");
    let tx = signer
        .sign_transaction(tx)
        .expect("to sign the storage transaction");

    // Ingest via API - should be accepted
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    // Assert that it was accepted
    assert!(
        result.is_ok(),
        "Expected transaction with higher fees to be accepted, got: {:?}",
        result.err()
    );

    tracing::info!("Transaction with higher fees correctly accepted");

    // Wait for it to appear in mempool
    genesis_node
        .wait_for_mempool(tx.header.id, 10)
        .await
        .expect("Transaction should be in mempool");

    genesis_node.stop().await;
    Ok(())
}

/// Test mempool ingress behavior at the Cascade hardfork boundary for OneYear/ThirtyDay term ledgers.
/// Pre-Cascade: term ledger transactions are rejected (InvalidLedger).
/// Post-Cascade: term ledger transactions with correct fees are accepted;
/// those with perm_fee or insufficient term_fee are rejected.
#[test_log::test(tokio::test)]
async fn heavy_cascade_mempool_ingress_boundary_for_term_ledgers() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;

    let num_blocks_in_epoch = 4_u64;
    let activation_height = num_blocks_in_epoch;

    let mut config = IrysNodeTest::default_async().cfg.with_consensus(|c| {
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.hardforks.cascade = Some(Cascade {
            activation_height,
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    let data = vec![7_u8; 96];
    let data_size = data.len() as u64;

    // Pre-Cascade: OneYear should be rejected as invalid ledger
    let one_year_pre_tx = signer.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::OneYear,
        BoundedFee::new(U256::from(1_000).into()),
        None,
    )?;
    let one_year_pre_tx = signer.sign_transaction(one_year_pre_tx)?;
    match ctx.ingest_data_tx(one_year_pre_tx.header.clone()).await {
        Err(AddTxError::TxIngress(TxIngressError::InvalidLedger(ledger_id))) => {
            assert_eq!(ledger_id, DataLedger::OneYear as u32);
        }
        Ok(_) => panic!("Expected pre-Cascade OneYear tx to be rejected"),
        Err(e) => panic!(
            "Expected InvalidLedger for pre-Cascade OneYear tx, got: {:?}",
            e
        ),
    }

    // Pre-Cascade: ThirtyDay should be rejected as invalid ledger
    let thirty_day_pre_tx = signer.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::ThirtyDay,
        BoundedFee::new(U256::from(1_000).into()),
        None,
    )?;
    let thirty_day_pre_tx = signer.sign_transaction(thirty_day_pre_tx)?;
    match ctx.ingest_data_tx(thirty_day_pre_tx.header.clone()).await {
        Err(AddTxError::TxIngress(TxIngressError::InvalidLedger(ledger_id))) => {
            assert_eq!(ledger_id, DataLedger::ThirtyDay as u32);
        }
        Ok(_) => panic!("Expected pre-Cascade ThirtyDay tx to be rejected"),
        Err(e) => panic!(
            "Expected InvalidLedger for pre-Cascade ThirtyDay tx, got: {:?}",
            e
        ),
    }

    // Mine until Cascade activates
    while ctx.get_canonical_chain_height().await < activation_height {
        ctx.mine_block().await?;
    }

    // Post-Cascade: OneYear with correct fees should succeed
    let one_year_price = ctx.get_data_price(DataLedger::OneYear, data_size).await?;
    let one_year_ok_tx = signer.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::OneYear,
        one_year_price.term_fee.into(),
        None,
    )?;
    let one_year_ok_tx = signer.sign_transaction(one_year_ok_tx)?;
    let one_year_ok_result = ctx.ingest_data_tx(one_year_ok_tx.header.clone()).await;
    assert!(
        one_year_ok_result.is_ok(),
        "Expected post-Cascade OneYear tx to be accepted, got: {:?}",
        one_year_ok_result
    );

    // Post-Cascade: ThirtyDay with correct fees should succeed
    let thirty_day_price = ctx.get_data_price(DataLedger::ThirtyDay, data_size).await?;
    let thirty_day_ok_tx = signer.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::ThirtyDay,
        thirty_day_price.term_fee.into(),
        None,
    )?;
    let thirty_day_ok_tx = signer.sign_transaction(thirty_day_ok_tx)?;
    let thirty_day_ok_result = ctx.ingest_data_tx(thirty_day_ok_tx.header.clone()).await;
    assert!(
        thirty_day_ok_result.is_ok(),
        "Expected post-Cascade ThirtyDay tx to be accepted, got: {:?}",
        thirty_day_ok_result
    );

    // Post-Cascade: OneYear must not have perm_fee
    let one_year_with_perm_tx = signer.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::OneYear,
        one_year_price.term_fee.into(),
        Some(BoundedFee::new(U256::from(1).into())),
    )?;
    let one_year_with_perm_tx = signer.sign_transaction(one_year_with_perm_tx)?;
    match ctx
        .ingest_data_tx(one_year_with_perm_tx.header.clone())
        .await
    {
        Err(AddTxError::TxIngress(TxIngressError::FundMisalignment(msg))) => {
            assert!(
                msg.contains("must not have a perm_fee"),
                "Expected perm_fee rejection message, got: {}",
                msg
            );
        }
        Ok(_) => panic!("Expected OneYear tx with perm_fee to be rejected"),
        Err(e) => panic!(
            "Expected FundMisalignment for OneYear tx with perm_fee, got: {:?}",
            e
        ),
    }

    // Post-Cascade: OneYear with insufficient term_fee should fail
    assert!(
        one_year_price.term_fee > irys_types::U256::from(0),
        "Expected OneYear term fee to be greater than zero after Cascade activation"
    );
    let insufficient_one_year_term_fee =
        BoundedFee::new(one_year_price.term_fee - irys_types::U256::from(1));
    let one_year_insufficient_term_tx = signer.create_transaction_with_fees(
        data,
        ctx.get_anchor().await?,
        DataLedger::OneYear,
        insufficient_one_year_term_fee,
        None,
    )?;
    let one_year_insufficient_term_tx = signer.sign_transaction(one_year_insufficient_term_tx)?;
    match ctx
        .ingest_data_tx(one_year_insufficient_term_tx.header.clone())
        .await
    {
        Err(AddTxError::TxIngress(TxIngressError::FundMisalignment(msg))) => {
            assert!(
                msg.contains("Insufficient term fee"),
                "Expected insufficient term fee message, got: {}",
                msg
            );
        }
        Ok(_) => panic!("Expected OneYear tx with insufficient term_fee to be rejected"),
        Err(e) => panic!(
            "Expected FundMisalignment for OneYear tx with insufficient term_fee, got: {:?}",
            e
        ),
    }

    ctx.stop().await;
    Ok(())
}

/// Verify that after Cascade activates, the mempool rejects Publish txs priced at
/// the pre-Cascade annual_cost_per_gb ($0.01/GB/year) and accepts txs priced at
/// the post-Cascade rate ($0.028/GB/year).
#[test_log::test(tokio::test)]
async fn heavy_cascade_mempool_rejects_publish_tx_with_pre_cascade_pricing() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;
    use irys_types::storage_pricing::calculate_term_fee_from_config;
    use irys_types::UnixTimestamp;
    use rust_decimal_macros::dec;

    let num_blocks_in_epoch = 4_u64;
    let activation_height = num_blocks_in_epoch;

    let mut config = IrysNodeTest::default_async().cfg.with_consensus(|c| {
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        // Disable minimum term fee so the cost-basis difference is observable
        c.minimum_term_fee_usd = irys_types::storage_pricing::Amount::token(dec!(0)).unwrap();
        c.hardforks.cascade = Some(Cascade {
            activation_height,
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });

    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // Use enough data so the fee is well above any minimum floor
    let data_size = ctx.node_ctx.config.consensus.chunk_size * 100;

    // Mine past Cascade activation
    while ctx.get_canonical_chain_height().await < activation_height {
        ctx.mine_block().await?;
    }

    let next_height = ctx.get_canonical_chain_height().await + 1;

    // Manually compute the term fee at height 0 (pre-Cascade base cost)
    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let pre_cascade_term_fee = calculate_term_fee_from_config(
        data_size,
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        0, // pre-Cascade height
    )?;

    // Compute the correct post-Cascade term fee
    let post_cascade_term_fee = calculate_term_fee_from_config(
        data_size,
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        next_height,
    )?;

    // Sanity: post > pre
    assert!(
        post_cascade_term_fee > pre_cascade_term_fee,
        "Post-Cascade fee ({}) should exceed pre-Cascade fee ({})",
        post_cascade_term_fee,
        pre_cascade_term_fee
    );

    // Create a Publish tx with the *pre-Cascade* term fee — should be rejected
    let data = vec![7_u8; data_size as usize];
    let pre_priced_tx = signer.create_transaction_with_fees(
        data.clone(),
        ctx.get_anchor().await?,
        DataLedger::Publish,
        BoundedFee::new(pre_cascade_term_fee),
        Some(BoundedFee::default()),
    )?;
    let pre_priced_tx = signer.sign_transaction(pre_priced_tx)?;

    match ctx.ingest_data_tx(pre_priced_tx.header.clone()).await {
        Err(AddTxError::TxIngress(TxIngressError::FundMisalignment(msg))) => {
            assert!(
                msg.contains("Insufficient term fee"),
                "Expected 'Insufficient term fee' error, got: {}",
                msg
            );
        }
        Ok(_) => panic!("Expected pre-Cascade-priced Publish tx to be rejected post-Cascade"),
        Err(e) => panic!(
            "Expected FundMisalignment for pre-Cascade-priced tx, got: {:?}",
            e
        ),
    }

    // Create a Publish tx with the *post-Cascade* term fee — should be accepted
    let price_info = ctx.get_data_price(DataLedger::Publish, data_size).await?;
    let post_priced_tx = signer.create_transaction_with_fees(
        data,
        ctx.get_anchor().await?,
        DataLedger::Publish,
        price_info.term_fee.into(),
        Some(price_info.perm_fee.into()),
    )?;
    let post_priced_tx = signer.sign_transaction(post_priced_tx)?;

    let result = ctx.ingest_data_tx(post_priced_tx.header.clone()).await;
    assert!(
        result.is_ok(),
        "Post-Cascade-priced Publish tx should be accepted, got: {:?}",
        result
    );

    ctx.stop().await;
    Ok(())
}

/// Test that API rejects data transactions targeting the Submit ledger.
/// Submit is an internal ledger — data transits through it on the way to
/// permanent storage but users cannot directly target it.
#[test_log::test(tokio::test)]
async fn test_api_rejects_submit_ledger_target() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    let data = vec![42_u8; 96];
    let anchor = genesis_node.get_anchor().await?;

    // Create a transaction explicitly targeting the Submit ledger
    let tx = signer
        .create_transaction_with_fees(
            data,
            anchor,
            DataLedger::Submit,
            BoundedFee::new(U256::from(1000).into()),
            None,
        )
        .expect("to create transaction");
    let tx = signer.sign_transaction(tx).expect("to sign transaction");

    // Try to ingest — should be rejected
    let result = genesis_node.ingest_data_tx(tx.header.clone()).await;

    match result {
        Err(AddTxError::TxIngress(TxIngressError::InvalidLedger(ledger_id))) => {
            assert_eq!(ledger_id, DataLedger::Submit as u32);
            tracing::info!("Transaction targeting Submit ledger correctly rejected");
        }
        Ok(_) => {
            panic!("Expected transaction targeting Submit ledger to be rejected");
        }
        Err(e) => {
            panic!(
                "Expected TxIngressError::InvalidLedger for Submit, got: {:?}",
                e
            );
        }
    }

    genesis_node.stop().await;
    Ok(())
}
