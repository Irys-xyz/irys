use irys_primitives::CommitmentStatus;
use irys_types::{Address, IrysTransactionId, H256};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct CommitmentStateEntry {
    pub id: IrysTransactionId,
    pub commitment_status: CommitmentStatus,
    // Only valid for pledge commitments
    pub partition_hash: Option<H256>,
    pub signer: Address,
    /// Irys token amount in atomic units
    #[allow(dead_code)]
    pub amount: u64,
}

#[derive(Debug, Default)]
pub struct CommitmentState {
    pub stake_commitments: BTreeMap<Address, CommitmentStateEntry>,
    pub pledge_commitments: BTreeMap<Address, Vec<CommitmentStateEntry>>,
}
