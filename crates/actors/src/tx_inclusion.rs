//! Canonical-only lookup: given a tx_id and a height bound, find the ledger
//! chunk range the tx contributed to its Submit ledger.
//!
//! Used by validation, chunk ingress, and cache pruning to determine the
//! canonical confirming block for a tx without consulting `CachedDataRoots`.
//!
//! Two-stage lookup:
//!   1. Migrated path: [`IrysDataTxMetadata`].`included_height` +
//!      `MigratedBlockHashes` — O(1) DB read.
//!   2. Pre-migration fallback: walk `block_tree` ≤ `block_migration_depth`
//!      blocks back from `max_height`, filtered to `ChainState::Onchain`.

use irys_database::{
    block_header_by_hash, tables::MigratedBlockHashes, tx_header_by_txid_canonical,
};
use irys_domain::{BlockTreeReadGuard, ChainState};
use irys_types::{
    DataLedger, IrysBlockHeader, IrysTransactionId, LedgerChunkOffset, LedgerChunkRange,
    app_state::DatabaseProvider,
};
use nodit::interval::ii;
use reth_db::Database as _;
use reth_db::transaction::DbTx as _;

/// Returns the Submit-ledger chunk range that this tx contributed to its
/// confirming canonical block, or `None` if the tx is not yet confirmed on
/// canonical at or before `max_height`.
pub fn find_canonical_ledger_range(
    tx_id: &IrysTransactionId,
    max_height: u64,
    block_migration_depth: u32,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<LedgerChunkRange>> {
    if let Some(range) = lookup_via_migrated_metadata(tx_id, max_height, db)? {
        return Ok(Some(range));
    }
    lookup_via_block_tree(tx_id, max_height, block_migration_depth, block_tree, db)
}

fn lookup_via_migrated_metadata(
    tx_id: &IrysTransactionId,
    max_height: u64,
    db: &DatabaseProvider,
) -> eyre::Result<Option<LedgerChunkRange>> {
    // `tx_header_by_txid_canonical` already enforces:
    //   - tx exists
    //   - included_height is set
    //   - included_height ≤ max_height
    //   - MigratedBlockHashes[included_height] is canonical
    db.view(|tx| -> eyre::Result<Option<LedgerChunkRange>> {
        let Some(header) = tx_header_by_txid_canonical(tx, tx_id, max_height)? else {
            return Ok(None);
        };
        let Some(included_height) = header.metadata().included_height else {
            return Ok(None);
        };
        let Some(block_hash) = tx.get::<MigratedBlockHashes>(included_height)? else {
            return Ok(None);
        };
        // MigratedBlockHashes attested this hash is canonical at this height;
        // a missing IrysBlockHeaders entry means the two tables disagree.
        let block = block_header_by_hash(tx, &block_hash, false)?.ok_or_else(|| {
            eyre::eyre!(
                "canonical metadata inconsistent: MigratedBlockHashes[{}] = {} but IrysBlockHeaders has no entry for that hash",
                included_height,
                block_hash
            )
        })?;
        let prev = if block.height == 0 {
            None
        } else {
            // Non-genesis: parent must be stored (parents migrate before
            // children).  A None here would silently turn into prev_total = 0
            // and produce a wrong range, so treat it as corruption.
            Some(
                block_header_by_hash(tx, &block.previous_block_hash, false)?.ok_or_else(|| {
                    eyre::eyre!(
                        "block header storage inconsistent: block {} at height {} references prev {} which is missing from IrysBlockHeaders",
                        block.block_hash,
                        block.height,
                        block.previous_block_hash
                    )
                })?,
            )
        };
        compute_submit_range(&block, prev.as_ref())
    })?
}

fn lookup_via_block_tree(
    tx_id: &IrysTransactionId,
    max_height: u64,
    block_migration_depth: u32,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<LedgerChunkRange>> {
    let tree = block_tree.read();
    let (canonical, _tip_idx) = tree.get_canonical_chain();

    // Anchor the migration window to `max_height`, not the chain tip.  For
    // callers asking about a historical height (e.g. validation of a block
    // at height H using parent_height = H-1, during reorg replay), the
    // unmigrated entries we care about are the `block_migration_depth` blocks
    // ending at max_height — not the blocks at the current tip.  The old
    // tip-anchored window silently dropped canonical confirmations whenever
    // max_height was far enough below the tip that the tip-anchored slice
    // contained only blocks with `height > max_height`.
    let depth = block_migration_depth as usize;
    let Some(max_idx) = canonical
        .iter()
        .rposition(|entry| entry.header().height <= max_height)
    else {
        return Ok(None);
    };
    let start_idx = max_idx.saturating_sub(depth.saturating_sub(1));
    for entry in canonical[start_idx..=max_idx].iter().rev() {
        let block = entry.header();
        let submit_txs = &block.data_ledgers[DataLedger::Submit].tx_ids.0;
        if !submit_txs.iter().any(|t| t == tx_id) {
            continue;
        }
        // Canonical-chain entries should be Onchain, but check defensively.
        let Some((_, state)) = tree.get_block_and_status(&block.block_hash) else {
            continue;
        };
        if !matches!(state, ChainState::Onchain) {
            continue;
        }

        let prev_in_tree = tree.get_block(&block.previous_block_hash).cloned();
        let prev = if let Some(p) = prev_in_tree {
            Some(p)
        } else if block.height == 0 {
            None
        } else {
            // Same invariant as the migrated path: a non-genesis canonical
            // block's parent must be reachable.  None from both tree and DB
            // would silently degrade the range; treat it as corruption.
            let from_db = db
                .view(|tx| block_header_by_hash(tx, &block.previous_block_hash, false))??;
            Some(from_db.ok_or_else(|| {
                eyre::eyre!(
                    "block header storage inconsistent: block {} at height {} references prev {} which is missing from both block_tree and IrysBlockHeaders",
                    block.block_hash,
                    block.height,
                    block.previous_block_hash
                )
            })?)
        };
        return compute_submit_range(block.as_ref(), prev.as_ref());
    }
    Ok(None)
}

/// Compute `[prev.submit.total_chunks, block.submit.total_chunks - 1]` as a
/// `LedgerChunkRange` (inclusive-inclusive), matching the historical
/// `get_ledger_range` semantics.  Returns `None` if the block added no
/// Submit-ledger chunks.  Returns `Err` if `block.total < prev.total` (data
/// corruption).
fn compute_submit_range(
    block: &IrysBlockHeader,
    prev: Option<&IrysBlockHeader>,
) -> eyre::Result<Option<LedgerChunkRange>> {
    let total = block.data_ledgers[DataLedger::Submit].total_chunks;
    let prev_total = prev
        .map(|p| p.data_ledgers[DataLedger::Submit].total_chunks)
        .unwrap_or(0);
    // Regression must be checked before any short-circuit: total < prev_total
    // is corruption regardless of whether total is zero.
    if total < prev_total {
        return Err(eyre::eyre!(
            "Block {} has total_chunks ({}) < prev block total_chunks ({}), data corruption",
            block.block_hash,
            total,
            prev_total
        ));
    }
    if total == prev_total {
        return Ok(None);
    }
    // total > prev_total ≥ 0, so total > 0 and `total - 1` cannot underflow.
    Ok(Some(LedgerChunkRange(ii(
        LedgerChunkOffset::from(prev_total),
        LedgerChunkOffset::from(total - 1),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_database::{
        IrysDatabaseArgs as _, insert_tx_header, open_or_create_db, set_data_tx_included_height,
        tables::{IrysBlockHeaders, IrysTables},
    };
    use irys_domain::{
        BlockTree, BlockTreeReadGuard, ChainState, CommitmentSnapshot, EpochSnapshot,
        dummy_ema_snapshot,
    };
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_testing_utils::utils::{TempDirBuilder, tempfile};
    use irys_types::{
        BlockTransactions, ConsensusConfig, DataTransactionHeader, DataTransactionHeaderV1,
        DataTransactionHeaderV1WithMetadata, DataTransactionMetadata, H256, H256List,
        IrysBlockHeader, SealedBlock, U256, app_state::DatabaseProvider,
    };
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::DbTxMut as _;
    use std::sync::{Arc, RwLock};

    fn open_db() -> eyre::Result<(DatabaseProvider, tempfile::TempDir)> {
        let tmp = TempDirBuilder::new().build();
        let env = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        Ok((DatabaseProvider(Arc::new(env)), tmp))
    }

    /// Build a signed block header with custom Submit-ledger totals and tx_ids.
    fn make_signed_header(
        height: u64,
        previous_block_hash: H256,
        cumulative_diff: u64,
        submit_total_chunks: u64,
        submit_tx_ids: Vec<H256>,
    ) -> IrysBlockHeader {
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.previous_block_hash = previous_block_hash;
        header.cumulative_diff = U256::from(cumulative_diff);
        let submit = &mut header.data_ledgers[DataLedger::Submit as usize];
        submit.total_chunks = submit_total_chunks;
        submit.tx_ids = H256List(submit_tx_ids);
        header.test_sign();
        header
    }

    fn put_block_header(db: &DatabaseProvider, header: &IrysBlockHeader) -> eyre::Result<()> {
        db.update(|tx| -> eyre::Result<()> {
            tx.put::<IrysBlockHeaders>(header.block_hash, header.clone().into())?;
            Ok(())
        })??;
        Ok(())
    }

    fn mark_migrated(db: &DatabaseProvider, height: u64, hash: H256) -> eyre::Result<()> {
        db.update(|tx| -> eyre::Result<()> {
            tx.put::<MigratedBlockHashes>(height, hash)?;
            Ok(())
        })??;
        Ok(())
    }

    fn write_tx_with_included_height(
        db: &DatabaseProvider,
        tx_id: H256,
        data_root: H256,
        included_height: u64,
    ) -> eyre::Result<()> {
        let mut metadata = DataTransactionMetadata::new();
        metadata.included_height = Some(included_height);
        let header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: tx_id,
                data_root,
                ledger_id: DataLedger::Submit as u32,
                ..Default::default()
            },
            metadata,
        });
        db.update(|tx| -> eyre::Result<()> {
            insert_tx_header(tx, &header)?;
            set_data_tx_included_height(tx, &tx_id, included_height)?;
            Ok(())
        })??;
        Ok(())
    }

    /// Build a BlockTree with `genesis` seeded as Onchain, then add `extra` blocks
    /// via `add_common` using `SealedBlock::new_unchecked` (so we don't have to
    /// populate matching tx bodies).
    fn build_tree(genesis: IrysBlockHeader, extras: Vec<IrysBlockHeader>) -> BlockTreeReadGuard {
        // Genesis must seal successfully — it has no tx_ids in either ledger.
        let mut tree = BlockTree::new(&genesis, ConsensusConfig::testing());
        for header in extras {
            let sealed = Arc::new(SealedBlock::new_unchecked(
                Arc::new(header.clone()),
                BlockTransactions::default(),
            ));
            tree.add_common(
                header.block_hash,
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                dummy_ema_snapshot(),
                ChainState::Onchain,
            )
            .expect("add_common succeeds");
        }
        BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)))
    }

    fn empty_block_tree_guard() -> BlockTreeReadGuard {
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = H256::zero();
        genesis.cumulative_diff = U256::from(0);
        genesis.test_sign();
        let tree = BlockTree::new(&genesis, ConsensusConfig::testing());
        BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)))
    }

    #[test_log::test(tokio::test)]
    async fn migrated_tx_returns_canonical_range() -> eyre::Result<()> {
        let (db, _tmp) = open_db()?;

        let tx_id = H256::random();
        let h0 = make_signed_header(0, H256::zero(), 0, 10, vec![]);
        let h1 = make_signed_header(1, h0.block_hash, 1, 25, vec![tx_id]);

        put_block_header(&db, &h0)?;
        put_block_header(&db, &h1)?;
        mark_migrated(&db, 0, h0.block_hash)?;
        mark_migrated(&db, 1, h1.block_hash)?;

        write_tx_with_included_height(&db, tx_id, H256::random(), 1)?;

        let guard = empty_block_tree_guard();
        let range = find_canonical_ledger_range(
            &tx_id,
            /* max_height */ 5,
            ConsensusConfig::testing().block_migration_depth,
            &guard,
            &db,
        )?
        .ok_or_else(|| eyre::eyre!("expected Some(range)"))?;

        // [10, 24] inclusive — the 15 chunks added in h1.
        assert_eq!(range.start(), LedgerChunkOffset::from(10_u64));
        assert_eq!(range.end(), LedgerChunkOffset::from(24_u64));
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn missing_metadata_and_no_block_tree_returns_none() -> eyre::Result<()> {
        let (db, _tmp) = open_db()?;
        let guard = empty_block_tree_guard();

        let tx_id = H256::random();
        let result = find_canonical_ledger_range(
            &tx_id,
            10,
            ConsensusConfig::testing().block_migration_depth,
            &guard,
            &db,
        )?;
        assert!(result.is_none());
        Ok(())
    }

    /// Regression: migrated lookup must error when MigratedBlockHashes points
    /// at a hash that IrysBlockHeaders has no entry for.  Earlier code returned
    /// `Ok(None)` silently and let the caller treat a tx as unconfirmed even
    /// though the canonical-height table disagreed.
    #[test_log::test(tokio::test)]
    async fn migrated_lookup_errors_when_canonical_header_missing() -> eyre::Result<()> {
        let (db, _tmp) = open_db()?;

        let tx_id = H256::random();
        // Mark a hash as canonical at height 1 in MigratedBlockHashes but
        // *omit* the matching IrysBlockHeaders entry, simulating cross-table
        // inconsistency.
        let phantom_hash = H256::random();
        mark_migrated(&db, 1, phantom_hash)?;
        write_tx_with_included_height(&db, tx_id, H256::random(), 1)?;

        let guard = empty_block_tree_guard();
        let err = find_canonical_ledger_range(
            &tx_id,
            /* max_height */ 5,
            ConsensusConfig::testing().block_migration_depth,
            &guard,
            &db,
        )
        .expect_err("missing canonical header must surface as Err");
        let msg = err.to_string();
        assert!(
            msg.contains("canonical metadata inconsistent"),
            "unexpected error message: {msg}"
        );
        Ok(())
    }

    /// Regression: migrated lookup must error when a non-genesis block's
    /// `previous_block_hash` is missing from IrysBlockHeaders.  Otherwise
    /// `compute_submit_range` would run with `prev_total = 0` and emit a
    /// silently-wrong range starting at offset 0.
    #[test_log::test(tokio::test)]
    async fn migrated_lookup_errors_when_prev_header_missing() -> eyre::Result<()> {
        let (db, _tmp) = open_db()?;

        let tx_id = H256::random();
        // Write h1 (the confirming block) but *not* h0 (its parent) to
        // IrysBlockHeaders.  Mark both heights migrated.
        let h0_phantom = H256::random();
        let h1 = make_signed_header(1, h0_phantom, 1, 25, vec![tx_id]);

        put_block_header(&db, &h1)?;
        mark_migrated(&db, 0, h0_phantom)?;
        mark_migrated(&db, 1, h1.block_hash)?;
        write_tx_with_included_height(&db, tx_id, H256::random(), 1)?;

        let guard = empty_block_tree_guard();
        let err = find_canonical_ledger_range(
            &tx_id,
            /* max_height */ 5,
            ConsensusConfig::testing().block_migration_depth,
            &guard,
            &db,
        )
        .expect_err("missing prev header must surface as Err");
        let msg = err.to_string();
        assert!(
            msg.contains("block header storage inconsistent"),
            "unexpected error message: {msg}"
        );
        Ok(())
    }

    /// Regression: `compute_submit_range` must flag `total < prev_total` as
    /// corruption even when `total == 0`.  An earlier shape of this function
    /// short-circuited on `total == 0` before the regression check and would
    /// have silently returned `Ok(None)` for a `prev_total = 100, total = 0`
    /// header pair.
    #[test_log::test(tokio::test)]
    async fn compute_submit_range_detects_zero_total_regression() {
        let prev = make_signed_header(
            /* height */ 0,
            H256::zero(),
            /* cumulative_diff */ 0,
            /* submit_total_chunks */ 100,
            vec![],
        );
        let block = make_signed_header(
            /* height */ 1,
            prev.block_hash,
            /* cumulative_diff */ 1,
            /* submit_total_chunks */ 0,
            vec![],
        );
        let err = compute_submit_range(&block, Some(&prev))
            .expect_err("total=0 with prev_total=100 must error as data corruption");
        let msg = err.to_string();
        assert!(
            msg.contains("data corruption"),
            "unexpected error message: {msg}"
        );
    }

    #[test_log::test(tokio::test)]
    async fn pre_migration_tx_returns_range_via_block_tree() -> eyre::Result<()> {
        let (db, _tmp) = open_db()?;

        let tx_id = H256::random();
        // h0: 5 Submit chunks (no tx) - serves as genesis with empty tx_ids.
        let h0 = make_signed_header(0, H256::zero(), 0, 5, vec![]);
        let h1 = make_signed_header(1, h0.block_hash, 1, 12, vec![tx_id]);

        let guard = build_tree(h0, vec![h1]);
        // Deliberately do NOT write IrysDataTxMetadata or MigratedBlockHashes —
        // the helper must fall back to the block_tree walk.

        let range = find_canonical_ledger_range(
            &tx_id,
            /* max_height */ 1,
            ConsensusConfig::testing().block_migration_depth,
            &guard,
            &db,
        )?
        .ok_or_else(|| eyre::eyre!("expected Some(range) via block_tree fallback"))?;

        // [5, 11] inclusive.
        assert_eq!(range.start(), LedgerChunkOffset::from(5_u64));
        assert_eq!(range.end(), LedgerChunkOffset::from(11_u64));
        Ok(())
    }

    /// Regression test: the block-tree fallback must anchor its scan window
    /// to `max_height`, not the canonical-chain tip.  Build a canonical chain
    /// long enough that a tip-anchored window of `block_migration_depth`
    /// blocks contains *only* entries with `height > max_height`, then put
    /// the only `tx_id` reference at a height that is within the migration
    /// window relative to `max_height`.  A tip-anchored implementation
    /// returns `None`; the correct max-height-anchored implementation
    /// returns the canonical range.
    #[test_log::test(tokio::test)]
    async fn pre_migration_lookup_anchored_to_max_height_not_tip() -> eyre::Result<()> {
        let (db, _tmp) = open_db()?;

        let tx_id = H256::random();
        // 10-block canonical chain.  Tx sits at height 2 in the Submit ledger
        // (chunks [5, 11]).  Heights 3..=9 carry no Submit txs but advance
        // the chain past block_migration_depth = 6 from height 2.
        let h0 = make_signed_header(0, H256::zero(), 0, 5, vec![]);
        let h1 = make_signed_header(1, h0.block_hash, 1, 5, vec![]);
        let h2 = make_signed_header(2, h1.block_hash, 2, 12, vec![tx_id]);
        let h3 = make_signed_header(3, h2.block_hash, 3, 12, vec![]);
        let h4 = make_signed_header(4, h3.block_hash, 4, 12, vec![]);
        let h5 = make_signed_header(5, h4.block_hash, 5, 12, vec![]);
        let h6 = make_signed_header(6, h5.block_hash, 6, 12, vec![]);
        let h7 = make_signed_header(7, h6.block_hash, 7, 12, vec![]);
        let h8 = make_signed_header(8, h7.block_hash, 8, 12, vec![]);
        let h9 = make_signed_header(9, h8.block_hash, 9, 12, vec![]);
        let guard = build_tree(h0, vec![h1, h2, h3, h4, h5, h6, h7, h8, h9]);

        // Query a historical height (3) far below the tip (9).  Migration
        // window relative to max_height=3 with depth=6 covers heights -2..=3
        // → 0..=3, which includes h2 where the tx lives.  A tip-anchored
        // window (heights 4..=9) would not.
        let range = find_canonical_ledger_range(
            &tx_id,
            /* max_height */ 3,
            ConsensusConfig::testing().block_migration_depth,
            &guard,
            &db,
        )?
        .ok_or_else(|| {
            eyre::eyre!(
                "expected Some(range) at max_height=3 — scan must be anchored \
                 to max_height, not the canonical tip"
            )
        })?;

        // h2 added chunks 5..=11 (h1.total=5, h2.total=12).
        assert_eq!(range.start(), LedgerChunkOffset::from(5_u64));
        assert_eq!(range.end(), LedgerChunkOffset::from(11_u64));
        Ok(())
    }

}
