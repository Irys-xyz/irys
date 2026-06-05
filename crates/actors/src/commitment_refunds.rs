use irys_domain::CommitmentSnapshot;
use irys_types::CommitmentTypeV2;
use irys_types::{CommitmentTransaction, ConsensusConfig};

use crate::block_producer::{UnpledgeRefundEvent, UnstakeRefundEvent};

/// Typed error for commitment-refund derivation.
///
/// Today the only failure shape is a snapshot invariant violation, which
/// is always a node fault: the local commitment snapshot is internally
/// inconsistent and retry cannot heal it. On the validator side this routes
/// to `ValidationError::ShadowTxNodeFault` (panic + supervisor restart);
/// on the producer side this is wrapped through `eyre::Report` and bubbles
/// as an `Irrecoverable` block production error.
#[derive(Debug, thiserror::Error)]
pub enum CommitmentRefundError {
    /// Snapshot invariant violation (e.g. `pledge_count_before_executing == 0`).
    /// Always a node fault.
    #[error("snapshot invariant violation: {0}")]
    SnapshotInvariant(String),
}

// Producer-side call sites return `eyre::Result`. `eyre::Report` provides a
// blanket `From<E: std::error::Error + Send + Sync + 'static>` impl, so `?`
// lifts `CommitmentRefundError` automatically — no explicit `From` needed.

/// Derive epoch unpledge refund events deterministically from a commitment snapshot.
///
/// Ordering is the same as CommitmentTransaction::Ord for Unpledge txs
/// (pledge_count_before_executing asc, fee desc, id asc).
pub(crate) fn derive_unpledge_refunds_from_snapshot(
    commit_snapshot: &CommitmentSnapshot,
    config: &ConsensusConfig,
) -> Result<Vec<UnpledgeRefundEvent>, CommitmentRefundError> {
    let mut unpledges: Vec<CommitmentTransaction> = commit_snapshot
        .commitments
        .values()
        .flat_map(|mc| mc.unpledges.iter().cloned())
        .collect();

    unpledges.sort();
    let mut out = Vec::with_capacity(unpledges.len());
    for tx in unpledges {
        let amount = match tx.commitment_type() {
            CommitmentTypeV2::Unpledge {
                pledge_count_before_executing,
                ..
            } => {
                if pledge_count_before_executing == 0 {
                    return Err(CommitmentRefundError::SnapshotInvariant(format!(
                        "Invalid unpledge in epoch snapshot: pledge_count_before_executing = 0 (tx: {:?})",
                        tx.id()
                    )));
                }
                CommitmentTransaction::calculate_pledge_value_at_count(
                    config,
                    pledge_count_before_executing - 1,
                )
            }
            _ => unreachable!("only unpledge expected here"),
        };
        out.push(UnpledgeRefundEvent {
            account: tx.signer(),
            amount,
            irys_ref_txid: tx.id(),
        });
    }
    Ok(out)
}

/// Derive epoch unstake refund events deterministically from a commitment snapshot.
///
/// Ordering is the same as CommitmentTransaction::Ord for Unstake txs
pub(crate) fn derive_unstake_refunds_from_snapshot(
    commit_snapshot: &CommitmentSnapshot,
    config: &ConsensusConfig,
) -> Result<Vec<UnstakeRefundEvent>, CommitmentRefundError> {
    // Collect all unstakes from the snapshot
    let mut unstakes: Vec<CommitmentTransaction> = commit_snapshot
        .commitments
        .values()
        .filter_map(|mc| mc.unstake.clone())
        .collect();

    unstakes.sort();
    let mut out = Vec::with_capacity(unstakes.len());
    for tx in unstakes {
        // Refund equals the staked value (from config); inclusion was fee-only
        let amount = config.stake_value.amount;
        out.push(UnstakeRefundEvent {
            account: tx.signer(),
            amount,
            irys_ref_txid: tx.id(),
        });
    }
    Ok(out)
}
