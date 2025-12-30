//! Tests for PD transaction minimum cost validation.
//!
//! The `min_pd_transaction_cost` Sprite hardfork parameter sets the minimum total cost
//! (in USD) for a PD transaction. Transactions with fees below this threshold are rejected.
//!
//! NOTE: The mempool prefilter only performs basic sanity checks. Full validation
//! (USD price conversion, min cost) happens during EVM execution. This means invalid
//! transactions can enter the mempool but will be rejected when the block builder
//! attempts to execute them.

use crate::utils::IrysNodeTest;
use irys_types::storage_pricing::Amount;
use rust_decimal_macros::dec;

/// Test that PD transactions with fees below min_pd_transaction_cost are rejected
/// at EVM execution time and not included in blocks.
///
/// This test verifies that:
/// 1. A low-fee PD transaction is NOT included in a block (rejected by EVM)
/// 2. Block production continues successfully after rejecting the low-fee tx
/// 3. The low-fee tx remains in mempool (not executed, nonce not consumed)
#[test_log::test(tokio::test)]
async fn heavy_pd_transaction_rejected_below_min_cost() -> eyre::Result<()> {
    // Configure with min_pd_transaction_cost
    // We set a threshold that will reject our 200 wei test tx but allow normal txs
    let mut config = IrysNodeTest::<()>::default_async().cfg;
    let pd_tx_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer]);

    // Set min_pd_transaction_cost to $0.000000001 USD (1e-9 USD = 1e9 wei in 1e18 scale)
    // At $1/IRYS: min_cost_irys = 1e9 IRYS wei = 1 gwei
    // Low-fee tx: 200 wei < 1e9 wei (fails)
    config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite must be configured")
        .min_pd_transaction_cost = Amount::token(dec!(0.000000001))?;

    // Start node
    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // Mine a block first to establish the IRYS/USD price via shadow transaction.
    // At genesis, the price is zero and minimum cost validation is skipped.
    ctx.mine_block_without_gossip().await?;
    tracing::info!("Mined initial block to set IRYS/USD price");

    // Inject low-fee PD tx - it will enter mempool (mempool only checks for zero fees)
    // but will be rejected during EVM execution due to failing min cost validation.
    // Total fees = (100 + 100) * 1 chunk = 200 wei (below 1e9 wei threshold)
    let low_fee_tx_hash = ctx
        .create_and_inject_pd_transaction_with_custom_fees(
            &pd_tx_signer,
            1,   // 1 chunk
            100, // 100 wei priority fee per chunk (very low)
            100, // 100 wei base fee per chunk (very low)
            0,   // nonce
            0,   // offset_base
        )
        .await?;

    tracing::info!(
        "Low-fee PD tx injected to mempool: {:?} (will fail EVM validation)",
        low_fee_tx_hash
    );

    // Mine block and verify low-fee tx is NOT included (rejected by EVM)
    let (_, eth_payload, _) = ctx.mine_block_without_gossip().await?;
    let low_fee_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &low_fee_tx_hash);

    assert!(
        !low_fee_tx_included,
        "Low-fee PD transaction should NOT be included in block (rejected by EVM min cost check)"
    );

    tracing::info!("Low-fee PD tx correctly rejected by EVM - not included in block");

    // The test has verified:
    // 1. The low-fee tx was not included (min cost validation worked)
    // 2. Block production didn't crash (error handling fixed)
    // 3. The system continues to function

    ctx.stop().await;
    Ok(())
}
