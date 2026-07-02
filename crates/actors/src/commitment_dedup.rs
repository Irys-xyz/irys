use crate::block_ancestry::walk_ancestors_tree_then_db;
use irys_database::{canonical_commitment_included_height, db::IrysDatabaseExt as _};
use irys_domain::BlockTreeReadGuard;
use irys_types::{CommitmentTransaction, DatabaseProvider, H256, IrysBlockHeader};
use std::collections::HashSet;
use std::ops::ControlFlow;

/// Returns the tx_id of the first commitment in `commitment_txs` that is a
/// replay of one already canonically included, or `Ok(None)` if none are.
///
/// WHY this exists: the per-block commitment snapshot resets at every epoch
/// boundary, so its "already included" check cannot see a commitment that was
/// included in a PRIOR epoch. Without a durable check a miner could re-include
/// the same commitment in a later epoch.
///
/// Two-range contract, meeting at `floor = height - block_tree_depth` with no
/// gap:
///   - Reorg window `[floor, height)`: resolved by-hash along THIS block's own
///     ancestry (block tree, DB fallback), so a reorged-out sibling's inclusion
///     never counts. See [`ancestor_commitment_tx_ids`].
///   - Below `floor`: the chain is finalized, so a content-verified lookup
///     ([`canonical_commitment_included_height`], capped at `floor`) is
///     branch-invariant there.
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
    let floor = block_under_validation
        .height
        .saturating_sub(block_tree_depth);

    // One read txn covers every finalized-inclusion lookup (a single MDBX txn
    // instead of one per commitment). The walk's own DB descent (Phase 2 below)
    // runs in a separate txn to keep `ancestor_commitment_tx_ids` self-contained
    // and directly testable; merging the two would force threading a `tx` param
    // through the walk, breaking that seam.
    db.view_eyre(|read_tx| {
        for tx in commitment_txs {
            // In-memory reorg-window check first; only pay the DB read on a miss.
            if prior_ids.contains(&tx.id())
                || canonical_commitment_included_height(read_tx, &tx.id(), floor)?.is_some()
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
}
