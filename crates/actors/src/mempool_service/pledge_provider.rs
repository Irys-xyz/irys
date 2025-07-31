use crate::mempool_service::AtomicMempoolState;
use irys_domain::BlockTreeReadGuard;
use irys_primitives::CommitmentType;
use irys_types::{transaction::PledgeDataProvider, Address, H256};
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
        user_address: &Address,
        commitment_type_filter: impl Fn(&CommitmentType) -> bool,
        mut seen_ids: Option<&mut HashSet<H256>>,
    ) -> u64 {
        let mempool = self.mempool_state.read().await;

        mempool
            .valid_commitment_tx
            .get(user_address)
            .map(|txs| {
                txs.iter()
                    .filter(|tx| commitment_type_filter(&tx.commitment_type))
                    .filter(|tx| {
                        // If we have a seen_ids set, only count if we can insert (not duplicate)
                        // Otherwise count all matching transactions
                        match seen_ids.as_mut() {
                            Some(ids) => ids.insert(tx.id),
                            None => true,
                        }
                    })
                    .count() as u64
            })
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl PledgeDataProvider for MempoolPledgeProvider {
    async fn pledge_count(&self, user_address: Address) -> u64 {
        let mut seen_ids = HashSet::<H256>::new();

        // Step 1: Collect pledges from both chain snapshots in a single read lock
        let (epoch_pledges, commitment_pledges) = {
            let block_tree = self.block_tree_read_guard.read();

            let epoch_pledges = block_tree
                .canonical_epoch_snapshot()
                .commitment_state
                .pledge_commitments
                .get(&user_address)
                .cloned()
                .unwrap_or_default();

            let commitment_pledges = block_tree
                .canonical_commitment_snapshot()
                .commitments
                .get(&user_address)
                .map(|commitments| commitments.pledges.clone())
                .unwrap_or_default();

            (epoch_pledges, commitment_pledges)
        };

        // Step 2: Count unique pledges from chain state
        // Deduplication is necessary as pledges may appear in both snapshots
        let chain_pledge_count = epoch_pledges
            .into_iter()
            .map(|p| p.id)
            .chain(commitment_pledges.into_iter().map(|p| p.id))
            .filter(|id| seen_ids.insert(*id))
            .count() as u64;

        // Step 3: Count unique pending pledges from mempool
        let mempool_pledge_count = self
            .count_mempool_commitments(
                &user_address,
                |ct| matches!(ct, CommitmentType::Pledge { .. }),
                Some(&mut seen_ids),
            )
            .await;

        // Step 4: Count pending unpledges (no deduplication needed)
        let pending_unpledges = self
            .count_mempool_commitments(
                &user_address,
                |ct| matches!(ct, CommitmentType::Unpledge { .. }),
                None,
            )
            .await;

        // Final calculation: total unique pledges - pending unpledges
        (chain_pledge_count + mempool_pledge_count).saturating_sub(pending_unpledges)
    }
}
