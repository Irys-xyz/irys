//! Shared promotion readiness logic for data transactions.
//! Allows both mempool service and cache service to evaluate whether a data tx
//! is ready (or close) to promotion based on ingress proofs and prior submit inclusion.

use crate::mempool_service::{Inner, PromotionStatus};
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{ingress_proofs_by_data_root, tx_header_by_txid};
use irys_domain::BlockTreeReadGuard;
use irys_types::{ingress::IngressProof, Config, DataTransactionHeader, DatabaseProvider, H256};

/// Computes promotion status for a single data transaction header.
/// Returns (status, optionally filtered proofs ready for inclusion).
pub fn compute_promotion_status(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    tx_header: &DataTransactionHeader,
    submit_tx_slice: &[DataTransactionHeader],
    submit_txs_from_canonical: &std::collections::HashSet<H256>,
    min_ingress_proof_anchor_height: u64,
) -> eyre::Result<(PromotionStatus, Option<Vec<IngressProof>>)> {
    // 1. Already promoted
    if tx_header.promoted_height.is_some() {
        return Ok((PromotionStatus::AlreadyPromoted, None));
    }

    // 2. Prior submit inclusion (canonical or single-block promotion or historical inclusion)
    if !submit_txs_from_canonical.contains(&tx_header.id) {
        let single_block = submit_tx_slice.iter().any(|data_tx_header| data_tx_header.id == tx_header.id);
        if !single_block {
            let previously_included = db
                .view_eyre(|tx| tx_header_by_txid(tx, &tx_header.id))?
                .is_some();
            if !previously_included {
                return Ok((PromotionStatus::MissingSubmitInclusion, None));
            }
        }
    }

    // 3. Collect proofs & filter out expired anchors
    let all_proofs = db
        .view_eyre(|read_tx| ingress_proofs_by_data_root(read_tx, tx_header.data_root))?
        .into_iter()
        .filter_map(|(_root, cached)| {
            match Inner::validate_ingress_proof_anchor_static(
                block_tree_guard,
                db,
                config,
                &cached.proof,
            ) {
                Ok(()) => Some(cached.proof.clone()),
                Err(_) => None,
            }
        })
        .collect::<Vec<_>>();

    let total_miners = block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .commitment_state
        .stake_commitments
        .len();
    let proofs_per_tx = std::cmp::min(
        config.consensus.number_of_ingress_proofs_total as usize,
        total_miners,
    );
    if all_proofs.len() < proofs_per_tx {
        return Ok((PromotionStatus::InsufficientProofs, None));
    }

    // 4. Anchor freshness filter using height threshold
    let mut fresh: Vec<IngressProof> = Vec::with_capacity(all_proofs.len());
    for proof in all_proofs {
        // Reuse mempool anchor inclusion check
        let anchor_is_valid =
            match Inner::validate_ingress_proof_anchor_static(block_tree_guard, db, config, &proof)
            {
                Ok(()) => {
                    // Height based pruning for inclusion
                    let maybe_canonical_anchor_height =
                        Inner::get_anchor_height_static(block_tree_guard, db, proof.anchor, true)?;
                    if let Some(canonical_anchor_height) = maybe_canonical_anchor_height {
                        canonical_anchor_height >= min_ingress_proof_anchor_height
                    } else {
                        false
                    }
                }
                Err(_) => false,
            };
        if anchor_is_valid {
            fresh.push(proof);
        }
    }
    if fresh.len() < proofs_per_tx {
        return Ok((PromotionStatus::InsufficientProofs, None));
    }

    // Assigned proof logic requires async context (block header fetch). Omitted for cache service.
    // We treat sufficient total fresh proofs as Ready to avoid duplication.
    Ok((PromotionStatus::Ready, Some(fresh)))
}
