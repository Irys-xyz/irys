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

/// Compute canonical fork-choice anchors by combining the in-memory block tree with the
/// persisted block index.
///
/// The migration and prune anchors are selected from the block tree when sufficient depth is
/// available. If the tree cache is shallower than the requested depth, the block index is used to
/// determine the correct fallback block. Should the requested depth not exist in the index yet,
/// the genesis (height 0) entry is returned instead.
pub fn canonical_anchors(
    block_tree: &block_tree::BlockTree,
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
    migration_depth: usize,
    prune_depth: usize,
) -> Result<block_tree::CanonicalAnchors> {
    let (canonical_chain, _) = block_tree.get_canonical_chain();
    if canonical_chain.is_empty() {
        bail!("canonical chain is empty while computing anchors");
    }

    let head_entry = canonical_chain
        .last()
        .ok_or_else(|| eyre!("canonical chain missing head entry"))?
        .clone();
    let head_header = load_header(block_tree, database, head_entry.block_hash)?;
    let head_anchor = block_tree::AnchorBlock {
        entry: head_entry.clone(),
        header: head_header,
    };

    let head_height = head_entry.height;
    let index_safe_height = block_index.latest_height();
    let migration_depth_u64 = migration_depth as u64;
    let prune_depth_u64 = prune_depth as u64;
    let depth_delta = prune_depth_u64.saturating_sub(migration_depth_u64);

    let tree_safe_height = if canonical_chain.len() > migration_depth {
        canonical_chain[canonical_chain.len() - 1 - migration_depth].height
    } else {
        canonical_chain.first().unwrap().height
    };
    let mut migration_height = tree_safe_height.max(index_safe_height);
    if migration_height > head_height {
        migration_height = head_height;
    }
    let migration_block = anchor_for_height(
        migration_height,
        &canonical_chain,
        block_tree,
        block_index,
        database,
    )?;

    let index_final_height = index_safe_height.saturating_sub(depth_delta);
    let mut prune_height = migration_height.saturating_sub(depth_delta);
    if prune_height < index_final_height {
        prune_height = index_final_height;
    }
    if prune_height > migration_height {
        prune_height = migration_height;
    }
    let prune_block = anchor_for_height(
        prune_height,
        &canonical_chain,
        block_tree,
        block_index,
        database,
    )?;

    let migration_depth_reached =
        head_height.saturating_sub(migration_height) >= migration_depth_u64;
    let prune_depth_reached = migration_height.saturating_sub(prune_height) >= depth_delta;

    Ok(block_tree::CanonicalAnchors {
        head: head_anchor,
        migration_block,
        prune_block,
        migration_depth_reached,
        prune_depth_reached,
    })
}

fn anchor_for_height(
    height: u64,
    canonical_chain: &[block_tree::BlockTreeEntry],
    block_tree: &block_tree::BlockTree,
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
) -> Result<block_tree::AnchorBlock> {
    if let Some(entry) = canonical_chain.iter().find(|entry| entry.height == height) {
        let header = load_header(block_tree, database, entry.block_hash)?;
        return Ok(block_tree::AnchorBlock {
            entry: entry.clone(),
            header,
        });
    }

    let index_item = block_index
        .get_item(height)
        .ok_or_else(|| eyre!("missing block index entry at height {height}"))?;
    let header = load_header_from_db(database, index_item.block_hash)?;
    let entry = block_tree::make_block_tree_entry(header.as_ref());

    Ok(block_tree::AnchorBlock { entry, header })
}

pub fn canonical_anchors_from_index(
    block_index: &block_index::BlockIndex,
    database: &DatabaseProvider,
    migration_depth: usize,
    prune_depth: usize,
) -> Result<block_tree::CanonicalAnchors> {
    if block_index.num_blocks() == 0 {
        bail!("block index is empty while computing canonical anchors");
    }

    let latest_height = block_index.latest_height();
    let migration_depth_u64 = migration_depth as u64;
    let prune_depth_u64 = prune_depth as u64;
    let chain_len = latest_height.saturating_add(1);

    let head_height = latest_height;

    let head_item = block_index
        .get_item(head_height)
        .ok_or_else(|| eyre!("missing block index entry at height {head_height}"))?;
    let head_header = load_header_from_db(database, head_item.block_hash)?;
    let head_entry = block_tree::make_block_tree_entry(head_header.as_ref());
    let head_anchor = block_tree::AnchorBlock {
        entry: head_entry.clone(),
        header: head_header.clone(),
    };

    let migration_depth_reached = chain_len > migration_depth_u64;

    let depth_delta = prune_depth_u64.saturating_sub(migration_depth_u64);
    let finalized_height = head_height.saturating_sub(depth_delta);

    let finalize_item = block_index
        .get_item(finalized_height)
        .ok_or_else(|| eyre!("missing block index entry at height {finalized_height}"))?;
    let finalize_header = if finalized_height == head_height {
        head_header
    } else {
        load_header_from_db(database, finalize_item.block_hash)?
    };
    let finalize_entry = if finalized_height == head_height {
        head_entry
    } else {
        block_tree::make_block_tree_entry(finalize_header.as_ref())
    };

    let prune_depth_reached = chain_len > prune_depth_u64;

    Ok(block_tree::CanonicalAnchors {
        head: head_anchor.clone(),
        migration_block: head_anchor,
        prune_block: block_tree::AnchorBlock {
            entry: finalize_entry,
            header: finalize_header,
        },
        migration_depth_reached,
        prune_depth_reached,
    })
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
