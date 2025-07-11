use super::{CommitmentState, PartitionAssignments};
use irys_database::Ledgers;
use irys_primitives::CommitmentStatus;
use irys_types::Address;
use std::sync::{Arc, RwLock, RwLockReadGuard};
//==============================================================================
// LedgersReadGuard
//------------------------------------------------------------------------------
/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct LedgersReadGuard {
    ledgers: Arc<RwLock<Ledgers>>,
}

impl LedgersReadGuard {
    /// Creates a new `ReadGuard` for Ledgers
    pub const fn new(ledgers: Arc<RwLock<Ledgers>>) -> Self {
        Self { ledgers }
    }

    /// Accessor method to get a read guard for Ledgers
    pub fn read(&self) -> RwLockReadGuard<'_, Ledgers> {
        self.ledgers.read().unwrap()
    }
}

//==============================================================================
// PartitionAssignmentsReadGuard
//------------------------------------------------------------------------------
/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct PartitionAssignmentsReadGuard {
    partition_assignments: Arc<RwLock<PartitionAssignments>>,
}

impl PartitionAssignmentsReadGuard {
    /// Creates a new `ReadGuard` for Ledgers
    pub const fn new(partition_assignments: Arc<RwLock<PartitionAssignments>>) -> Self {
        Self {
            partition_assignments,
        }
    }

    /// Accessor method to get a read guard for Ledgers
    pub fn read(&self) -> RwLockReadGuard<'_, PartitionAssignments> {
        self.partition_assignments.read().unwrap()
    }
}

//==============================================================================
// CommitmentStateReadGuard
//------------------------------------------------------------------------------
/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct CommitmentStateReadGuard {
    commitment_state: Arc<RwLock<CommitmentState>>,
}

impl CommitmentStateReadGuard {
    /// Creates a new `ReadGuard` for the CommitmentState
    pub const fn new(commitment_state: Arc<RwLock<CommitmentState>>) -> Self {
        Self { commitment_state }
    }

    /// Accessor method to get a ReadGuard for the CommitmentState
    pub fn read(&self) -> RwLockReadGuard<'_, CommitmentState> {
        self.commitment_state.read().unwrap()
    }

    pub fn is_staked(&self, address: Address) -> bool {
        if let Some(commitment) = self.read().stake_commitments.get(&address) {
            return commitment.commitment_status == CommitmentStatus::Active;
        }
        false
    }
}
