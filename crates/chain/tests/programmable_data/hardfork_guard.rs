//! Tests that PD transactions are rejected before Sprite hardfork and accepted after.

use crate::utils::IrysNodeTest;
use irys_types::hardfork_config::{FrontierParams, IrysHardforkConfig, Sprite};
use irys_types::storage_pricing::Amount;
use irys_types::UnixTimestamp;
use rust_decimal_macros::dec;

/// Test that PD transactions are rejected before Sprite hardfork activates,
/// and accepted after it activates.
///
/// Flow:
/// 1. Configure Sprite hardfork to activate 5 seconds in the future
/// 2. Start node at genesis
/// 3. Verify Sprite is NOT active at genesis
/// 4. Try to submit PD transaction - expect rejection
/// 5. Mine blocks until Sprite activates
/// 6. Verify Sprite IS active
/// 7. Submit PD transaction - expect acceptance
/// 8. Mine block and verify PD transaction is included
#[test_log::test(tokio::test)]
async fn heavy_pd_transactions_rejected_before_sprite_hardfork() -> eyre::Result<()> {
    // 1. Configure Sprite to activate 5 seconds in the future
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let sprite_activation = now_secs + 5;

    let mut config = IrysNodeTest::<()>::default_async().cfg;
    let pd_tx_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer]);

    config.consensus.get_mut().hardforks = IrysHardforkConfig {
        frontier: FrontierParams {
            number_of_ingress_proofs_total: 2,
            number_of_ingress_proofs_from_assignees: 0,
        },
        next_name_tbd: None,
        sprite: Some(Sprite {
            activation_timestamp: UnixTimestamp::from_secs(sprite_activation),
            cost_per_mb: Amount::token(dec!(0.01)).expect("valid token amount"),
            base_fee_floor: Amount::token(dec!(0.01)).expect("valid token amount"),
            max_pd_chunks_per_block: 7_500,
            // Set min_pd_transaction_cost low enough that the test's PD tx will pass.
            // The test uses ~1e11 wei per chunk, so at $1/IRYS we need min < 1e11 wei.
            min_pd_transaction_cost: Amount::token(dec!(0.0)).expect("valid token amount"),
        }),
        aurora: None,
        borealis: None,
    };

    // 2. Start node
    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // 3. Verify Sprite NOT active at genesis
    let genesis_block = ctx.get_block_by_height(0).await?;
    assert!(
        !ctx.node_ctx
            .config
            .consensus
            .hardforks
            .is_sprite_active(genesis_block.timestamp_secs()),
        "Sprite should NOT be active at genesis"
    );

    // 4. Try to submit PD transaction - expect rejection
    let rejection_result = ctx
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            1,         // chunks_per_tx
            1_000_000, // priority_fee_per_chunk
            0,         // nonce
            0,         // offset_base
        )
        .await;

    assert!(
        rejection_result.is_err(),
        "PD transaction should be rejected before Sprite hardfork, got: {:?}",
        rejection_result
    );
    tracing::info!(
        "PD transaction correctly rejected before Sprite: {:?}",
        rejection_result.err()
    );

    // 5. Mine blocks until Sprite activates
    let post_sprite_block = loop {
        let block = ctx.mine_block().await?;
        if block.timestamp_secs().as_secs() >= sprite_activation {
            break block;
        }
    };

    // 6. Verify Sprite IS active
    assert!(
        ctx.node_ctx
            .config
            .consensus
            .hardforks
            .is_sprite_active(post_sprite_block.timestamp_secs()),
        "Sprite should be active after mining past activation timestamp"
    );
    tracing::info!(
        "Sprite hardfork activated at block height {}",
        post_sprite_block.height
    );

    // 7. Submit PD transaction - expect acceptance
    let tx_hash = ctx
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            1,         // chunks_per_tx
            1_000_000, // priority_fee_per_chunk
            0,         // nonce (same as before since rejection didn't increment nonce)
            0,         // offset_base
        )
        .await?;

    tracing::info!("PD transaction accepted after Sprite: {:?}", tx_hash);

    // 8. Mine block and verify PD transaction is included
    let (_, eth_payload, _) = ctx.mine_block_without_gossip().await?;
    let block_txs: Vec<_> = eth_payload.block().body().transactions.iter().collect();

    assert!(
        block_txs.iter().any(|tx| tx.hash() == &tx_hash),
        "PD transaction should be included in block after Sprite activation"
    );
    tracing::info!("PD transaction included in block successfully");

    ctx.stop().await;
    Ok(())
}
