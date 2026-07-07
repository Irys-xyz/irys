use crate::utils::{
    AddTxError, BlockValidationOutcome, InjectCommitmentsStrategy, IrysNodeTest, solution_context,
};
use irys_actors::mempool_service::TxIngressError;
use irys_actors::{
    ProductionStrategy,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _},
    block_producer::BlockProdStrategy as _,
};
use irys_testing_utils::initialize_tracing;
use irys_types::{CommitmentTransaction, DataLedger, NodeConfig, irys::IrysSigner};
use std::sync::Arc;

/// Commitments must be validated against `commitment_anchor_expiry_depth` (the
/// longer commitment window), while data txs stay gated by the shorter
/// `tx_anchor_expiry_depth`. `NodeConfig::testing()` sets tx depth = 20 and
/// commitment depth = 100, so an anchor ~26 blocks deep is too old for a data
/// tx but still valid for a commitment.
#[test_log::test(tokio::test)]
async fn heavy_commitment_accepts_old_anchor_data_tx_rejects() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor, then mine past the tx window (20) but stay
    // within the commitment window (100).
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(25).await?;

    // Commitment (stake) anchored at the old block: accepted.
    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx)
        .await
        .expect("commitment with old-but-in-window anchor should be accepted");

    // Data tx anchored at the same old block: rejected (too old for the
    // shorter tx window).
    let price_info = node.get_data_price(DataLedger::Publish, 32).await?;
    let data_tx = signer.create_publish_transaction(
        vec![7_u8; 32],
        old_anchor,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = signer.sign_transaction(data_tx)?;

    let result = node.ingest_data_tx(data_tx.header).await;
    match result {
        Err(AddTxError::TxIngress(err)) => {
            assert!(
                matches!(err, TxIngressError::InvalidAnchor(_)),
                "expected InvalidAnchor, got {err:?}"
            );
        }
        other => panic!("expected data tx to be rejected with InvalidAnchor, got {other:?}"),
    }

    node.stop().await;
    Ok(())
}

/// Mempool pruning (not just ingress) must also use the commitment window:
/// once a commitment is accepted with an old-but-in-window anchor, later
/// block confirmations must not prune it until its anchor age exceeds
/// `commitment_anchor_expiry_depth`, even though the same anchor age would
/// already exceed the (shorter) `tx_anchor_expiry_depth` used for data txs.
/// `NodeConfig::testing()`: tx depth = 20, commitment depth = 100,
/// block_migration_depth = 6 -> effective data expiry = 20+6+5 = 31,
/// effective commitment expiry = 100+6+5 = 111. An anchor age of 40 is past
/// the former but well within the latter.
#[test_log::test(tokio::test)]
async fn heavy_commitment_survives_pruning_past_data_tx_window() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    let old_anchor = node.mine_block().await?.block_hash;

    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx.clone())
        .await
        .expect("commitment with old-but-in-window anchor should be accepted");

    // Mine past the data-tx effective expiry (31) but stay within the
    // commitment effective expiry (111). Each mined block confirms the
    // previous one, which drives a `prune_pending_txs` cycle.
    node.mine_blocks(40).await?;

    node.wait_for_mempool_commitment_txs(vec![stake_tx.id()], 10)
        .await
        .expect("commitment must survive pruning within its longer window");

    node.stop().await;
    Ok(())
}

/// Block production must select commitments over the (longer) commitment
/// anchor window, not the shorter data-tx window. `NodeConfig::testing()`:
/// tx depth = 20, commitment depth = 100, block_migration_depth = 6. After
/// mining 26 blocks past the anchor, the anchor (height 1) is older than the
/// tx-derived min anchor height (26 - (20-6) = 12) but within the
/// commitment-derived min anchor height (26 - (100-6) = 0, saturating), and
/// has matured past block_migration_depth. A commitment selector still gated
/// on the tx window would exclude it; gated on the commitment window it must
/// be included.
///
/// This exercises the selector (`tx_selector::select_best_txs`) directly via
/// `get_best_mempool_tx`, rather than through full block production +
/// self-validation: block-level anchor validation (`block_discovery.rs`) is
/// still gated on the tx window (a separate task) and would reject a produced
/// block before this fix lands, which is not what this test is scoped to.
#[test_log::test(tokio::test)]
async fn heavy_block_production_selects_commitment_over_commitment_window() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor, then mine well past the tx window (20) but stay
    // within the commitment window (100).
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(25).await?;

    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx.clone())
        .await
        .expect("commitment with old-but-in-window anchor should be accepted");

    let canonical_tip = node.get_canonical_chain().last().unwrap().block_hash();
    let selected = node.get_best_mempool_tx(canonical_tip).await?;
    assert!(
        selected
            .commitment_tx
            .iter()
            .any(|tx| tx.id() == stake_tx.id()),
        "commitment with in-window (commitment-window) anchor must be selected for inclusion"
    );

    node.stop().await;
    Ok(())
}

/// Block-level prevalidation (`block_discovery`) must accept a commitment whose
/// anchor is older than `tx_anchor_expiry_depth` (20) but still within
/// `commitment_anchor_expiry_depth` (100). Before this fix commitments were
/// validated against the shorter tx-anchor set, so such a block was rejected as
/// `InvalidAnchor` — even though the selector (B5) would include it. This test
/// drives the full produce + self-validate path end-to-end.
#[test_log::test(tokio::test)]
async fn heavy_commitment_in_window_anchor_validates() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing(); // tx 20, commitment 100, block_tree_depth 50
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor (height 1), then mine past the tx window (20) but
    // stay well within the commitment window (100). At tip ~26 the anchor is
    // ~25 deep: too old for a data tx, in-window for a commitment, and still
    // above the reorg floor (tip - block_tree_depth saturates to 0), so it is
    // resolved by-hash.
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(25).await?;

    // A stake commitment anchored at the old block: in-window, self-contained
    // (needs no prior staking), so it can validate through full block
    // production + prevalidation once B6 lands.
    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx.clone())
        .await
        .expect("commitment with old-but-in-window anchor should be accepted by the mempool");

    let (header, _payload, _txs, outcome) = node.mine_block_and_wait_for_validation().await?;
    assert!(
        matches!(outcome, BlockValidationOutcome::StoredOnNode(_)),
        "block with in-window commitment anchor must validate: {outcome:?}"
    );
    assert!(
        header.commitment_tx_ids().contains(&stake_tx.id()),
        "the produced block must include the in-window commitment"
    );

    node.stop().await;
    Ok(())
}

/// Block-level prevalidation must REJECT a commitment whose anchor is older than
/// `commitment_anchor_expiry_depth` (100). The mempool/selector would never
/// surface such a commitment, so an evil producer force-includes it; the block
/// must be rejected by `block_discovery` with `InvalidAnchor`.
#[test_log::test(tokio::test)]
async fn heavy_commitment_out_of_window_anchor_rejected() -> eyre::Result<()> {
    initialize_tracing();
    // Shrink the commitment window so we only need a handful of blocks to age an
    // anchor past it (mining a full 100-deep window would be prohibitively slow).
    // Must stay >= block_migration_depth (Config::validate).
    let commitment_depth: u16 = 8;
    let mut config = NodeConfig::testing();
    config
        .consensus
        .get_mut()
        .mempool
        .commitment_anchor_expiry_depth = commitment_depth;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor (height 1), then mine past the (shrunk) commitment
    // window so the anchor is out of window. At tip ~12 the anchor is ~11 deep >
    // commitment_anchor_expiry_depth (8).
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(commitment_depth as usize + 3).await?;

    // A stake anchored at the (now out-of-window) old block. Signed but never
    // ingested — the mempool would reject it; the evil producer injects it.
    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;

    let strategy = InjectCommitmentsStrategy {
        txs: vec![stake_tx.clone()],
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

    let result = strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;

    assert!(
        matches!(
            &result,
            Err(BlockDiscoveryError::InvalidAnchor { anchor, .. }) if *anchor == old_anchor
        ),
        "commitment anchored past the commitment window must be rejected as InvalidAnchor, got {result:?}"
    );

    node.stop().await;
    Ok(())
}

/// End-to-end: a commitment anchored beyond `tx_anchor_expiry_depth` but
/// within `commitment_anchor_expiry_depth` is ingested, produced, and
/// self-validated via the real `mine_block()` path (not a direct selector
/// call). This proves the selector (B5) and block-level anchor validation
/// (B6) agree: if either still gated on the shorter tx window, either the
/// commitment would never be selected, or a produced block containing it
/// would be rejected by the node's own validation and the canonical height
/// would not advance.
///
/// Contrast with a same-age data tx: covered by
/// `heavy_commitment_accepts_old_anchor_data_tx_rejects` above, which shows
/// ingress rejection under the shorter tx window; not duplicated here.
#[test_log::test(tokio::test)]
async fn heavy_e2e_commitment_in_window_anchor_produces_and_validates() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing(); // tx depth 20, commitment depth 100, block_migration_depth 6, block_tree_depth 50
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor (height 1), then mine past the tx window (20) but
    // stay within the commitment window (100), and past block_migration_depth
    // (6) so the anchor is matured.
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(25).await?;

    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx.clone())
        .await
        .expect("commitment with old-but-in-window anchor should be accepted");

    let height_before = node.get_canonical_chain_height().await;
    node.mine_block().await?;
    let height_after = node.get_canonical_chain_height().await;
    assert_eq!(
        height_after,
        height_before + 1,
        "the node must accept and canonicalize its own produced block"
    );

    let produced_block = node.get_block_by_height(height_after).await?;
    assert!(
        produced_block.commitment_tx_ids().contains(&stake_tx.id()),
        "the produced, validated block must include the in-window commitment"
    );

    node.stop().await;
    Ok(())
}

/// Same as the positive case, but the in-window anchor sits BELOW the reorg
/// floor so it must be resolved from the finalized block index rather than the
/// by-hash reorg-window walk. This exercises the NC-0042 by-hash-boundary ->
/// block-index handoff on the (now deeper) commitment window.
///
/// Config: tx window 4, block_tree_depth 8, block_migration_depth 1, commitment
/// window 100. Anchor at height 1; mined to tip ~10. The by-hash walk stops at
/// `tip - tx_window` (~6), so the anchor (height 1) is below it and is resolved
/// from the block index. The reorg floor is `tip - block_tree_depth` (~2), so
/// the anchor is also below the floor — the index entry is the branch-safe
/// resolution path.
#[test_log::test(tokio::test)]
async fn heavy_commitment_in_window_anchor_below_reorg_floor_validates() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing();
    {
        let c = config.consensus.get_mut();
        c.mempool.tx_anchor_expiry_depth = 4;
        c.block_tree_depth = 8;
        c.block_migration_depth = 1;
        // commitment window stays at the testing default (100), well above the
        // anchor's ~9-block age.
    }
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("N", 10)
        .await;

    // Capture an old anchor (height 1), then mine so the anchor ages past both
    // the tx window (4) and the reorg floor (block_tree_depth 8) but stays well
    // within the commitment window (100).
    let old_anchor = node.mine_block().await?.block_hash;
    node.mine_blocks(9).await?;

    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::new_stake(consensus, old_anchor);
    signer.sign_commitment(&mut stake_tx)?;
    node.ingest_commitment_tx(stake_tx.clone())
        .await
        .expect("commitment with old-but-in-window anchor should be accepted by the mempool");

    let (header, _payload, _txs, outcome) = node.mine_block_and_wait_for_validation().await?;
    assert!(
        matches!(outcome, BlockValidationOutcome::StoredOnNode(_)),
        "block with below-reorg-floor in-window commitment anchor must validate: {outcome:?}"
    );
    assert!(
        header.commitment_tx_ids().contains(&stake_tx.id()),
        "the produced block must include the in-window commitment"
    );

    node.stop().await;
    Ok(())
}
