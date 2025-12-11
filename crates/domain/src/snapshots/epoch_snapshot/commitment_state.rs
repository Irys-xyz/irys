use irys_types::CommitmentStatus;
use irys_types::{IrysAddress, IrysTransactionId, H256, U256};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Hash)]
pub struct CommitmentStateEntry {
    pub id: IrysTransactionId,
    pub commitment_status: CommitmentStatus,
    // Only valid for pledge commitments
    pub partition_hash: Option<H256>,
    pub signer: IrysAddress,
    /// Irys token amount in atomic units
    pub amount: U256,
}

#[derive(Debug, Default, Clone, Hash)]
pub struct CommitmentState {
    pub stake_commitments: BTreeMap<IrysAddress, CommitmentStateEntry>,
    pub pledge_commitments: BTreeMap<IrysAddress, Vec<CommitmentStateEntry>>,
}

impl CommitmentState {
    pub(crate) fn is_staked(&self, address: IrysAddress) -> bool {
        if let Some(commitment) = self.stake_commitments.get(&address) {
            return commitment.commitment_status == CommitmentStatus::Active;
        }
        false
    }
}
