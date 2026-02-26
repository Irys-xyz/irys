use crate::mempool_service::TxIngressError;
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::{BlockTreeEntry, BlockTreeReadGuard};
use irys_types::ingress::IngressProof;
use irys_types::{H256, IrysTransactionCommon, app_state::DatabaseProvider};
use tracing::{debug, warn};

/// Resolves an anchor (block hash) to its height.
/// If the anchor is not found, returns `Ok(None)`.
/// Set `canonical` to `true` to enforce that the anchor must be on the current canonical chain.
// TODO(correctness): DB fallback does not verify canonical status — orphan-fork anchors
// could be accepted when canonical=true. The DB fallback should either return None when
// canonical is required, or verify the block is on the canonical chain.
#[tracing::instrument(level = "trace", skip_all, fields(anchor = %anchor, canonical = canonical))]
pub fn get_anchor_height(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    anchor: H256,
    canonical: bool,
) -> eyre::Result<Option<u64>> {
    // check the block tree, then DB
    if let Some(height) = {
        // in a block so rust doesn't complain about it being held across an await point
        // I suspect if let Some desugars to something that lint doesn't like
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
        Ok(Some(height))
    } else if let Some(hdr) =
        db.view_eyre(|tx| irys_database::block_header_by_hash(tx, &anchor, false))?
    {
        Ok(Some(hdr.height))
    } else {
        Ok(None)
    }
}

/// Returns the height of the latest block on the canonical chain.
pub fn get_latest_block_height(block_tree: &BlockTreeReadGuard) -> Result<u64, TxIngressError> {
    // TODO: `get_canonical_chain` clones the entire canonical chain, we can make do with a ref here
    let canon_chain = block_tree.read().get_canonical_chain();
    let latest = canon_chain.0.last().ok_or(TxIngressError::Other(
        "unable to get canonical chain from block tree".to_owned(),
    ))?;

    Ok(latest.height())
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
    // ingress proof anchors must be canonical for inclusion
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
        eyre::bail!(
            "SHOULDNT HAPPEN: {tx_id} anchor {anchor} has height {anchor_height}, min: {min_anchor_height}, max: {max_anchor_height}"
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
            // Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
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
        // TODO: recover the signer's address here? (or compute an ID)
        warn!(
            "ingress proof data_root {} signature {:?} anchor {anchor} has height {anchor_height}, which is too old compared to min height {min_anchor_height}",
            &ingress_proof.data_root, &ingress_proof.signature
        );
        Ok(false)
    }
}
