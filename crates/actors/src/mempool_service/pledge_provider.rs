use crate::mempool_service::AtomicMempoolState;
use irys_domain::BlockTreeReadGuard;
use irys_types::CommitmentTypeV2;
use irys_types::{H256, IrysAddress, transaction::PledgeDataProvider};
use std::collections::HashSet;

/// A pledge data provider that combines both canonical chain state and mempool pending transactions
/// to provide an accurate count of pledges for fee calculation.
#[derive(Debug)]
pub struct MempoolPledgeProvider {
    mempool_state: AtomicMempoolState,
    block_tree_read_guard: BlockTreeReadGuard,
}

impl MempoolPledgeProvider {
    pub fn new(
        mempool_state: AtomicMempoolState,
        block_tree_read_guard: BlockTreeReadGuard,
    ) -> Self {
        Self {
            mempool_state,
            block_tree_read_guard,
        }
    }

    /// Counts commitment transactions of a specific type in the mempool
    /// Optionally tracks seen transaction IDs to avoid duplicates
    async fn count_mempool_commitments(
        &self,
        user_address: &IrysAddress,
        commitment_type_filter: impl Fn(CommitmentTypeV2) -> bool,
        seen_ids: &mut HashSet<H256>,
    ) -> u64 {
        self.mempool_state
            .count_mempool_commitments(user_address, commitment_type_filter, seen_ids)
            .await
    }
}

#[async_trait::async_trait]
impl PledgeDataProvider for MempoolPledgeProvider {
    async fn pledge_count(&self, user_address: IrysAddress) -> u64 {
        let mut seen_ids = HashSet::<H256>::new();

        let (epoch, commitments) = {
            let block_tree = self.block_tree_read_guard.read();
            let epoch = block_tree.canonical_epoch_snapshot();
            let commitment = block_tree.canonical_commitment_snapshot();
            (epoch, commitment)
        };

        // Collect pledges from both snapshots
        let (epoch_pledges, commitment_pledges) = {
            let epoch_pledges = epoch
                .commitment_state
                .pledge_commitments
                .get(&user_address)
                .cloned()
                .unwrap_or_default();

            let commitment_pledges = commitments
                .commitments
                .get(&user_address)
                .map(|commitments| commitments.pledges.clone())
                .unwrap_or_default();

            (epoch_pledges, commitment_pledges)
        };

        // Count unique pledges from chain state
        // Deduplication is necessary as pledges may appear in both snapshots
        let chain_pledge_count = epoch_pledges
            .into_iter()
            .map(|p| p.id)
            .chain(commitment_pledges.into_iter().map(|p| p.id()))
            .filter(|id| seen_ids.insert(*id))
            .count() as u64;

        // Count unique pending pledges from mempool
        let mempool_pledge_count = self
            .count_mempool_commitments(
                &user_address,
                |ct| matches!(ct, CommitmentTypeV2::Pledge { .. }),
                &mut seen_ids,
            )
            .await;

        // Count pending unpledges
        let pending_unpledges = self
            .count_mempool_commitments(
                &user_address,
                |ct| matches!(ct, CommitmentTypeV2::Unpledge { .. }),
                &mut seen_ids,
            )
            .await;

        // Final calculation: total unique pledges - pending unpledges
        (chain_pledge_count + mempool_pledge_count).saturating_sub(pending_unpledges)
    }
}
