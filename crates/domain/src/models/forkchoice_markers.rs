use eyre::Result;
use eyre::bail;
use eyre::eyre;

use irys_database::database;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::BlockHash;
use irys_types::DatabaseProvider;

use irys_types::IrysBlockHeader;

use std::sync::Arc;

use super::block_index;
use super::block_tree;

/// ForkChoiceMarkers captures the head plus safe/finalized anchor blocks used for fork choice.
/// `head` tracks the current canonical tip broadcast to downstream services.
/// `migration_block` marks the migration depth to block index.
/// `prune_block` marks the prune depth of the block tree.
#[derive(Debug, Clone)]
pub struct ForkChoiceMarkers {
    pub head: Arc<IrysBlockHeader>,
    pub migration_block: Arc<IrysBlockHeader>,
    pub prune_block: Arc<IrysBlockHeader>,
}

fn compute_depth_delta(prune_depth: usize, migration_depth: usize) -> Result<u64> {
    u64::try_from(prune_depth.saturating_sub(migration_depth))
        .map_err(|e| eyre!("depth_delta overflow: {e}"))
}

impl ForkChoiceMarkers {
    /// Computes canonical fork-choice markers from the live block tree state.
    ///
    /// - `head` is the latest validated canonical tip from the block tree and only advances when the
    ///   canonical head changes.
    /// - `migration_block` (confirmed) is the block scheduled for migration into the block index at
    ///   the configured `migration_depth` behind the head.
    /// - `prune_block` (finalized) is the block due to be pruned from the block tree once migration
    ///   completes, `block_tree_depth` behind the head.
    ///
    /// If the in-memory cache is shallower than the requested depth (common immediately after
    /// startup), the method falls back to the persisted block index for historical headers.
    pub fn from_block_tree(
        block_tree: &block_tree::BlockTree,
        block_index: &block_index::BlockIndex,
        database: &DatabaseProvider,
        migration_depth: usize,
        prune_depth: usize,
    ) -> Result<Self> {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        if canonical_chain.is_empty() {
            bail!("canonical chain is empty while computing fork-choice markers");
        }

        let head_height = tree_head_height(&canonical_chain)?;
        let tree_safe_height = tree_safe_height(&canonical_chain, migration_depth)?;
        let index_safe_height = block_index.latest_height();
        let migration_height =
            compute_migration_height(head_height, tree_safe_height, index_safe_height);
        let depth_delta = compute_depth_delta(prune_depth, migration_depth)?;
        let prune_height = compute_prune_height(migration_height, index_safe_height, depth_delta);

        let head_block = block_at_height(
            head_height,
            &canonical_chain,
            block_tree,
            block_index,
            database,
        )?;

        let migration_block = block_at_height(
            migration_height,
            &canonical_chain,
            block_tree,
            block_index,
            database,
        )?;

        let prune_block = block_at_height(
            prune_height,
            &canonical_chain,
            block_tree,
            block_index,
            database,
        )?;

        Ok(Self {
            head: head_block,
            migration_block,
            prune_block,
        })
    }

    /// Computes canonical fork-choice markers using only the block index—mirroring the values that
    /// would have been in effect before shutdown (aside from the head, which is rolled back to the
    /// latest indexed block).
    ///
    /// During startup the block tree is empty, so:
    /// - `head` resolves to the latest block index entry — the confirmed/migrated frontier, which
    ///   sits `migration_depth` below the pre-shutdown canonical head (the head rolls back to it).
    /// - `migration_block` mirrors that same entry (the “confirmed” head just before shutdown).
    /// - `prune_block` is `finalized_height(...)` behind that frontier — `block_tree_depth −
    ///   migration_depth` below the tip, equal to `block_tree_depth` below the pre-shutdown head —
    ///   so the finalized marker aligns with the state before shutdown.
    pub fn from_index(
        block_index: &block_index::BlockIndex,
        database: &DatabaseProvider,
        migration_depth: usize,
        prune_depth: usize,
    ) -> Result<Self> {
        if block_index.num_blocks() == 0 {
            bail!("block index is empty while computing fork-choice markers");
        }

        let head_height = block_index.latest_height();
        let migration_height = head_height;
        // At restart the head rolls back to the confirmed (migrated) frontier, so
        // `head_height` here IS the confirmed frontier — the canonical anchor for
        // finalization. This keeps the finalized we re-announce to reth aligned
        // with the value in effect before shutdown (no recession).
        let prune_height =
            finalized_height(head_height, prune_depth as u64, migration_depth as u64);

        let head_block = marker_from_index_height(block_index, database, head_height)?;
        let migration_block = marker_from_index_height(block_index, database, migration_height)?;
        let prune_block = marker_from_index_height(block_index, database, prune_height)?;

        Ok(Self {
            head: head_block,
            migration_block,
            prune_block,
        })
    }
}

pub(crate) fn block_at_height(
    height: u64,
    canonical_chain: &[block_tree::BlockTreeEntry],
    block_tree: &block_tree::BlockTree,
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
) -> Result<Arc<IrysBlockHeader>> {
    if let Some(entry) = canonical_chain
        .iter()
        .find(|entry| entry.height() == height)
    {
        let header = load_header(block_tree, database, entry.block_hash())?;
        return Ok(header);
    }

    marker_from_index_height(block_index, database, height)
}

pub(crate) fn marker_from_index_height(
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
    height: u64,
) -> Result<Arc<IrysBlockHeader>> {
    let index_item = block_index
        .get_item(height)
        .ok_or_else(|| eyre!("missing block index entry at height {height}"))?;
    let header = load_header_from_db(database, index_item.block_hash)?;
    Ok(header)
}

pub(crate) fn load_header(
    block_tree: &block_tree::BlockTree,
    database: &DatabaseProvider,
    hash: BlockHash,
) -> Result<Arc<IrysBlockHeader>> {
    if let Some(header) = block_tree.get_block(&hash) {
        return Ok(Arc::new(header.clone()));
    }

    load_header_from_db(database, hash)
}

pub(crate) fn load_header_from_db(
    database: &DatabaseProvider,
    hash: BlockHash,
) -> Result<Arc<IrysBlockHeader>> {
    let header = database
        .view_eyre(|tx| database::block_header_by_hash(tx, &hash, false))?
        .ok_or_else(|| eyre!("block {hash} not found in database while loading anchor header"))?;

    Ok(Arc::new(header))
}

pub(crate) fn tree_head_height(canonical_chain: &[block_tree::BlockTreeEntry]) -> Result<u64> {
    canonical_chain
        .last()
        .map(super::block_tree::BlockTreeEntry::height)
        .ok_or_else(|| eyre!("canonical chain missing head entry"))
}

pub(crate) fn tree_safe_height(
    canonical_chain: &[block_tree::BlockTreeEntry],
    migration_depth: usize,
) -> Result<u64> {
    if canonical_chain.len() > migration_depth {
        Ok(canonical_chain[canonical_chain.len() - 1 - migration_depth].height())
    } else {
        canonical_chain
            .first()
            .map(super::block_tree::BlockTreeEntry::height)
            .ok_or_else(|| eyre!("canonical chain missing genesis entry"))
    }
}

pub(crate) fn compute_migration_height(
    head_height: u64,
    tree_safe_height: u64,
    index_safe_height: u64,
) -> u64 {
    tree_safe_height.max(index_safe_height).min(head_height)
}

pub(crate) fn compute_prune_height(
    migration_height: u64,
    index_safe_height: u64,
    depth_delta: u64,
) -> u64 {
    let index_final_height = index_safe_height.saturating_sub(depth_delta);
    let desired_prune = migration_height.saturating_sub(depth_delta);
    desired_prune.max(index_final_height).min(migration_height)
}

/// The finalized height: the block that has left (or is leaving) the block tree
/// and is therefore irreversible. Finalization depth is canonically equal to
/// `block_tree_depth` — the moment a block falls `block_tree_depth` behind the
/// head it is pruned from the tree, and no admissible reorg (whose LCA must live
/// in the tree) can reach it.
///
/// Anchored to the **confirmed frontier** (the migrated block-index tip), not the
/// live head: the confirmed frontier persists across a restart, whereas the head
/// rolls back to it. Because the confirmed frontier sits `migration_depth` behind
/// the head, `confirmed − (block_tree_depth − migration_depth)` equals
/// `head − block_tree_depth` in steady state while remaining monotonic across a
/// restart's head-rollback. This is the single definition every finalized/prune
/// computation (fork-choice markers, block-tree restore floor, partition-recovery
/// guard) must agree on.
pub fn finalized_height(confirmed_height: u64, block_tree_depth: u64, migration_depth: u64) -> u64 {
    let depth_delta = block_tree_depth.saturating_sub(migration_depth);
    confirmed_height.saturating_sub(depth_delta)
}

/// The block-tree cache floor to restore after a restart: the lowest height the
/// rebuilt tree should keep, so the reorg window never straddles a finalized block.
///
/// On restart the head rolls back to the confirmed (migrated) index tip, so we
/// reconstruct the pre-crash head (`confirmed_tip + migration_depth`) and apply the
/// same rule a running node's `prune` follows (`head - (block_tree_depth - 1)`).
/// This equals `finalized_height(..) + 1` once the chain is `block_tree_depth`
/// deep, and saturates to 0 (keeping genesis) on a young chain.
pub fn restore_cache_floor(confirmed_tip: u64, block_tree_depth: u64, migration_depth: u64) -> u64 {
    let precrash_head = confirmed_tip.saturating_add(migration_depth);
    precrash_head.saturating_sub(block_tree_depth.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn migration_height_never_exceeds_head(
            head_height: u64,
            tree_safe_height: u64,
            index_safe_height: u64,
        ) {
            let result = compute_migration_height(head_height, tree_safe_height, index_safe_height);
            prop_assert!(result <= head_height);
        }

        #[test]
        fn migration_height_equals_clamped_max_safe(
            head_height: u64,
            tree_safe_height: u64,
            index_safe_height: u64,
        ) {
            let result = compute_migration_height(head_height, tree_safe_height, index_safe_height);
            let max_safe = tree_safe_height.max(index_safe_height);
            prop_assert_eq!(result, max_safe.min(head_height));
        }

        #[test]
        fn prune_height_never_exceeds_migration(
            migration_height: u64,
            index_safe_height: u64,
            depth_delta: u64,
        ) {
            let result = compute_prune_height(migration_height, index_safe_height, depth_delta);
            prop_assert!(result <= migration_height);
        }

        #[test]
        fn prune_height_zero_delta_equals_migration(
            migration_height: u64,
            index_safe_height: u64,
        ) {
            let result = compute_prune_height(migration_height, index_safe_height, 0);
            prop_assert_eq!(result, migration_height);
        }
    }

    /// The canonical finalized height is the confirmed (migrated) frontier minus
    /// `depth_delta` (= `block_tree_depth − migration_depth`). Worked from the
    /// confirmed counterexample: confirmed tip 94, block_tree_depth 18,
    /// migration_depth 6 → finalized 82.
    #[test]
    fn finalized_height_is_confirmed_minus_depth_delta() {
        assert_eq!(finalized_height(94, 18, 6), 82);
    }

    /// Anchored to the confirmed frontier, the helper equals `head − block_tree_depth`
    /// in steady state, because the confirmed frontier sits `migration_depth` behind
    /// the head. head 100, migration_depth 6 → confirmed 94 → finalized 82 = 100 − 18.
    #[test]
    fn finalized_height_equals_head_minus_block_tree_depth_in_steady_state() {
        let (head, block_tree_depth, migration_depth) = (100_u64, 18_u64, 6_u64);
        let confirmed = head - migration_depth;
        assert_eq!(
            finalized_height(confirmed, block_tree_depth, migration_depth),
            head - block_tree_depth,
        );
    }

    /// Before the chain is `block_tree_depth` deep nothing has left the tree, so
    /// finalized saturates at genesis rather than underflowing.
    #[test]
    fn finalized_height_saturates_to_genesis_on_shallow_chain() {
        assert_eq!(finalized_height(5, 18, 6), 0);
    }

    /// Deep chain: the reconstructed pre-crash head is `confirmed_tip + migration_depth`
    /// = 100, so the floor is `100 - (18 - 1)` = 83, matching `finalized_height(94, 18, 6) + 1`.
    #[test]
    fn restore_cache_floor_matches_finalized_height_plus_one_on_deep_chain() {
        assert_eq!(restore_cache_floor(94, 18, 6), 83);
        assert_eq!(
            restore_cache_floor(94, 18, 6),
            finalized_height(94, 18, 6) + 1
        );
    }

    /// Boundary: pre-crash head (confirmed_tip + migration_depth = 18) exactly equals
    /// block_tree_depth, so the floor lands at height 1.
    #[test]
    fn restore_cache_floor_at_exact_boundary() {
        assert_eq!(restore_cache_floor(12, 18, 6), 1);
    }

    /// Young chain: pre-crash head (9 + 6 = 15) is still less than block_tree_depth (18),
    /// so the floor saturates to 0, keeping genesis in the tree.
    #[test]
    fn restore_cache_floor_saturates_to_genesis_on_young_chain() {
        assert_eq!(restore_cache_floor(9, 18, 6), 0);
    }
}
