use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::BlockTreeReadGuard;
use irys_types::{DatabaseProvider, H256, IrysBlockHeader};
use std::collections::HashSet;

/// Commitment tx_ids included in `block_under_validation`'s ancestors within
/// `[height - walk_depth, height)`, resolved by-hash (block tree, DB fallback).
///
/// Branch-correct: only walks THIS block's own ancestry, so it never counts a
/// reorged-out sibling. Callers use it for the reorg window (`walk_depth =
/// block_tree_depth`) and cover finalized inclusions below the floor via
/// `canonical_commitment_included_height`.
///
/// Fails closed: an in-window ancestor missing from BOTH the block tree and the
/// DB is a local inconsistency (every ancestor of a validatable block was
/// itself validated-and-stored when processed), so it `bail!`s rather than
/// returning a partial set. A partial set would let a replay above the break
/// point slip past both the by-hash set and the finalized lookup. Callers map
/// the error to a SoftInternal-park path (don't accept or reject — retry). The
/// clean stops (dropped below `min_height`, reached genesis) return normally.
pub fn ancestor_commitment_tx_ids(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    block_under_validation: &IrysBlockHeader,
    walk_depth: u64,
) -> eyre::Result<HashSet<H256>> {
    let mut ids = HashSet::new();
    let block_height = block_under_validation.height;
    let min_height = block_height.saturating_sub(walk_depth);

    let guard = block_tree.read();
    let mut cursor = block_under_validation.previous_block_hash;
    // Walk parents until we drop below min_height or hit genesis.
    loop {
        let header = match guard.get_block(&cursor) {
            Some(h) => h.clone(),
            None => match db.view_eyre(|tx| block_header_by_hash(tx, &cursor, false))? {
                Some(h) => h,
                None => eyre::bail!(
                    "commitment dedup walk: ancestor {cursor} (within the reorg \
                     window of block {} at height {block_height}) is missing from \
                     both the block tree and the DB",
                    block_under_validation.block_hash,
                ),
            },
        };
        if header.height < min_height {
            break;
        }
        ids.extend(header.commitment_tx_ids().iter().copied());
        if header.height == 0 {
            break;
        }
        cursor = header.previous_block_hash;
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::BlockTree;
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_types::{ConsensusConfig, SystemLedger, SystemTransactionLedger};
    use std::sync::{Arc, RwLock};

    fn signed_genesis() -> IrysBlockHeader {
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = 0;
        header.poa.chunk = Some(Default::default());
        header.test_sign();
        header
    }

    fn child_with_commitments(
        parent: &IrysBlockHeader,
        height: u64,
        commitment_tx_ids: Vec<H256>,
    ) -> IrysBlockHeader {
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.previous_block_hash = parent.block_hash;
        header.poa.chunk = Some(Default::default());
        header.system_ledgers = vec![SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment as u32,
            tx_ids: irys_types::H256List(commitment_tx_ids),
        }];
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
