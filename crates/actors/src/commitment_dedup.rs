use crate::block_ancestry::walk_ancestors_tree_then_db;
use irys_database::{canonical_commitment_included_height, db::IrysDatabaseExt as _};
use irys_domain::BlockTreeReadGuard;
use irys_types::{CommitmentTransaction, DatabaseProvider, H256, IrysBlockHeader};
use std::collections::HashSet;
use std::ops::ControlFlow;

/// Returns the tx_id of the first commitment in `commitment_txs` that is a
/// replay of one already canonically included, or of an earlier commitment in
/// this same slice, or `Ok(None)` if none are.
///
/// WHY this exists: the per-block commitment snapshot resets at every epoch
/// boundary, so its "already included" check cannot see a commitment that was
/// included in a PRIOR epoch. Without a durable check a miner could re-include
/// the same commitment in a later epoch.
///
/// It also covers the degenerate case of a commitment listed twice within the
/// block under validation itself: `CommitmentSnapshot::add_commitment` is
/// deliberately idempotent for Stake/Pledge (a commitment may legitimately be
/// first-included exactly once, and the snapshot has no way to tell "already
/// pending from this same block" apart from "already pending from a prior
/// one"), so the snapshot alone cannot reject a same-block duplicate.
///
/// Two-range contract, meeting at `floor = height - block_tree_depth` with no
/// gap:
///   - Reorg window `[floor, height)`: resolved by-hash along THIS block's own
///     ancestry (block tree, DB fallback), so a reorged-out sibling's inclusion
///     never counts. See [`ancestor_commitment_tx_ids`].
///   - Strictly below `floor`: the chain is finalized, so a content-verified
///     lookup ([`canonical_commitment_included_height`], capped at `floor - 1`)
///     is branch-invariant there. The walk owns the floor height itself: a
///     node-local `MigratedBlockHashes` row there is reorg-mutable, and since
///     the walk resolves the floor on the candidate's own branch, a
///     lookup answer at that height is never a true positive — the same
///     strictly-below cap as the data-tx fallbacks in `data_txs_are_valid` and
///     `resolve_promoted_on_branch` (keep the cap forms in sync).
///
/// The in-memory reorg-window set is checked before the DB lookup, so a
/// commitment-free branch or an in-window hit never pays for a finalized read.
/// Iteration order is preserved: the first replayed tx_id in `commitment_txs`
/// is the one returned.
///
/// Fails closed (propagates `Err`) on any lookup inconsistency; callers map the
/// error to a park/retry path rather than accepting or rejecting the block.
pub fn find_replayed_commitment(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    block_under_validation: &IrysBlockHeader,
    commitment_txs: &[CommitmentTransaction],
    block_tree_depth: u64,
) -> eyre::Result<Option<H256>> {
    // A commitment-free block has nothing to dedup: skip the ancestor walk and
    // the read txn entirely (the common case).
    if commitment_txs.is_empty() {
        return Ok(None);
    }

    let prior_ids =
        ancestor_commitment_tx_ids(block_tree, db, block_under_validation, block_tree_depth)?;
    // Cap the finalized lookup strictly below the walk window (see the
    // two-range contract above). Zero coverage loss: the walk covers `[floor,
    // height)` inclusively on the candidate's branch.
    let finalized_cap = block_under_validation
        .height
        .saturating_sub(block_tree_depth)
        .saturating_sub(1);

    // One read txn covers every finalized-inclusion lookup (a single MDBX txn
    // instead of one per commitment). The walk's own DB descent (Phase 2 below)
    // runs in a separate txn to keep `ancestor_commitment_tx_ids` self-contained
    // and directly testable; merging the two would force threading a `tx` param
    // through the walk, breaking that seam.
    let mut seen: HashSet<H256> = HashSet::with_capacity(commitment_txs.len());
    db.view_eyre(|read_tx| {
        for tx in commitment_txs {
            // Within-block duplicate first: cheapest check, and it must not
            // depend on any DB or ancestry state. Then the in-memory
            // reorg-window check; only pay the DB read on a miss on both.
            if !seen.insert(tx.id())
                || prior_ids.contains(&tx.id())
                || canonical_commitment_included_height(read_tx, &tx.id(), finalized_cap)?.is_some()
            {
                return Ok(Some(tx.id()));
            }
        }
        Ok(None)
    })
}

/// Commitment tx_ids included in `block_under_validation`'s ancestors within
/// `[height - walk_depth, height)`, resolved by-hash (block tree, DB fallback).
///
/// Branch-correct: only walks THIS block's own ancestry, so it never counts a
/// reorged-out sibling.
///
/// Two-phase to avoid cloning full headers (up to ~`block_tree_depth` of them,
/// each carrying a ~256KB PoA chunk) and to avoid holding the block-tree read
/// guard across DB I/O. Soundness of the split: the block tree holds a
/// contiguous recent window, so once the parent-walk falls out of the tree it
/// never re-enters (a migrated block's ancestors are all migrated too).
/// Therefore:
///   - Phase 1 walks under ONE tree read guard, borrowing each header (no
///     clone) while ancestors are in the tree, then drops the guard.
///   - Phase 2 continues the descent in ONE DB read txn via
///     `block_header_by_hash`.
///
/// Fails closed: an in-window ancestor missing from BOTH the block tree and the
/// DB is a local inconsistency (every ancestor of a validatable block was
/// itself validated-and-stored when processed), so it `bail!`s rather than
/// returning a partial set. A partial set would let a replay above the break
/// point slip past both the by-hash set and the finalized lookup. Callers map
/// the error to a SoftInternal-park path (don't accept or reject — retry). The
/// clean stops (dropped below `min_height`, reached genesis) return normally.
pub(crate) fn ancestor_commitment_tx_ids(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    block_under_validation: &IrysBlockHeader,
    walk_depth: u64,
) -> eyre::Result<HashSet<H256>> {
    let mut ids = HashSet::new();
    let block_height = block_under_validation.height;
    let min_height = block_height.saturating_sub(walk_depth);
    walk_ancestors_tree_then_db(
        block_tree,
        db,
        block_under_validation.previous_block_hash,
        min_height,
        |header| {
            ids.extend(header.commitment_tx_ids().iter().copied());
            Ok(ControlFlow::Continue(()))
        },
    )
    .map_err(|err| {
        eyre::eyre!(
            "commitment dedup walk for block {} at height {block_height} failed: {err}",
            block_under_validation.block_hash
        )
    })?;

    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::BlockTree;
    use irys_testing_utils::{IrysBlockHeaderTestExt as _, mock_header_with_commitments};
    use irys_types::ConsensusConfig;
    use std::sync::{Arc, RwLock};

    fn signed_genesis() -> IrysBlockHeader {
        let mut header = mock_header_with_commitments(0, vec![]);
        header.test_sign();
        header
    }

    fn child_with_commitments(
        parent: &IrysBlockHeader,
        height: u64,
        commitment_tx_ids: Vec<H256>,
    ) -> IrysBlockHeader {
        let mut header = mock_header_with_commitments(height, commitment_tx_ids);
        header.previous_block_hash = parent.block_hash;
        header.test_sign();
        header
    }

    fn test_db() -> (irys_testing_utils::tempfile::TempDir, DatabaseProvider) {
        use irys_database::IrysDatabaseArgs as _;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = irys_database::open_or_create_db(
            tmp.path(),
            irys_database::tables::IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        (tmp, DatabaseProvider(Arc::new(db)))
    }

    // Full behavioral coverage is in the integration tests (Tasks A3, A5); this
    // pins the boundary: walk_depth 0 inspects no ancestors.
    #[test]
    fn walk_depth_zero_returns_empty() {
        let genesis = signed_genesis();
        let commitment_id = H256::random();
        let block1 = child_with_commitments(&genesis, 1, vec![commitment_id]);

        let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));
        let (_tmp, db) = test_db();

        let ids = ancestor_commitment_tx_ids(&block_tree, &db, &block1, 0).unwrap();
        assert!(ids.is_empty());
    }

    // An in-window ancestor missing from both the tree and the DB must fail
    // loud rather than return a partial set (which could hide a replay).
    #[test]
    fn unknown_in_window_ancestor_errors() {
        let genesis = signed_genesis();
        // Child whose parent hash resolves nowhere: the tree holds only genesis
        // and the DB is empty.
        let mut block2 = child_with_commitments(&genesis, 2, vec![H256::random()]);
        block2.previous_block_hash = H256::random();

        let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));
        let (_tmp, db) = test_db();

        let err = ancestor_commitment_tx_ids(&block_tree, &db, &block2, 100).unwrap_err();
        assert!(
            err.to_string().contains("missing from"),
            "expected fail-closed error, got: {err}"
        );
    }

    fn signed_commitment() -> CommitmentTransaction {
        let config = ConsensusConfig::testing();
        let mut tx = CommitmentTransaction::new_stake(&config, H256::random());
        irys_types::irys::IrysSigner::random_signer(&config)
            .sign_commitment(&mut tx)
            .unwrap();
        tx
    }

    // `CommitmentSnapshot::add_commitment` is idempotent for Stake/Pledge, so
    // the snapshot alone cannot reject a commitment listed twice in the same
    // block under validation. The dedup must catch this itself.
    #[test]
    fn same_block_duplicate_commitment_is_replay() {
        let genesis = signed_genesis();
        let block1 = child_with_commitments(&genesis, 1, vec![]);
        let tx = signed_commitment();

        let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));
        let (_tmp, db) = test_db();

        let result =
            find_replayed_commitment(&block_tree, &db, &block1, &[tx.clone(), tx.clone()], 100)
                .unwrap();
        assert_eq!(result, Some(tx.id()));
    }

    #[test]
    fn distinct_commitments_not_flagged() {
        let genesis = signed_genesis();
        let block1 = child_with_commitments(&genesis, 1, vec![]);
        let tx_a = signed_commitment();
        let tx_b = signed_commitment();
        assert_ne!(tx_a.id(), tx_b.id());

        let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));
        let (_tmp, db) = test_db();

        let result =
            find_replayed_commitment(&block_tree, &db, &block1, &[tx_a, tx_b], 100).unwrap();
        assert_eq!(result, None);
    }

    // The by-hash walk owns the floor height (`height - block_tree_depth`)
    // inclusively on the candidate's own branch, so a node-local
    // `MigratedBlockHashes` inclusion EXACTLY at the floor is reorg-mutable and
    // must never decide replay-validity. The finalized lookup is capped strictly
    // below the floor: a boundary-height sibling on a divergent local branch
    // carrying the same commitment is not a duplicate.
    #[test]
    fn floor_height_sibling_inclusion_is_not_replay() -> eyre::Result<()> {
        use irys_database::tables::MigratedBlockHashes;
        use irys_database::{batch_set_commitment_tx_included_height, insert_block_header};
        use reth_db::transaction::DbTxMut as _;

        let genesis = signed_genesis();
        // b1 on the candidate's own branch at the floor height (1), carrying no
        // commitments; only in the DB (below the retained tree window), so the
        // walk resolves the floor via its DB fallback and finds no replay.
        let b1 = child_with_commitments(&genesis, 1, vec![]);
        // Candidate at height 2 carrying commitment C; floor = 2 - 1 = 1.
        let commitment = signed_commitment();
        let cid = commitment.id();
        let b2 = child_with_commitments(&b1, 2, vec![cid]);

        // Node-local SIBLING at the floor height whose commitment ledger carries
        // C: the local canonical chain diverges from the candidate's branch at
        // the boundary, and the metadata row points at this sibling.
        let mut sibling = mock_header_with_commitments(1, vec![cid]);
        sibling.test_sign();

        let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));
        let (_tmp, db) = test_db();

        db.update_eyre(|tx| {
            insert_block_header(tx, &b1)?;
            insert_block_header(tx, &sibling)?;
            tx.put::<MigratedBlockHashes>(1, sibling.block_hash)?;
            batch_set_commitment_tx_included_height(tx, std::iter::once(&cid), 1)?;
            Ok(())
        })?;

        let result = find_replayed_commitment(&block_tree, &db, &b2, &[commitment], 1)?;
        assert_eq!(
            result, None,
            "a canonical inclusion exactly at the walk floor must not count as replay"
        );
        Ok(())
    }

    // A genuine finalized inclusion STRICTLY below the floor is still flagged:
    // the stricter cap loses no coverage. C2 is included at height 1, the
    // candidate at height 3 replays it, floor = 2 (walk covers height 2 only),
    // so the finalized lookup (capped at 1) is the sole path that can see it.
    #[test]
    fn finalized_inclusion_below_floor_is_replay() -> eyre::Result<()> {
        use irys_database::tables::MigratedBlockHashes;
        use irys_database::{batch_set_commitment_tx_included_height, insert_block_header};
        use reth_db::transaction::DbTxMut as _;

        let genesis = signed_genesis();
        let commitment = signed_commitment();
        let cid = commitment.id();
        // b1 finalizes C2 at height 1 (below the candidate's floor of 2).
        let b1 = child_with_commitments(&genesis, 1, vec![cid]);
        // b2 at the floor carries no commitments; the walk covers it and finds
        // nothing.
        let b2 = child_with_commitments(&b1, 2, vec![]);
        // Candidate at height 3 replays C2; floor = 3 - 1 = 2, cap = 1.
        let b3 = child_with_commitments(&b2, 3, vec![cid]);

        let cache = BlockTree::new(&genesis, ConsensusConfig::testing());
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(cache)));
        let (_tmp, db) = test_db();

        db.update_eyre(|tx| {
            insert_block_header(tx, &b1)?;
            insert_block_header(tx, &b2)?;
            tx.put::<MigratedBlockHashes>(1, b1.block_hash)?;
            batch_set_commitment_tx_included_height(tx, std::iter::once(&cid), 1)?;
            Ok(())
        })?;

        let result = find_replayed_commitment(&block_tree, &db, &b3, &[commitment], 1)?;
        assert_eq!(
            result,
            Some(cid),
            "a finalized inclusion strictly below the floor must still be a replay"
        );
        Ok(())
    }
}
