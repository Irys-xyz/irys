use super::{CommitmentState, EpochServiceActor, EpochServiceError, PartitionAssignments};
use crate::services::Stop;
use actix::{ActorContext as _, Handler, Message, MessageResponse};
use irys_database::Ledgers;
use irys_primitives::CommitmentStatus;
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    Address, CommitmentTransaction, IrysBlockHeader,
};
use std::sync::{Arc, RwLock, RwLockReadGuard};

/// Sent when a new epoch block is reached (and at genesis)
#[derive(Message, Debug)]
#[rtype(result = "Result<(),EpochServiceError>")]
pub struct NewEpochMessage {
    pub epoch_block: Arc<IrysBlockHeader>,
    pub previous_epoch_block: Option<IrysBlockHeader>,
    pub commitments: Arc<Vec<CommitmentTransaction>>,
}

impl Handler<NewEpochMessage> for EpochServiceActor {
    type Result = Result<(), EpochServiceError>;
    fn handle(&mut self, msg: NewEpochMessage, _ctx: &mut Self::Context) -> Self::Result {
        let new_epoch_block = msg.epoch_block;
        let new_epoch_commitments = msg.commitments;
        let previous_epoch_block = msg.previous_epoch_block;

        self.perform_epoch_tasks(
            &previous_epoch_block,
            &new_epoch_block,
            (*new_epoch_commitments).clone(),
        )?;

        Ok(())
    }
}

//==============================================================================
// LedgersReadGuard
//------------------------------------------------------------------------------
/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
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

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "LedgersReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetLedgersGuardMessage;

impl Handler<GetLedgersGuardMessage> for EpochServiceActor {
    type Result = LedgersReadGuard; // Return guard directly

    fn handle(&mut self, _msg: GetLedgersGuardMessage, _ctx: &mut Self::Context) -> Self::Result {
        LedgersReadGuard::new(Arc::clone(&self.ledgers))
    }
}

//==============================================================================
// PartitionAssignmentsReadGuard
//------------------------------------------------------------------------------
/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
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

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "PartitionAssignmentsReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetPartitionAssignmentsGuardMessage;

impl Handler<GetPartitionAssignmentsGuardMessage> for EpochServiceActor {
    type Result = PartitionAssignmentsReadGuard; // Return guard directly

    fn handle(
        &mut self,
        _msg: GetPartitionAssignmentsGuardMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        PartitionAssignmentsReadGuard::new(self.partition_assignments.clone())
    }
}

//==============================================================================
// CommitmentStateReadGuard
//------------------------------------------------------------------------------
/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
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

/// Retrieve a read only reference to the commitment state
#[derive(Message, Debug)]
#[rtype(result = "CommitmentStateReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetCommitmentStateGuardMessage;

impl Handler<GetCommitmentStateGuardMessage> for EpochServiceActor {
    type Result = CommitmentStateReadGuard; // Return guard directly
    fn handle(
        &mut self,
        _msg: GetCommitmentStateGuardMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        CommitmentStateReadGuard::new(self.commitment_state.clone())
    }
}

//==============================================================================
// GetPartitionAssignment
//------------------------------------------------------------------------------
/// Retrieve partition assignment (ledger and its relative offset) for a partition
#[derive(Message, Debug)]
#[rtype(result = "Option<PartitionAssignment>")]
pub struct GetPartitionAssignmentMessage(pub PartitionHash);

impl Handler<GetPartitionAssignmentMessage> for EpochServiceActor {
    type Result = Option<PartitionAssignment>;
    fn handle(
        &mut self,
        msg: GetPartitionAssignmentMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let pa = self.partition_assignments.read().unwrap();
        pa.get_assignment(msg.0)
    }
}

//==============================================================================
// GetMinerPartitionAssignments
//------------------------------------------------------------------------------
/// Retrieve all partition assignments for a given mining address
#[derive(Message, Debug)]
#[rtype(result = "Vec<PartitionAssignment>")]
pub struct GetMinerPartitionAssignmentsMessage(pub Address);

impl Handler<GetMinerPartitionAssignmentsMessage> for EpochServiceActor {
    type Result = Vec<PartitionAssignment>;
    fn handle(
        &mut self,
        msg: GetMinerPartitionAssignmentsMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let miner_address = msg.0;
        self.get_partition_assignments(miner_address)
    }
}

//==============================================================================
// Stop
//------------------------------------------------------------------------------
impl Handler<Stop> for EpochServiceActor {
    type Result = ();

    fn handle(&mut self, _msg: Stop, ctx: &mut Self::Context) {
        ctx.stop();
    }
}
