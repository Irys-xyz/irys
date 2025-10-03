use eyre::{bail, Result};
use irys_domain::CommitmentSnapshot;
use irys_primitives::CommitmentType;
use irys_types::{CommitmentTransaction, ConsensusConfig};

use crate::block_producer::UnpledgeRefundEvent;

/// Derive epoch unpledge refund events deterministically from a commitment snapshot.
///
/// Ordering is the same as CommitmentTransaction::Ord for Unpledge txs
/// (pledge_count_before_executing asc, fee desc, id asc).
pub(crate) fn derive_unpledge_refunds_from_snapshot(
    commit_snapshot: &CommitmentSnapshot,
    config: &ConsensusConfig,
) -> Result<Vec<UnpledgeRefundEvent>> {
    let mut unpledges: Vec<CommitmentTransaction> = commit_snapshot
        .commitments
        .values()
        .flat_map(|mc| mc.unpledges.iter().cloned())
        .collect();

    unpledges.sort();
    let mut out = Vec::with_capacity(unpledges.len());
    for tx in unpledges.into_iter() {
        let amount = match tx.commitment_type {
            CommitmentType::Unpledge { pledge_count_before_executing, .. } => {
                if pledge_count_before_executing == 0 {
                    bail!(
                        "Invalid unpledge in epoch snapshot: pledge_count_before_executing = 0 (tx: {:?})",
                        tx.id
                    );
                }
                CommitmentTransaction::calculate_pledge_value_at_count(
                    config,
                    pledge_count_before_executing - 1,
                )
            }
            _ => unreachable!("only unpledge expected here"),
        };
        out.push(UnpledgeRefundEvent { account: tx.signer, amount, irys_ref_txid: tx.id });
    }
    Ok(out)
}
