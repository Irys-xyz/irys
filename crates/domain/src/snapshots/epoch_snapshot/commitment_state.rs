use irys_types::CommitmentStatus;
use irys_types::{H256, IrysAddress, IrysTransactionId, U256};
use std::collections::BTreeMap;

/// Entry representing a stake commitment.
#[derive(Debug, Clone, Hash)]
pub struct StakeEntry {
    pub id: IrysTransactionId,
    pub commitment_status: CommitmentStatus,
    pub signer: IrysAddress,
    /// Irys token amount in atomic units
    pub amount: U256,
    /// Address to receive rewards
    pub reward_address: IrysAddress,
}

/// Entry representing a pledge commitment.
#[derive(Debug, Clone, Hash)]
pub struct PledgeEntry {
    pub id: IrysTransactionId,
    pub commitment_status: CommitmentStatus,
    pub signer: IrysAddress,
    /// Irys token amount in atomic units
    pub amount: U256,
    /// Partition hash assigned to this pledge (None until assigned)
    pub partition_hash: Option<H256>,
}

#[derive(Debug, Default, Clone, Hash)]
pub struct CommitmentState {
    pub stake_commitments: BTreeMap<IrysAddress, StakeEntry>,
    pub pledge_commitments: BTreeMap<IrysAddress, Vec<PledgeEntry>>,
}

impl CommitmentState {
    pub(crate) fn is_staked(&self, address: IrysAddress) -> bool {
        if let Some(commitment) = self.stake_commitments.get(&address) {
            return commitment.commitment_status == CommitmentStatus::Active;
        }
        false
    }
}
