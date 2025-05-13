use std::collections::BTreeMap;

use base58::ToBase58;
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    DataLedger, H256,
};
use tracing::debug;

/// A state struct that can be wrapped with Arc<`RwLock`<>> to provide parallel read access
#[derive(Debug)]
pub struct PartitionAssignments {
    /// Active data partition state mapped by partition hash
    pub data_partitions: BTreeMap<PartitionHash, PartitionAssignment>,
    /// Available capacity partitions mapped by partition hash
    pub capacity_partitions: BTreeMap<PartitionHash, PartitionAssignment>,
}

/// Implementation helper functions
impl Default for PartitionAssignments {
    fn default() -> Self {
        Self::new()
    }
}

impl PartitionAssignments {
    /// Initialize a new `PartitionAssignments` state wrapper struct
    pub fn new() -> Self {
        Self {
            data_partitions: BTreeMap::new(),
            capacity_partitions: BTreeMap::new(),
        }
    }

    /// Retrieves a `PartitionAssignment` by partition hash if it exists
    pub fn get_assignment(&self, partition_hash: H256) -> Option<PartitionAssignment> {
        self.data_partitions
            .get(&partition_hash)
            .copied()
            .or(self.capacity_partitions.get(&partition_hash).copied())
    }

    // TODO: convert to Display impl for PartitionAssignments
    pub fn print_assignments(&self) {
        debug!(
            "Partition Assignments ({}):",
            self.data_partitions.len() + self.capacity_partitions.len()
        );

        // List Publish ledger assignments, ordered by index
        let mut publish_assignments: Vec<_> = self
            .data_partitions
            .iter()
            .filter(|(_, a)| a.ledger_id == Some(DataLedger::Publish as u32))
            .collect();
        publish_assignments.sort_unstable_by(|(_, a1), (_, a2)| a1.slot_index.cmp(&a2.slot_index));

        for (hash, assignment) in publish_assignments {
            let ledger = DataLedger::try_from(assignment.ledger_id.unwrap()).unwrap();
            debug!(
                "{:?}[{}] {} miner: {}",
                ledger,
                assignment.slot_index.unwrap(),
                hash.0.to_base58(),
                assignment.miner_address
            );
        }

        // List Submit ledger assignments, ordered by index
        let mut submit_assignments: Vec<_> = self
            .data_partitions
            .iter()
            .filter(|(_, a)| a.ledger_id == Some(DataLedger::Submit as u32))
            .collect();
        submit_assignments.sort_unstable_by(|(_, a1), (_, a2)| a1.slot_index.cmp(&a2.slot_index));
        for (hash, assignment) in submit_assignments {
            let ledger = DataLedger::try_from(assignment.ledger_id.unwrap()).unwrap();
            debug!(
                "{:?}[{}] {} miner: {}",
                ledger,
                assignment.slot_index.unwrap(),
                hash.0.to_base58(),
                assignment.miner_address
            );
        }

        // List capacity ledger assignments, ordered by hash (natural ordering)
        for (index, (hash, assignment)) in self.capacity_partitions.iter().enumerate() {
            debug!(
                "Capacity[{}] {} miner: {}",
                index,
                hash.0.to_base58(),
                assignment.miner_address
            );
        }
    }
}
