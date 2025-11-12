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
        Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
            tracing::info!("Transaction correctly rejected with Unfunded error");
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
        Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
            tracing::info!("Transaction correctly rejected with Unfunded error");
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
