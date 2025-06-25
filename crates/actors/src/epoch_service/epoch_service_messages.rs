use super::{CommitmentState, EpochServiceError, EpochServiceInner, PartitionAssignments};
use crate::EpochReplayData;
use irys_database::Ledgers;
use irys_primitives::CommitmentStatus;
use irys_storage::StorageModuleInfo;
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    Address, CommitmentTransaction, IrysBlockHeader,
};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use tokio::sync::oneshot;

// Messages that the StorageModuleService service supports
#[derive(Debug)]
pub enum EpochServiceMessage {
    // Tells the epoch service a new epoch block has been discovered
    NewEpoch {
        new_epoch_block: Arc<IrysBlockHeader>,
        previous_epoch_block: Option<IrysBlockHeader>,
        commitments: Arc<Vec<CommitmentTransaction>>,
        sender: oneshot::Sender<Result<(), EpochServiceError>>,
    },
    // Read only access to ledgers state (maps ledger slots to partition hashes)
    GetLedgersGuard(oneshot::Sender<LedgersReadGuard>),
    // Read only access ot the PartitionAssignments state (mapping partition hashes to miner addresses)
    GetPartitionAssignmentsGuard(oneshot::Sender<PartitionAssignmentsReadGuard>),
    // Read only access to commitment state (status of stakes and pledges at the end of the last epoch)
    GetCommitmentStateGuard(oneshot::Sender<CommitmentStateReadGuard>),
    // Gets partition metadata for a particular partition hash
    GetPartitionAssignment(PartitionHash, oneshot::Sender<Option<PartitionAssignment>>),
    // Gets all the partition assignments for a given miner address
    GetMinerPartitionAssignments(Address, oneshot::Sender<Vec<PartitionAssignment>>),
    ReplayEpochData(
        Vec<EpochReplayData>,
        oneshot::Sender<Vec<StorageModuleInfo>>,
    ),
}

impl EpochServiceInner {
    pub fn handle_message(&mut self, msg: EpochServiceMessage) -> eyre::Result<()> {
        match msg {
            EpochServiceMessage::NewEpoch {
                new_epoch_block,
                previous_epoch_block,
                commitments,
                sender,
            } => {
                self.handle_new_epoch(new_epoch_block, previous_epoch_block, commitments, sender)?;
            }
            EpochServiceMessage::GetLedgersGuard(sender) => {
                self.handle_get_ledgers_guard(sender)?;
            }
            EpochServiceMessage::GetPartitionAssignmentsGuard(sender) => {
                self.handle_get_partition_assignments_guard(sender)?;
            }
            EpochServiceMessage::GetCommitmentStateGuard(sender) => {
                self.handle_get_commitment_state_guard(sender)?;
            }
            EpochServiceMessage::GetPartitionAssignment(partition_hash, sender) => {
                self.handle_get_partition_assignment(partition_hash, sender)?;
            }
            EpochServiceMessage::GetMinerPartitionAssignments(miner_address, sender) => {
                self.handle_get_miner_partition_assignments(miner_address, sender)?;
            }
            EpochServiceMessage::ReplayEpochData(epoch_replay_data, sender) => {
                self.handle_replay_epoch_data(epoch_replay_data, sender)?;
            }
        }
        Ok(())
    }

    fn handle_new_epoch(
        &mut self,
        new_epoch_block: Arc<IrysBlockHeader>,
        previous_epoch_block: Option<IrysBlockHeader>,
        commitments: Arc<Vec<CommitmentTransaction>>,
        response: oneshot::Sender<Result<(), EpochServiceError>>,
    ) -> eyre::Result<()> {
        match response.send(self.perform_epoch_tasks(
            &previous_epoch_block,
            &new_epoch_block,
            (*commitments).clone(),
        )) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }

    fn handle_get_ledgers_guard(
        &self,
        sender: oneshot::Sender<LedgersReadGuard>,
    ) -> eyre::Result<()> {
        match sender.send(LedgersReadGuard::new(Arc::clone(&self.ledgers))) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }

    fn handle_get_partition_assignments_guard(
        &self,
        sender: oneshot::Sender<PartitionAssignmentsReadGuard>,
    ) -> eyre::Result<()> {
        match sender.send(PartitionAssignmentsReadGuard::new(
            self.partition_assignments.clone(),
        )) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }

    fn handle_get_commitment_state_guard(
        &self,
        sender: oneshot::Sender<CommitmentStateReadGuard>,
    ) -> eyre::Result<()> {
        match sender.send(CommitmentStateReadGuard::new(self.commitment_state.clone())) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }

    fn handle_get_partition_assignment(
        &self,
        partition_hash: PartitionHash,
        sender: oneshot::Sender<Option<PartitionAssignment>>,
    ) -> eyre::Result<()> {
        let pa = self.partition_assignments.read().unwrap();
        match sender.send(pa.get_assignment(partition_hash)) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }

    fn handle_get_miner_partition_assignments(
        &self,
        miner_address: Address,
        sender: oneshot::Sender<Vec<PartitionAssignment>>,
    ) -> eyre::Result<()> {
        match sender.send(self.get_partition_assignments(miner_address)) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }

    fn handle_replay_epoch_data(
        &mut self,
        epoch_replay_data: Vec<EpochReplayData>,
        sender: oneshot::Sender<Vec<StorageModuleInfo>>,
    ) -> eyre::Result<()> {
        let infos = self.replay_epoch_data(epoch_replay_data)?;
        match sender.send(infos) {
            Ok(()) => Ok(()),
            Err(e) => Err(eyre::eyre!("response.send() error: {:?}", e)),
        }
    }
}

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
