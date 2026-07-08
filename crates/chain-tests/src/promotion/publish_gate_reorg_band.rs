use crate::utils::IrysNodeTest;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::CachedDataRoot;
use irys_database::tables::{CachedDataRoots, MigratedBlockHashes};
use irys_database::{insert_block_header, insert_tx_header, set_data_tx_included_height};
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataLedger, H256, H256List, IrysBlockHeader, NodeConfig, UnixTimestamp, irys::IrysSigner,
};
use reth_db::transaction::DbTxMut as _;
use tracing::info;

/// Fabricate a canonical (migrated) Submit inclusion for `txid` at `height`:
/// a mock block carrying the tx in its Submit ledger, `MigratedBlockHashes[height]`
/// repointed at it, and the `IrysDataTxMetadata.included_height` hint set. The
/// block's parent is a real, already-migrated header so the migrated-metadata
/// range lookup (`find_canonical_ledger_range`) resolves without error and the
/// tx can proceed to promotion when the gate admits it.
fn plant_submit_inclusion(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    txid: H256,
    height: u64,
    prev_block_hash: H256,
) -> eyre::Result<()> {
    let mut planted = IrysBlockHeader::new_mock_header();
    planted.height = height;
    planted.block_hash = H256::random();
    planted.previous_block_hash = prev_block_hash;
    planted.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![txid]);
    planted.data_ledgers[DataLedger::Submit].total_chunks = 0;
    let planted_hash = planted.block_hash;

    node.node_ctx.db.update_eyre(|db_tx| {
        insert_block_header(db_tx, &planted)?;
        db_tx.put::<MigratedBlockHashes>(height, planted_hash)?;
        set_data_tx_included_height(db_tx, &txid, height)?;
        Ok(())
    })?;
    Ok(())
}

/// Manufacture a publish candidate whose data tx the node never included in a
/// real block: insert the tx header into the DB (so it resolves), cache its
/// data_root with the tx pinned in `txid_set`, and store a valid, staked ingress
/// proof for it. This is exactly the state the publish-candidate scan reads
/// before applying the prior-Submit gate.
async fn make_publish_candidate(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    config: &NodeConfig,
    seed: u8,
) -> eyre::Result<H256> {
    let chunks: Vec<[u8; 32]> = vec![
        [seed; 32],
        [seed.wrapping_add(1); 32],
        [seed.wrapping_add(2); 32],
    ];
    let data: Vec<u8> = chunks.iter().flat_map(|c| c.iter()).copied().collect();

    let price_info = node
        .get_data_price(DataLedger::Publish, data.len() as u64)
        .await?;
    let data_tx = signer.create_publish_transaction(
        data,
        node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = signer.sign_transaction(data_tx)?;
    let txid = data_tx.header.id;
    let data_root = data_tx.header.data_root;

    // The tx is never posted to the mempool; the publish-candidate lookup falls
    // back to the DB, so the header must be resolvable there (otherwise the
    // stale-txid debug_assert in get_publish_txs_and_proofs hard-fails).
    node.node_ctx.db.update_eyre(|db_tx| {
        insert_tx_header(db_tx, &data_tx.header)?;
        let cached = CachedDataRoot {
            data_size: data_tx.header.data_size,
            data_size_confirmed: true,
            txid_set: vec![txid],
            block_set: vec![],
            expiry_height: Some(1000),
            cached_at: UnixTimestamp::from_secs(0),
        };
        db_tx.put::<CachedDataRoots>(data_root, cached)?;
        Ok(())
    })?;

    let anchor = node.get_anchor().await?;
    let ingress_proof = generate_ingress_proof(
        signer,
        data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        anchor,
    )?;
    node.node_ctx.db.update_eyre(|db_tx| {
        irys_database::store_ingress_proof_checked(db_tx, &ingress_proof, signer)
    })?;

    Ok(txid)
}

/// Producer-side sibling of the validator pin
/// `test_prevalidation_ignores_content_verified_row_inside_walk_window`.
///
/// The producer's publish prior-Submit gate consults the content-verified
/// canonical Submit index only for FINALIZED heights — capped at
/// `tip - block_tree_depth`. A canonical Submit row INSIDE the reorg-mutable
/// band (strictly above that floor) describes only this node's current chain,
/// not necessarily the branch being built, so it must NOT satisfy the gate:
/// the parent's own ancestry is already resolved by the canonical fold, and any
/// in-band inclusion must come from the fold, never the DB.
///
/// This test plants two content-verified canonical Submit inclusions for two txs
/// that appear in NO real block-tree block (so the fold misses both, and the DB
/// gate is the deciding check):
///   - `band_txid` at `floor + 1` (inside the reorg-mutable band) — must be
///     SKIPPED: no finalized prior Submit at or below the cap.
///   - `control_txid` at `floor - 1` (finalized) — must be PROMOTED: the gate is
///     satisfied by a branch-invariant row, proving the cap does not over-skip.
///
/// Under the old `parent_height` cap the band row satisfied the gate and the tx
/// was wrongly promoted; under the finalized-floor cap only the control survives.
#[test_log::test(tokio::test)]
async fn heavy_publish_gate_skips_reorg_band_submit_row() -> eyre::Result<()> {
    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            // Small depths so the finalized floor (tip - block_tree_depth) is
            // reachable quickly while leaving a reorg-mutable band above it.
            // Invariants (Config::validate): migration < tree_depth, and
            // block_migration <= tx_anchor_expiry <= block_tree_depth.
            consensus.block_migration_depth = 1;
            consensus.block_tree_depth = 3;
            consensus.mempool.tx_anchor_expiry_depth = 3;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
            consensus
                .hardforks
                .frontier
                .number_of_ingress_proofs_from_assignees = 0;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let signer = config.signer();
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", 30)
        .await;

    // Mine enough blocks that the finalized floor is strictly positive and a
    // reorg-mutable band exists above it.
    node.mine_blocks(6).await?;

    let tip = node.get_canonical_chain_height().await;
    let floor = tip - config.consensus_config().block_tree_depth;
    assert!(floor >= 1, "finalized floor must be positive (tip={tip})");
    let band_height = floor + 1; // inside the reorg-mutable band, above the cap
    let control_height = floor - 1; // finalized, at/below the cap

    info!(tip, floor, band_height, control_height, "geometry");

    // Two publish candidates the node never included in any real block.
    let band_txid = make_publish_candidate(&node, &signer, &config, 0x10).await?;
    let control_txid = make_publish_candidate(&node, &signer, &config, 0x40).await?;

    // Plant content-verified canonical Submit inclusions at the two heights.
    // Parent = the real, already-migrated header one below the planted height.
    // Read the real parent headers from the block index: at block_tree_depth = 3
    // these heights have already been pruned from the in-memory block tree.
    let band_prev = node
        .get_block_by_height_from_index(band_height - 1, false)?
        .block_hash;
    let control_prev = node
        .get_block_by_height_from_index(control_height - 1, false)?
        .block_hash;
    plant_submit_inclusion(&node, band_txid, band_height, band_prev)?;
    plant_submit_inclusion(&node, control_txid, control_height, control_prev)?;

    // Run producer tx selection against the canonical tip (the parent we build on).
    let canonical_tip = node.get_canonical_chain().last().unwrap().block_hash();
    let mempool_txs = node.get_best_mempool_tx(canonical_tip).await?;
    let promoted: Vec<H256> = mempool_txs.publish_tx.txs.iter().map(|h| h.id).collect();

    // Regression pin: the in-band row must NOT satisfy the gate.
    assert!(
        !promoted.contains(&band_txid),
        "regression: producer promoted a publish candidate whose only Submit \
         inclusion sits INSIDE the reorg-mutable band (height {band_height} > \
         finalized floor {floor}) — such a row describes only this node's chain, \
         not necessarily the branch being built, so the prior-Submit gate must \
         skip it. promoted={promoted:?}"
    );

    // Control: a finalized Submit row (height {control_height} <= floor {floor})
    // must still satisfy the gate, proving the cap does not over-skip.
    assert!(
        promoted.contains(&control_txid),
        "expected the publish candidate with a FINALIZED Submit inclusion \
         (height {control_height} <= floor {floor}) to be promoted; the cap must \
         not over-skip branch-invariant rows. promoted={promoted:?}"
    );

    node.stop().await;
    Ok(())
}
