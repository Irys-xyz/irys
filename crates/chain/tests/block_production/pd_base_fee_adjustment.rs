use crate::utils::IrysNodeTest;
use alloy_consensus::Transaction as _;
use irys_actors::{pd_pricing::base_fee::PD_BASE_FEE_INDEX, reth_ethereum_primitives};
use irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{storage_pricing::Amount, NodeConfig, U256};
use reth::primitives::SealedBlock;
use rust_decimal_macros::dec;

/// Test that PD base fee increases when block utilization exceeds the 50% target.
///
/// This test verifies the dynamic PD base fee adjustment mechanism:
/// 1. Creates 4 PD transactions, each using 20 chunks (4 * 20 = 80 total chunks = 80% utilization)
/// 2. Mines a block containing all 4 transactions
/// 3. Verifies the PD base fee in the subsequent block increases according to the formula:
///    - Target utilization: 50%
///    - Max adjustment: +-12.5%
///    - At 80% utilization: adjustment = ((80%-50%)/(100%-50%)) * 12.5% = 7.5% increase
///
/// This also tests that chunk counting works correctly across multiple PD transactions in a block.
#[test_log::test(actix_web::test)]
async fn test_pd_base_fee_increases_with_high_utilization() -> eyre::Result<()> {
    // Configure node with predictable PD parameters
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 256 * 1024; // 256 KB

    // Set max PD chunks to 100 for easy percentage calculations
    let max_pd_chunks = 100_u64;
    let sprite = config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing");
    sprite.max_pd_chunks_per_block = max_pd_chunks;
    // Set min_pd_transaction_cost to 0 to avoid rejections in this test
    sprite.min_pd_transaction_cost = Amount::token(dec!(0.0)).expect("valid token amount");

    // Create and fund a test account for PD transactions
    let pd_tx_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer]);

    // Start node
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    // Establish baseline: mine first block and extract initial PD base fee
    let _ = node.mine_block().await?;

    // Create high utilization: inject 4 PD transactions with 20 chunks each
    // Total: 4 * 20 = 80 chunks = 80% of max_pd_chunks_per_block (100)
    let num_transactions = 4;
    let chunks_per_tx = 20;
    let mut tx_hashes = Vec::new();
    for i in 0..num_transactions {
        let tx_hash = node
            .create_and_inject_pd_transaction_with_optimal_fees(
                &pd_tx_signer,
                chunks_per_tx,
                i as u64,                 // nonce
                i * chunks_per_tx as u32, // offset_base
            )
            .await?;
        tx_hashes.push(tx_hash);
    }

    // Mine block 2 containing the PD transactions
    let (_block2, eth_payload2, _) = node.mine_block_without_gossip().await?;

    // Verify all PD transactions were included
    // 3 shadow txs: BlockReward, PdBaseFeeUpdate, IrysUsdPriceUpdate
    verify_pd_transactions_included(eth_payload2.block(), &tx_hashes, 3)?;

    // Extract PD base fee from block 2
    let block2_pd_base_fee: U256 = extract_pd_base_fee_from_block(eth_payload2.block())?.into();

    // Mine block 3 to observe fee adjustment based on block 2's 80% utilization
    let (_block3, eth_payload3, _) = node.mine_block_without_gossip().await?;
    let block3_pd_base_fee: U256 = extract_pd_base_fee_from_block(eth_payload3.block())?.into();

    // Calculate expected fee increase using exact U256 arithmetic
    // Formula: For utilization > 50%:
    //   delta = (utilization - target) / (100% - target)
    //   adjustment = delta * max_adjustment
    //   new_fee = old_fee * (1 + adjustment)
    //
    // With 80% utilization:
    //   delta = (0.80 - 0.50) / (1.00 - 0.50) = 0.30 / 0.50 = 0.6
    //   adjustment = 0.6 * 0.125 = 0.075 (7.5%)
    //   new_fee = old_fee * 1.075 = (old_fee * 1075) / 1000

    // Exact integer arithmetic: multiply by 1075, divide by 1000
    let expected_block3_fee = (block2_pd_base_fee * U256::from(1075)) / U256::from(1000);

    // Exact comparison
    assert_eq!(
        block3_pd_base_fee, expected_block3_fee,
        "PD base fee should increase by exactly 7.5% (1.075*) at 80% utilization.\n\
         Block 2 fee: {}\n\
         Expected Block 3 fee: {} (block2_fee * 1075 / 1000)\n\
         Actual Block 3 fee: {}",
        block2_pd_base_fee, expected_block3_fee, block3_pd_base_fee
    );

    // Also verify the fee actually increased (sanity check)
    assert!(
        block3_pd_base_fee > block2_pd_base_fee,
        "Block 3 PD base fee ({}) should be greater than Block 2 ({}) after high utilization",
        block3_pd_base_fee,
        block2_pd_base_fee
    );

    // Cleanup
    node.stop().await;

    Ok(())
}

/// Verify that all expected PD transactions are included in a block.
///
/// Checks that:
/// 1. Block contains the expected total number of transactions (shadow txs + PD txs)
/// 2. All expected PD transaction hashes are present in the block
fn verify_pd_transactions_included(
    sealed_block: &SealedBlock<reth_ethereum_primitives::Block>,
    expected_pd_tx_hashes: &[alloy_primitives::FixedBytes<32>],
    num_shadow_txs: usize,
) -> eyre::Result<()> {
    let block_txs: Vec<_> = sealed_block.body().transactions.iter().collect();
    let expected_total = num_shadow_txs + expected_pd_tx_hashes.len();

    // Verify total transaction count
    assert_eq!(
        block_txs.len(),
        expected_total,
        "Block should contain {} shadow txs + {} PD txs = {} total, got {}",
        num_shadow_txs,
        expected_pd_tx_hashes.len(),
        expected_total,
        block_txs.len()
    );

    // Verify each expected PD transaction is in the block
    for (idx, expected_hash) in expected_pd_tx_hashes.iter().enumerate() {
        let found = block_txs.iter().any(|tx| tx.hash() == expected_hash);
        assert!(
            found,
            "PD transaction {} with hash {:?} should be included in block",
            idx, expected_hash
        );
    }

    Ok(())
}

/// Helper function to extract PD base fee from an EVM block's 2nd shadow transaction.
///
/// The PD base fee is stored in the PdBaseFeeUpdate transaction, which is always
/// the 2nd transaction in a block (after BlockReward at position 0).
///
/// Returns the fee as an alloy U256 (needs to be converted to irys U256 if needed).
fn extract_pd_base_fee_from_block(
    sealed_block: &SealedBlock<reth_ethereum_primitives::Block>,
) -> eyre::Result<alloy_primitives::U256> {
    use eyre::OptionExt as _;

    // Get the 2nd transaction (index 1) which should be PdBaseFeeUpdate
    let second_tx = sealed_block
        .body()
        .transactions
        .get(PD_BASE_FEE_INDEX)
        .ok_or_eyre("Block must have at least 2 transactions (BlockReward + PdBaseFeeUpdate)")?;

    // Decode as shadow transaction
    let shadow_tx = ShadowTransaction::decode(&mut second_tx.input().as_ref())
        .map_err(|e| eyre::eyre!("Failed to decode 2nd transaction as shadow tx: {}", e))?;

    // Extract the per_chunk value from PdBaseFeeUpdate
    let packet = shadow_tx
        .as_v1()
        .ok_or_eyre("Expected V1 shadow transaction")?;
    match packet {
        TransactionPacket::PdBaseFeeUpdate(update) => Ok(update.per_chunk),
        other => eyre::bail!(
            "2nd transaction in block is not PdBaseFeeUpdate (found: {:?})",
            other
        ),
    }
}
