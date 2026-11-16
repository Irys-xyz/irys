use crate::anchor_validation::validate_ingress_proof_anchor_for_inclusion;
use crate::chunk_ingress_service::ChunkIngressServiceInner;
use irys_database::DatabaseProvider;
use irys_database::tables::CompactCachedIngressProof;
use irys_domain::BlockTreeReadGuard;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::ingress::CachedIngressProof;
use irys_types::{Config, IrysAddress, IrysTransactionCommon, U256};
use reth::rpc::types::BlockId;
use std::collections::{HashMap, HashSet};
use tracing::{debug, warn};

pub(crate) async fn fetch_balances_for_transactions<T: IrysTransactionCommon>(
    reth_adapter: &IrysRethNodeAdapter,
    block_id: Option<BlockId>,
    txs: &[T],
) -> eyre::Result<HashMap<IrysAddress, U256>> {
    let signers: Vec<IrysAddress> = txs
        .iter()
        .map(irys_types::IrysTransactionCommon::signer)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    reth_adapter.get_balances_irys(&signers, block_id).await
}

// Helper function that verifies transaction funding and tracks cumulative fees
// Returns true if the transaction can be funded based on current account balance
// and previously included transactions in this block
pub(crate) fn check_funding<T: IrysTransactionCommon>(
    tx: &T,
    balances: &HashMap<IrysAddress, U256>,
    unfunded_address: &mut HashSet<IrysAddress>,
    fees_spent_per_address: &mut HashMap<IrysAddress, U256>,
) -> bool {
    let signer = tx.signer();

    // Skip transactions from addresses with previously unfunded transactions
    // This ensures we don't include any transactions (including pledges) from
    // addresses that couldn't afford their stake commitments
    if unfunded_address.contains(&signer) {
        return false;
    }

    let fee = tx.total_cost();
    let current_spent = *fees_spent_per_address.get(&signer).unwrap_or(&U256::zero());

    // Calculate total required balance including previously selected transactions

    // get balance state for the block we're building off of
    let balance = balances.get(&signer).copied().unwrap_or_else(U256::zero);

    let total_required = match current_spent.checked_add(fee) {
        Some(total) => total,
        None => {
            warn!(
                tx.signer = ?signer,
                tx.fee = ?fee,
                cumulative_spent = ?current_spent,
                "Cumulative fee overflow for signer — skipping transaction"
            );
            unfunded_address.insert(signer);
            return false;
        }
    };

    let has_funds = balance >= total_required;

    // Track fees for this address regardless of whether this specific transaction is included
    fees_spent_per_address
        .entry(signer)
        .and_modify(|val| *val = val.checked_add(fee).unwrap_or(U256::MAX))
        .or_insert(fee);

    // If transaction cannot be funded, mark the entire address as unfunded
    // Since stakes are processed before pledges, this prevents inclusion of
    // pledge commitments when their associated stake commitment is unfunded
    if !has_funds {
        debug!(
        tx.signer = ?signer,
        account.balance = ?balance,
        "Transaction funding check failed"
        );
        unfunded_address.insert(signer);
        return false;
    }

    has_funds
}

/// Lowest anchor height that may be included in a block at `next_block_height`.
pub(crate) fn min_ingress_proof_anchor_height(next_block_height: u64, config: &Config) -> u64 {
    next_block_height
        .saturating_sub(config.consensus.mempool.ingress_proof_anchor_expiry_depth as u64)
}

pub(super) fn same_chain_ingress_proofs(
    proofs: impl IntoIterator<Item = CompactCachedIngressProof>,
    chain_id: u64,
) -> Vec<CompactCachedIngressProof> {
    proofs
        .into_iter()
        .filter(|cached| cached.proof.chain_id == chain_id)
        .collect()
}

/// Proofs whose anchor has not expired.
///
/// A database error inside the expiry check counts as not expired. That is what
/// the promotion selector does.
pub(crate) fn unexpired_ingress_proofs(
    proofs: impl IntoIterator<Item = CompactCachedIngressProof>,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
) -> Vec<CachedIngressProof> {
    proofs
        .into_iter()
        .filter(|cached| {
            !ChunkIngressServiceInner::is_ingress_proof_expired_static(
                block_tree,
                db,
                config,
                &cached.proof,
            )
            .expired_or_invalid
        })
        .map(|cached| cached.0)
        .collect()
}

/// Keep the first proof for each stored signer. Later duplicates are counted
/// and dropped. Order of the kept proofs is the input order.
pub(crate) fn dedup_ingress_proofs_by_signer(
    proofs: Vec<CachedIngressProof>,
) -> (Vec<CachedIngressProof>, usize) {
    keep_first_per_address(proofs, |proof| proof.address)
}

/// Proofs whose anchor is valid for inclusion. A database error is returned.
/// An unknown or too-old anchor is omitted.
pub(crate) fn anchor_valid_ingress_proofs(
    proofs: Vec<CachedIngressProof>,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    min_anchor_height: u64,
) -> eyre::Result<Vec<CachedIngressProof>> {
    let mut kept = Vec::with_capacity(proofs.len());
    for cached in proofs {
        let valid = validate_ingress_proof_anchor_for_inclusion(
            block_tree,
            db,
            min_anchor_height,
            &cached.proof,
        )?;
        if valid {
            kept.push(cached);
        }
    }
    Ok(kept)
}

/// Keep the first item for each address, in input order. Returns the kept
/// items and how many later duplicates were dropped.
pub(crate) fn keep_first_per_address<T>(
    items: Vec<T>,
    address: impl Fn(&T) -> IrysAddress,
) -> (Vec<T>, usize) {
    let before = items.len();
    let mut seen = HashSet::new();
    let kept = items
        .into_iter()
        .filter(|item| seen.insert(address(item)))
        .collect::<Vec<_>>();
    let dropped = before - kept.len();
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{H256, IrysAddress};

    #[test]
    fn selector_keeps_only_same_chain_proof_for_data_root() {
        let chain_id = 1_270;
        let data_root = H256::repeat_byte(9);
        let cached_proof = |address, proof_chain_id| {
            let mut proof = irys_types::IngressProof::default();
            proof.data_root = data_root;
            proof.chain_id = proof_chain_id;
            CompactCachedIngressProof(CachedIngressProof { address, proof })
        };
        let foreign = cached_proof(IrysAddress::repeat_byte(1), chain_id + 1);
        let same_chain = cached_proof(IrysAddress::repeat_byte(2), chain_id);

        let selected = same_chain_ingress_proofs(vec![foreign, same_chain.clone()], chain_id);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0, same_chain.0);
        assert_eq!(selected[0].proof.data_root, data_root);
    }
}
