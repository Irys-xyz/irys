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
use crate::utils::{
    InjectCommitmentsStrategy, IrysNodeTest, assert_validation_error, solution_context,
};
use irys_actors::{
    ProductionStrategy,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _},
    block_producer::BlockProdStrategy as _,
    block_validation::ValidationError,
};
use irys_chain::IrysNodeCtx;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::{CommitmentTransaction, IrysAddress, NodeConfig, irys::IrysSigner};
use std::sync::Arc;

// `InjectCommitmentsStrategy` (crate::utils) is the evil producer used below:
// it force-includes a fixed set of commitment txs in the commitment ledger,
// bypassing normal mempool selection. A single dup tx replays an
// already-finalized commitment; `[dup, dup]` lists the same commitment twice
// within one block.

/// Sets up a node with an epoch-activated signer and a finalized
/// `UpdateRewardAddress` commitment: stake + cross an epoch boundary (so the
/// signer is epoch-activated and a later replay reads back `Unknown` instead
/// of `Unstaked`), then post + include an `UpdateRewardAddress` and mine well
/// past the next epoch boundary so it finalizes below `tip - block_tree_depth`
/// (its inclusion block migrates and its snapshot record is dropped by the
/// epoch reset). Shared scaffolding for the two replay-rejection tests below,
/// which differ only in which validation path they route the crafted replay
/// block through.
async fn setup_finalized_update_reward_address(
    seconds_to_wait: usize,
) -> eyre::Result<(IrysNodeTest<IrysNodeCtx>, CommitmentTransaction)> {
    let num_blocks_in_epoch = 4;
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

    let _stake = node.post_stake_commitment_with_signer(&signer).await?;
    node.mine_blocks(num_blocks_in_epoch).await?;
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64, seconds_to_wait)
        .await?;

    let update = node
        .post_update_reward_address(&signer, IrysAddress::random())
        .await?;
    node.wait_for_mempool(update.id(), seconds_to_wait).await?;
    node.mine_blocks(num_blocks_in_epoch * 2).await?;
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64 * 3, seconds_to_wait)
        .await?;

    Ok((node, update))
}

/// A finalized commitment re-included in a fresh non-epoch block AFTER an epoch
/// boundary must be rejected by block_discovery prevalidation as a replay, even
/// though the post-epoch commitment snapshot has no local record of it and would
/// otherwise re-accept it.
#[test_log::test(tokio::test)]
async fn heavy_commitment_replay_across_epoch_rejected() -> eyre::Result<()> {
    let seconds_to_wait = 20;
    let (node, update) = setup_finalized_update_reward_address(seconds_to_wait).await?;

    // Craft a non-epoch block that re-includes the already-finalized commitment.
    // Reuse the EXACT tx object (same tx_id / original anchor) so the durable
    // dedup can match it; the original anchor is still within
    // tx_anchor_expiry_depth.
    let strategy = InjectCommitmentsStrategy {
        txs: vec![update.clone()],
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
    let seconds_to_wait = 20;
    let (node, update) = setup_finalized_update_reward_address(seconds_to_wait).await?;

    // Craft a non-epoch block that re-includes the already-finalized commitment
    // (exact tx object: same tx_id / original anchor, still within
    // tx_anchor_expiry_depth).
    let strategy = InjectCommitmentsStrategy {
        txs: vec![update.clone()],
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

/// `CommitmentSnapshot::add_commitment` is deliberately idempotent for
/// Stake/Pledge (a commitment may legitimately be first-included exactly
/// once, and the snapshot cannot tell "already pending from this same block"
/// apart from "already pending from a prior one"). So a producer that lists
/// the SAME commitment twice in one block's commitment ledger would otherwise
/// pass the snapshot check twice. `find_replayed_commitment`'s within-block
/// duplicate check (crates/actors/src/commitment_dedup.rs) is the layer
/// designed to catch this; unit tests there exercise it directly.
///
/// This test hits a DIFFERENT, earlier backstop first: `BlockTransactions`'s
/// header/body reconciliation (crates/types/src/block.rs) builds its
/// tx_id -> tx map from the body by id, so a duplicated tx collapses to one
/// map entry, and matching the header's system ledger (which still lists the
/// id twice) against it fails with "missing tx" on the second occurrence.
/// Block production therefore never produces a block at all — an
/// irrecoverable error aborts it before `find_replayed_commitment` is ever
/// reached. This is documented here rather than dropped, since it shows the
/// attack has no route to a real block even before the dedup layer runs.
#[test_log::test(tokio::test)]
async fn heavy_same_block_duplicate_commitment_rejected() -> eyre::Result<()> {
    let seconds_to_wait = 20;
    let mut config = NodeConfig::testing();

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // A fresh stake commitment, posted to the mempool but never mined into any
    // block: the duplicate must be caught purely within the block under
    // validation, not via the durable finalized-inclusion lookup.
    let dup = node.post_stake_commitment_with_signer(&signer).await?;
    node.wait_for_mempool_commitment_txs(vec![dup.id()], seconds_to_wait)
        .await?;

    let strategy = InjectCommitmentsStrategy {
        txs: vec![dup.clone(), dup.clone()],
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };

    node.gossip_disable();
    let production_result = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await;
    node.gossip_enable();

    match production_result {
        Err(irys_actors::block_producer::BlockProductionError::Irrecoverable { source }) => {
            assert!(
                source.to_string().contains("system ledger missing tx"),
                "expected a header/body reconciliation failure on the duplicated commitment, got: {source}"
            );
        }
        Err(other) => panic!(
            "expected an Irrecoverable header/body reconciliation error, got a different \
             BlockProductionError: {other}"
        ),
        Ok(_) => panic!(
            "expected block production to fail on the same-block duplicate before a block is produced"
        ),
    }

    node.stop().await;
    Ok(())
}

/// Genesis first-includes its commitments but never flows through the
/// migration metadata writer, so no dedup row is written for it at
/// init. The startup backfill must fill that gap: after a node boots, the
/// finalized-inclusion lookup the dedup consults
/// (`canonical_commitment_included_height`) must resolve every genesis
/// commitment to height 0 (also exercising its content check against the real
/// genesis header). Without the row, once genesis passes the reorg floor a
/// replayed genesis commitment slips past the dedup entirely — the
/// finalized-lookup half is the only path that can catch it there.
///
/// This asserts the seam directly rather than replaying a genesis commitment in
/// a crafted block: the block_discovery prevalidation path rejects a genesis
/// commitment on its ancient chained anchor (`H256::default()`-rooted) before
/// the dedup runs, and the full-validation path would require re-executing a
/// genesis Stake shadow tx — both orthogonal to the metadata gap this fix
/// closes.
#[test_log::test(tokio::test)]
async fn genesis_commitments_resolve_to_included_height_zero_after_boot() -> eyre::Result<()> {
    let config = NodeConfig::testing();
    let node = IrysNodeTest::new_genesis(config).start().await;

    let genesis = node.get_block_by_height_from_index(0, false)?;
    let genesis_commitment_ids = genesis.commitment_tx_ids().to_vec();
    assert!(
        !genesis_commitment_ids.is_empty(),
        "genesis must carry commitment txs for this regression to be meaningful"
    );

    for id in &genesis_commitment_ids {
        let resolved = node.node_ctx.db.view_eyre(|tx| {
            irys_database::canonical_commitment_included_height(tx, id, u64::MAX)
        })?;
        assert_eq!(
            resolved,
            Some(0),
            "genesis commitment {id} must resolve to included_height 0 after boot"
        );
    }

    node.stop().await;
    Ok(())
}

/// Selection-side counterpart to the two validation tests above: the block
/// PRODUCER (`select_best_txs`) must not re-select a commitment already finalized
/// below the reorg floor.
///
/// After [`setup_finalized_update_reward_address`], the `UpdateRewardAddress`'s
/// inclusion block sits below `tip - block_tree_depth` — pruned from the canonical
/// cache, migrated to the DB — while the tx object is still inside its
/// anchor-expiry window. Confirmation does NOT evict a commitment from the
/// mempool; it only stamps `included_height`, so the finalized tx is STILL a
/// `sorted_commitments()` candidate. The in-memory `confirmed_commitments` dedup
/// (built from the ~block_tree_depth canonical cache) can no longer see it — only
/// the finalized-index dedup (`canonical_commitment_included_height`) can. Mining
/// a normal block through the default `ProductionStrategy` must therefore exclude
/// it; without the selection-side finalized check the producer would re-include
/// it and build a block its own validation rejects as a replay.
#[test_log::test(tokio::test)]
async fn heavy_finalized_commitment_not_reselected_by_producer() -> eyre::Result<()> {
    let seconds_to_wait = 20;
    let (node, update) = setup_finalized_update_reward_address(seconds_to_wait).await?;

    // Guard against a vacuous pass: the finalized commitment must still be a live
    // selection candidate (present in the exact set `select_best_txs` iterates),
    // otherwise "absent from the produced block" would prove nothing.
    let candidates = node
        .node_ctx
        .mempool_guard
        .atomic_state()
        .sorted_commitments()
        .await;
    assert!(
        candidates.iter().any(|c| c.id() == update.id()),
        "finalized UpdateRewardAddress must still be a mempool selection candidate \
         (confirmation stamps included_height but does not evict); got {} candidates",
        candidates.len()
    );

    // Produce a normal (non-epoch) block through mempool selection. With
    // num_blocks_in_epoch = 4 the setup leaves the tip at height 12, so this
    // block is height 13 — deliberately not an epoch boundary, where commitment
    // selection is bypassed.
    let block = node.mine_block().await?;
    assert_ne!(
        block.height % 4,
        0,
        "assertion block must be a non-epoch block so commitment selection runs"
    );
    assert!(
        !block.commitment_tx_ids().contains(&update.id()),
        "producer must not re-select a commitment finalized below the reorg floor; \
         block {} at height {} re-included {}",
        block.block_hash,
        block.height,
        update.id()
    );

    node.stop().await;
    Ok(())
}
