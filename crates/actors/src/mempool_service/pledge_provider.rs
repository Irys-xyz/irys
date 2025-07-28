use crate::mempool_service::AtomicMempoolState;
use irys_domain::BlockTreeReadGuard;
use irys_primitives::CommitmentType;
use irys_types::{transaction::PledgeDataProvider, Address};

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
}

#[async_trait::async_trait]
impl PledgeDataProvider for MempoolPledgeProvider {
    async fn pledge_count(&self, user_address: Address) -> usize {
        // Get the canonical pledge count from the blockchain state
        let canonical_count = {
            let commitment_snapshot = self
                .block_tree_read_guard
                .read()
                .canonical_commitment_snapshot();
            commitment_snapshot
                .commitments
                .get(&user_address)
                .map(|miner_commitments| miner_commitments.pledges.len())
                .unwrap_or(0)
        };

        // Count pending pledge transactions in the mempool
        let pending_pledges = {
            let mempool = self.mempool_state.read().await;
            mempool
                .valid_commitment_tx
                .get(&user_address)
                .map(|txs| {
                    txs.iter()
                        .filter(|tx| tx.commitment_type == CommitmentType::Pledge)
                        .count()
                })
                .unwrap_or(0)
        };

        // Count pending unpledge transactions in the mempool
        let pending_unpledges = {
            let mempool = self.mempool_state.read().await;
            mempool
                .valid_commitment_tx
                .get(&user_address)
                .map(|txs| {
                    txs.iter()
                        .filter(|tx| tx.commitment_type == CommitmentType::Unpledge)
                        .count()
                })
                .unwrap_or(0)
        };

        // Calculate effective pledge count:
        // canonical pledges + pending pledges - pending unpledges
        canonical_count
            .saturating_add(pending_pledges)
            .saturating_sub(pending_unpledges)
    }
}
