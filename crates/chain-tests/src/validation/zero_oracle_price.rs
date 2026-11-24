//! Zero `oracle_irys_price` must be rejected in prevalidation.
//!
//! A non-positive oracle price is never valid: fee math divides by the token
//! price, and accepting zero makes the relative safe-range band collapse so
//! the chain cannot recover via normal oracle updates.

use std::sync::Arc;

use crate::utils::{IrysNodeTest, solution_context};
use irys_actors::{
    BlockProdStrategy as _, ProductionStrategy,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl},
    block_validation::PreValidationError,
};
use irys_types::{
    NodeConfig, SealedBlock, UnixTimestamp, hardfork_config::Delta, storage_pricing::Amount,
};
use rust_decimal_macros::dec;

/// Exercise the real prevalidator with re-signed headers. Zero is invalid
/// on either side of Delta; a 50% jump is valid only before Delta.
#[rstest::rstest]
#[case::zero_before_delta(false, true)]
#[case::zero_after_delta(true, true)]
#[case::jump_before_delta(false, false)]
#[case::jump_after_delta(true, false)]
#[test_log::test(tokio::test)]
async fn heavy_oracle_price_bound_enforced(
    #[case] delta_active: bool,
    #[case] zero_price: bool,
) -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = 32;
        c.hardforks.delta = delta_active.then_some(Delta {
            activation_timestamp: UnixTimestamp::from_secs(0),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
            capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
            ingress_proof_reward_pool_percent: Delta::default_ingress_proof_reward_pool_percent(),
            token_price_safe_range: Delta::default_token_price_safe_range(),
            ema_price_adjustment_interval: Delta::default_ema_price_adjustment_interval(),
        });
    });

    let test_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", 20)
        .await;
    genesis_node.mine_block().await?;

    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .expect("block production should succeed");

    assert!(
        !block.header().oracle_irys_price.amount.is_zero(),
        "honest production must include a positive oracle price"
    );

    let mut header = (**block.header()).clone();
    let parent = genesis_node.get_block_by_height(header.height - 1).await?;
    header.oracle_irys_price = if zero_price {
        Amount::token(dec!(0.0)).expect("zero token amount")
    } else {
        parent
            .oracle_irys_price
            .add_multiplier(Amount::percentage(dec!(0.5))?)?
    };
    // Keep the EMA consistent in the allowed pre-Delta case, so acceptance
    // proves that the bound is selected at the candidate's fork timestamp.
    if !zero_price && !delta_active {
        let snapshot = genesis_node
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&parent.block_hash)
            .expect("parent EMA snapshot");
        header.ema_irys_price = snapshot
            .calculate_ema_for_new_block(
                &parent,
                header.oracle_irys_price,
                &genesis_config
                    .consensus_config()
                    .ema_rules_at(header.timestamp_secs()),
            )
            .ema;
    }
    genesis_config.signer().sign_block_header(&mut header)?;

    let mut tampered_body = block.to_block_body();
    tampered_body.block_hash = header.block_hash;
    let tampered_block = Arc::new(SealedBlock::new(header, tampered_body)?);

    let block_discovery = BlockDiscoveryFacadeImpl::new(
        genesis_node
            .node_ctx
            .service_senders
            .block_discovery
            .clone(),
    );
    let result = block_discovery.handle_block(tampered_block, false).await;

    if zero_price || delta_active {
        assert!(
            matches!(
                result,
                Err(BlockDiscoveryError::BlockValidationError(
                    PreValidationError::OraclePriceInvalid
                ))
            ),
            "out-of-bound oracle must be rejected, got: {result:?}"
        );
    } else {
        assert!(result.is_ok(), "50% jump is legal before Delta: {result:?}");
    }

    genesis_node.stop().await;
    Ok(())
}
