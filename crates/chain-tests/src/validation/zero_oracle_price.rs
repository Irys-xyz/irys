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
use irys_types::{NodeConfig, SealedBlock, storage_pricing::Amount};
use rust_decimal_macros::dec;

/// Produce a valid block, rewrite `oracle_irys_price` to zero, re-sign, and
/// assert BlockDiscovery rejects with `OraclePriceInvalid`.
#[test_log::test(tokio::test)]
async fn heavy_zero_oracle_price_block_rejected() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = 32;
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
    header.oracle_irys_price = Amount::token(dec!(0.0)).expect("zero token amount");
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

    assert!(
        matches!(
            result,
            Err(BlockDiscoveryError::BlockValidationError(
                PreValidationError::OraclePriceInvalid
            ))
        ),
        "block with oracle_irys_price = 0 must be rejected, got: {result:?}"
    );

    genesis_node.stop().await;
    Ok(())
}
