pub mod block_index;
pub mod block_tree;
pub mod chain_sync_state;
pub mod chunk_provider;
pub mod circular_buffer;
pub mod execution_payload_cache;
pub mod peer_list;
pub mod reth_provider;
pub mod storage_module;

pub use block_index::*;
pub use block_tree::*;
pub use chunk_provider::*;
pub use circular_buffer::*;
pub use execution_payload_cache::*;
pub use peer_list::*;
pub use reth_provider::*;
pub use storage_module::*;

use std::sync::Arc;

use eyre::{bail, eyre, Result};
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader};

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
/// Computes canonical fork-choice anchors from the live block tree state
///
/// - `head` is the latest validated canonical tip from the block tree and only advances when the
///   canonical head changes.
/// - `migration_block` (confirmed) is the block scheduled for migration into the block index at the
///   configured `migration_depth` behind the head.
/// - `prune_block` (finalized) is the block due to be pruned from the block tree once migration
///   completes, `block_tree_depth` behind the head.
///
/// If the in-memory cache is shallower than the requested depth (happens right after the node starts up),
/// the function falls back to the persisted block index for historical headers.
pub fn fork_choice_markers(
    block_tree: &block_tree::BlockTree,
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
    migration_depth: usize,
    prune_depth: usize,
) -> Result<ForkChoiceMarkers> {
    let (canonical_chain, _) = block_tree.get_canonical_chain();
    if canonical_chain.is_empty() {
        bail!("canonical chain is empty while computing anchors");
    }

    let head_height = tree_head_height(&canonical_chain)?;
    let tree_safe_height = tree_safe_height(&canonical_chain, migration_depth)?;
    let index_safe_height = block_index.latest_height();
    let migration_height =
        compute_migration_height(head_height, tree_safe_height, index_safe_height);
    let depth_delta = prune_depth.saturating_sub(migration_depth) as u64;
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

    Ok(ForkChoiceMarkers {
        head: head_block,
        migration_block,
        prune_block,
    })
}

fn block_at_height(
    height: u64,
    canonical_chain: &[block_tree::BlockTreeEntry],
    block_tree: &block_tree::BlockTree,
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
) -> Result<Arc<IrysBlockHeader>> {
    if let Some(entry) = canonical_chain.iter().find(|entry| entry.height == height) {
        let header = load_header(block_tree, database, entry.block_hash)?;
        return Ok(header);
    }

    marker_from_index_height(block_index, database, height)
}

/// Computes canonical fork-choice anchors using only the block index — mirroring the values that
/// would have been in effect before shutdown (except for the `head`, which gets rolled back).
///
/// During startup the block tree is empty, so:
/// - `head` resolves to the latest block index entry (the prior canonical head).
/// - `migration_block` mirrors that same entry to match the “confirmed” head just before shutdown.
/// - `prune_block` is derived from the index at `block_tree_depth` behind the tip so the finalized
///   marker aligns with the state before shutdown.
pub fn fork_choice_markers_from_index(
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
    migration_depth: usize,
    prune_depth: usize,
) -> Result<ForkChoiceMarkers> {
    if block_index.num_blocks() == 0 {
        bail!("block index is empty while computing canonical anchors");
    }

    let head_height = block_index.latest_height();
    let migration_height = head_height;
    let depth_delta = prune_depth.saturating_sub(migration_depth) as u64;
    let prune_height = head_height.saturating_sub(depth_delta);

    let head_block = marker_from_index_height(block_index, database, head_height)?;
    let migration_block = marker_from_index_height(block_index, database, migration_height)?;
    let prune_block = marker_from_index_height(block_index, database, prune_height)?;

    Ok(ForkChoiceMarkers {
        head: head_block,
        migration_block,
        prune_block,
    })
}

fn marker_from_index_height(
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

fn load_header(
    block_tree: &block_tree::BlockTree,
    database: &DatabaseProvider,
    hash: BlockHash,
) -> Result<Arc<IrysBlockHeader>> {
    if let Some(header) = block_tree.get_block(&hash) {
        return Ok(Arc::new(header.clone()));
    }

    load_header_from_db(database, hash)
}

fn load_header_from_db(
    database: &DatabaseProvider,
    hash: BlockHash,
) -> Result<Arc<IrysBlockHeader>> {
    let header = database
        .view_eyre(|tx| database::block_header_by_hash(tx, &hash, false))?
        .ok_or_else(|| eyre!("block {hash} not found in database while loading anchor header"))?;

    Ok(Arc::new(header))
}

fn tree_head_height(canonical_chain: &[block_tree::BlockTreeEntry]) -> Result<u64> {
    canonical_chain
        .last()
        .map(|entry| entry.height)
        .ok_or_else(|| eyre!("canonical chain missing head entry"))
}

fn tree_safe_height(
    canonical_chain: &[block_tree::BlockTreeEntry],
    migration_depth: usize,
) -> Result<u64> {
    if canonical_chain.len() > migration_depth {
        Ok(canonical_chain[canonical_chain.len() - 1 - migration_depth].height)
    } else {
        canonical_chain
            .first()
            .map(|entry| entry.height)
            .ok_or_else(|| eyre!("canonical chain missing genesis entry"))
    }
}

fn compute_migration_height(
    head_height: u64,
    tree_safe_height: u64,
    index_safe_height: u64,
) -> u64 {
    tree_safe_height.max(index_safe_height).min(head_height)
}

fn compute_prune_height(migration_height: u64, index_safe_height: u64, depth_delta: u64) -> u64 {
    let index_final_height = index_safe_height.saturating_sub(depth_delta);
    let desired_prune = migration_height.saturating_sub(depth_delta);
    desired_prune.max(index_final_height).min(migration_height)
}
