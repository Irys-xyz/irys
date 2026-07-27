//! Three-way reward-address bind: registered stake payout, Irys header
//! `reward_address`, and EVM coinbase must all match.
//!
//! Regression for H2: a block that declares a spoofed header `reward_address`
//! while keeping the original EVM coinbase (and registered stake payout) must
//! be rejected.

use std::sync::Arc;

use super::send_block_and_read_state;
use crate::utils::{IrysNodeTest, assert_validation_error, solution_context};
use irys_actors::block_validation::ValidationError;
use irys_actors::{BlockProdStrategy as _, ProductionStrategy};
use irys_types::{IrysAddress, NodeConfig, SealedBlock as IrysSealedBlock};

/// Spoof header `reward_address` after production (coinbase unchanged).
/// Full validation must reject on coinbase ≠ header.reward_address.
///
/// Note: this harness injects via `BlockPreValidated`, so prevalidation
/// (registered ↔ header) is not re-run here; that check is covered on the
/// gossip/discovery path. This test locks the paid ↔ declared bind.
#[test_log::test(tokio::test)]
async fn heavy_test_reward_address_not_bound_to_evm_coinbase() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let signer = genesis_config.signer().clone();
    let configured_reward = genesis_config.reward_address;
    let spoofed_reward = IrysAddress::random();
    eyre::ensure!(
        configured_reward != spoofed_reward,
        "test requires distinct spoofed reward address"
    );

    let genesis_node = IrysNodeTest::new_genesis(genesis_config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.mine_block().await?;

    let block_prod_strategy = ProductionStrategy {
        inner: genesis_node.node_ctx.block_producer_inner.clone(),
    };
    let (sealed, _stats, eth_payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("no block produced"))?;

    let coinbase = eth_payload.block().header().beneficiary;
    assert_eq!(
        IrysAddress::from(coinbase),
        sealed.header().reward_address,
        "honest production binds coinbase to header reward_address"
    );
    assert_eq!(
        sealed.header().reward_address,
        configured_reward,
        "produced header should use registered/config reward_address"
    );

    let mut header = sealed.header().as_ref().clone();
    header.reward_address = spoofed_reward;
    signer.sign_block_header(&mut header)?;

    assert_ne!(
        IrysAddress::from(coinbase),
        header.reward_address,
        "test setup: header reward_address must diverge from EVM coinbase"
    );

    let mut body = sealed.to_block_body();
    body.block_hash = header.block_hash;
    let mismatched = Arc::new(IrysSealedBlock::new(header, body)?);

    genesis_node
        .node_ctx
        .block_pool
        .add_execution_payload_to_cache(eth_payload.block().clone())
        .await;

    let outcome =
        send_block_and_read_state(&genesis_node.node_ctx, Arc::clone(&mismatched), true).await?;

    assert_validation_error(
        outcome,
        |e| match e {
            ValidationError::ShadowTransactionInvalid(msg) => {
                msg.contains("EVM coinbase") && msg.contains("reward_address")
            }
            _ => false,
        },
        "spoofed header reward_address must be rejected (coinbase != declared)",
    );

    genesis_node.stop().await;
    Ok(())
}
