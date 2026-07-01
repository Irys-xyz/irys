//! Durable replay protection for commitment transactions in block_discovery
//! prevalidation.
//!
//! The per-block commitment snapshot is rebuilt from empty at every epoch
//! boundary (`create_commitment_snapshot_for_block` returns
//! `CommitmentSnapshot::default()` on epoch blocks). So the snapshot-based
//! "already included in a prior block" check (block_discovery + block_validation)
//! CANNOT catch a commitment replayed across an epoch boundary: in the fresh
//! post-epoch snapshot the replayed commitment reads back as `Unknown` and its
//! `add_commitment` is re-`Accepted`.
//!
//! An `UpdateRewardAddress` is the demonstrator: it carries no per-miner count
//! state (unlike Pledge, whose `pledge_count_before_executing` invariant already
//! rejects a stale replay) and does not flip the epoch-level staked flag (unlike
//! Stake, which reads back `Accepted` once epoch-activated). After the snapshot
//! resets, re-including a finalized `UpdateRewardAddress` passes both the status
//! check and `add_commitment` — only the durable, DB-backed dedup rejects it.
//!
//! block_discovery therefore rejects a non-epoch block that re-includes a
//! commitment already recorded on this branch's ancestry (reorg window, by-hash)
//! OR finalized below the reorg floor (MBH-verified
//! `canonical_commitment_included_height`) with `DuplicateTransaction`. This test
//! drives that path end-to-end.

use super::send_block_and_read_state;
use crate::utils::{IrysNodeTest, assert_validation_error, solution_context};
use irys_actors::{
    BlockProducerInner, ProductionStrategy, async_trait,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _},
    block_producer::{BlockProdStrategy, MempoolTxsBundle},
    block_validation::ValidationError,
};
use irys_types::{CommitmentTransaction, IrysAddress, NodeConfig, irys::IrysSigner};
use std::sync::Arc;

/// Evil producer that force-includes an already-finalized commitment in the
/// commitment ledger, bypassing the normal mempool selection (which would never
/// re-select it).
struct ReplayStrategy {
    prod: ProductionStrategy,
    dup: CommitmentTransaction,
}

#[async_trait::async_trait]
impl BlockProdStrategy for ReplayStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.prod.inner
    }

    async fn get_mempool_txs(
        &self,
        prev_block_header: &irys_types::IrysBlockHeader,
        block_timestamp: irys_types::UnixTimestampMs,
    ) -> Result<MempoolTxsBundle, irys_actors::tx_selector::TxSelectorError> {
        // Start from a correctly-populated bundle (right epoch snapshot / fees),
        // then substitute the replayed commitment as the sole commitment tx.
        let mut bundle = self
            .prod
            .get_mempool_txs(prev_block_header, block_timestamp)
            .await?;
        bundle.commitment_txs = vec![self.dup.clone()];
        bundle.commitment_txs_to_bill = vec![self.dup.clone()];
        Ok(bundle)
    }
}

/// A finalized commitment re-included in a fresh non-epoch block AFTER an epoch
/// boundary must be rejected by block_discovery prevalidation as a replay, even
/// though the post-epoch commitment snapshot has no local record of it and would
/// otherwise re-accept it.
#[test_log::test(tokio::test)]
async fn heavy_commitment_replay_across_epoch_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().chunk_size = 32;
    // Small reorg window so the commitment's inclusion finalizes below the floor
    // fast; migration_depth < block_tree_depth keeps Config::validate happy.
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().block_tree_depth = 3;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Stake the signer and cross an epoch boundary so the signer is
    // epoch-activated (`has_stake` true in the post-epoch snapshot). This lets a
    // later UpdateRewardAddress replay read back `Unknown` (needs stake) instead
    // of `Unstaked`.
    let _stake = node.post_stake_commitment_with_signer(&signer).await?;
    node.mine_blocks(num_blocks_in_epoch).await?;
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64, seconds_to_wait)
        .await?;

    // Post an UpdateRewardAddress (Borealis is active from genesis in the testing
    // config) and include it, then mine well past the next epoch boundary so it
    // finalizes below `tip - block_tree_depth` (its inclusion block migrates and
    // its snapshot record is dropped by the epoch reset).
    let update = node
        .post_update_reward_address(&signer, IrysAddress::random())
        .await?;
    node.wait_for_mempool(update.id(), seconds_to_wait).await?;
    node.mine_blocks(num_blocks_in_epoch * 2).await?;
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64 * 3, seconds_to_wait)
        .await?;

    // Craft a non-epoch block that re-includes the already-finalized commitment.
    // Reuse the EXACT tx object (same tx_id / original anchor) so the durable
    // dedup can match it; the original anchor is still within
    // tx_anchor_expiry_depth.
    let strategy = ReplayStrategy {
        dup: update.clone(),
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };

    node.gossip_disable();
    let (block, _stats, _payload) = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .expect("block produced");
    node.gossip_enable();

    // Route through block_discovery (the code under test). Not the direct
    // `BlockPreValidated` path, which bypasses block_discovery entirely.
    let result = strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;

    assert!(
        matches!(&result, Err(BlockDiscoveryError::DuplicateTransaction(id)) if *id == update.id()),
        "re-included finalized commitment must be rejected as a replay, got {result:?}"
    );

    node.stop().await;
    Ok(())
}

/// Same replay as above, but routed through the SEPARATE full-validation path
/// (`ValidationService` → `commitment_txs_are_valid`) instead of block_discovery
/// prevalidation. `send_block_and_read_state` sends `BlockPreValidated`, which
/// bypasses block_discovery entirely, so this exercises the durable dedup in
/// full validation specifically — the reject the prevalidation path never
/// reaches here. Without it the post-epoch snapshot re-accepts the replayed
/// `UpdateRewardAddress` and the block is stored.
#[test_log::test(tokio::test)]
async fn heavy_commitment_replay_rejected_in_full_validation() -> eyre::Result<()> {
    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().block_tree_depth = 3;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Stake the signer and cross an epoch boundary so the signer is
    // epoch-activated; a later UpdateRewardAddress replay then reads back
    // `Unknown` (needs stake) instead of `Unstaked` in the post-epoch snapshot.
    let _stake = node.post_stake_commitment_with_signer(&signer).await?;
    node.mine_blocks(num_blocks_in_epoch).await?;
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64, seconds_to_wait)
        .await?;

    // Post + include an UpdateRewardAddress, then mine past the next epoch so it
    // finalizes below `tip - block_tree_depth` (its inclusion block migrates and
    // its snapshot record is dropped by the epoch reset).
    let update = node
        .post_update_reward_address(&signer, IrysAddress::random())
        .await?;
    node.wait_for_mempool(update.id(), seconds_to_wait).await?;
    node.mine_blocks(num_blocks_in_epoch * 2).await?;
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64 * 3, seconds_to_wait)
        .await?;

    // Craft a non-epoch block that re-includes the already-finalized commitment
    // (exact tx object: same tx_id / original anchor, still within
    // tx_anchor_expiry_depth).
    let strategy = ReplayStrategy {
        dup: update.clone(),
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };

    node.gossip_disable();
    let (block, _stats, _payload) = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .expect("block produced");
    node.gossip_enable();

    // Route through full validation via `BlockPreValidated`, bypassing
    // block_discovery prevalidation. This is the path that reaches
    // `commitment_txs_are_valid`.
    let outcome = send_block_and_read_state(&node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        outcome,
        |e| matches!(e, ValidationError::DuplicateCommitmentTransaction { tx_id } if *tx_id == update.id()),
        "full validation must reject the re-included finalized commitment as a duplicate",
    );

    node.stop().await;
    Ok(())
}
