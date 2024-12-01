use actix::MessageResponse;
use serde::{Deserialize, Serialize};

use crate::{Address, H256};

/// A H256 hash that uniquely identifies a partition
pub type PartitionHash = H256;

/// Temporary struct tracking partition assignments to miners - will be moved to database
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, MessageResponse, Clone, Copy)]
pub struct PartitionAssignment {
    /// Hash of the partition
    pub partition_hash: PartitionHash,
    /// Address of the miner pledged to store it
    pub miner_address: Address,
    /// If assigned to a ledger, the ledger number
    pub ledger_num: Option<u64>,
    /// If assigned to a ledger, the index in the ledger
    pub slot_index: Option<usize>,
}
