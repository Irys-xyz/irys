use crate::mempool_service::TxIngressError;
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::{BlockTreeEntry, BlockTreeReadGuard};
use irys_types::ingress::IngressProof;
use irys_types::{ConsensusConfig, H256, IrysTransactionCommon, app_state::DatabaseProvider};
use tracing::{debug, warn};

/// Resolves an anchor (block hash) to its height.
/// If the anchor is not found, returns `Ok(None)`.
///
/// When `canonical=true`: checks the block tree's canonical chain first, then falls back
/// to the database with a `MigratedBlockHashes` cross-check to ensure the block is
/// actually canonical (not an orphan from a resolved fork).
///
/// When `canonical=false`: checks the block tree for any known block, then falls back to
/// `IrysBlockHeaders` without canonical verification (used by mempool expiry).
#[tracing::instrument(level = "trace", skip_all, fields(anchor = %anchor, canonical = canonical))]
pub fn get_anchor_height(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    anchor: H256,
    canonical: bool,
) -> eyre::Result<Option<u64>> {
    // Fast path: check the in-memory block tree first.
    if let Some(height) = {
        let guard = block_tree.read();
        if canonical {
            guard
                .get_canonical_chain()
                .0
                .iter()
                .find(|b| b.block_hash() == anchor)
                .map(BlockTreeEntry::height)
        } else {
            guard.get_block(&anchor).map(|h| h.height)
        }
    } {
        return Ok(Some(height));
    }

    // Slow path: consult the database for blocks pruned from the tree.
    if canonical {
        // Cross-check IrysBlockHeaders against MigratedBlockHashes in one
        // read transaction to ensure the block is actually canonical.
        db.view_eyre(|tx| irys_database::canonical_block_height_by_hash(tx, &anchor))
    } else {
        // Non-canonical lookup (e.g. mempool expiry): accept any known block.
        let hdr = db.view_eyre(|tx| irys_database::block_header_by_hash(tx, &anchor, false))?;
        Ok(hdr.map(|h| h.height))
    }
}

/// Validates that a transaction's anchor falls within `[min_anchor_height, max_anchor_height]`.
/// Returns `Ok(true)` if valid, `Ok(false)` if out of range.
#[tracing::instrument(level = "trace", skip_all)]
pub fn validate_anchor_for_inclusion(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    min_anchor_height: u64,
    max_anchor_height: u64,
    tx: &impl IrysTransactionCommon,
) -> eyre::Result<bool> {
    let tx_id = tx.id();
    let anchor = tx.anchor();
    // transaction anchors must be canonical for inclusion
    let anchor_height = match get_anchor_height(block_tree, db, anchor, true).map_err(|e| {
        TxIngressError::DatabaseError(format!("Error getting anchor height for {}: {}", anchor, e))
    })? {
        Some(height) => height,
        None => {
            return Err(TxIngressError::InvalidAnchor(anchor).into());
        }
    };

    // these have to be inclusive so we handle txs near height 0 correctly
    let new_enough = anchor_height >= min_anchor_height;
    let old_enough = anchor_height <= max_anchor_height;
    if old_enough && new_enough {
        Ok(true)
    } else if !old_enough {
        warn!(
            "Tx {tx_id} anchor {anchor} has height {anchor_height}, which is too new compared to max height {max_anchor_height}"
        );
        Ok(false)
    } else if !new_enough {
        warn!(
            "Tx {tx_id} anchor {anchor} has height {anchor_height}, which is too old compared to min height {min_anchor_height}"
        );
        Ok(false)
    } else {
        unreachable!(
            "exhaustive boolean check: tx_id={tx_id} anchor={anchor} height={anchor_height} min={min_anchor_height} max={max_anchor_height}"
        );
    }
}

/// Validates that an ingress proof's anchor is recent enough (`>= min_anchor_height`).
/// Returns `Ok(true)` if valid, `Ok(false)` if the anchor is too old.
#[tracing::instrument(skip_all)]
pub fn validate_ingress_proof_anchor_for_inclusion(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    min_anchor_height: u64,
    ingress_proof: &IngressProof,
) -> eyre::Result<bool> {
    let anchor = ingress_proof.anchor;
    let anchor_height = match get_anchor_height(block_tree, db, anchor, true).map_err(|e| {
        TxIngressError::DatabaseError(format!("Error getting anchor height for {}: {}", anchor, e))
    })? {
        Some(height) => height,
        None => {
            return Ok(false);
        }
    };

    // these have to be inclusive so we handle txs near height 0 correctly
    let new_enough = anchor_height >= min_anchor_height;
    debug!(
        "ingress proof ID: {} anchor_height: {anchor_height} min_anchor_height: {min_anchor_height}",
        &ingress_proof.id()
    );
    // note: we don't need old_enough as we're part of the block header
    // so there's no need to go through the mempool
    // let old_enough: bool = anchor_height <= max_anchor_height;
    if new_enough {
        Ok(true)
    } else {
        let signer = ingress_proof
            .recover_signer()
            .map(|addr| format!("{addr}"))
            .unwrap_or_else(|_| "unknown".to_string());
        warn!(
            signer,
            "ingress proof data_root {} anchor {anchor} has height {anchor_height}, which is too old compared to min height {min_anchor_height}",
            &ingress_proof.data_root,
        );
        Ok(false)
    }
}

/// Computes the valid anchor height range for transaction inclusion during block production.
///
/// Returns `(min_anchor_height, max_anchor_height)` where:
/// - `max` = `height - block_migration_depth` (blocks must be finalized)
/// - `min` = `height - (tx_anchor_expiry_depth - block_migration_depth)` (anchor freshness)
pub fn tx_inclusion_anchor_range(consensus: &ConsensusConfig, height: u64) -> (u64, u64) {
    let migration = u64::from(consensus.block_migration_depth);
    let expiry = u64::from(consensus.mempool.tx_anchor_expiry_depth);
    let min = height.saturating_sub(expiry.saturating_sub(migration));
    let max = height.saturating_sub(migration);
    (min, max)
}

/// Computes the minimum block height for a transaction anchor to be considered non-expired.
pub fn min_tx_anchor_height(consensus: &ConsensusConfig, height: u64) -> u64 {
    height.saturating_sub(u64::from(consensus.mempool.tx_anchor_expiry_depth))
}

/// Computes the minimum block height for an ingress proof anchor to be considered non-expired.
pub fn min_ingress_proof_anchor_height(consensus: &ConsensusConfig, height: u64) -> u64 {
    height.saturating_sub(u64::from(
        consensus.mempool.ingress_proof_anchor_expiry_depth,
    ))
}

#[cfg(test)]
#[path = "anchor_validation_tests.rs"]
mod tests;
