use actix::SystemService;
use actix::{Actor, ActorContext, Context, Handler, Message, MessageResponse};
use base58::ToBase58;
use eyre::{Error, Result};
use irys_config::StorageSubmodulesConfig;
use irys_database::{block_header_by_hash, commitment_tx_by_txid, data_ledger::*, SystemLedger};
use irys_primitives::CommitmentStatus;
use irys_storage::{ie, StorageModuleInfo};
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    DatabaseProvider, IrysBlockHeader, SimpleRNG, StorageSyncConfig, H256,
};
use irys_types::{
    partition_chunk_offset_ie, Address, CommitmentTransaction, ConsensusConfig, IrysTransactionId,
    NodeConfig, PartitionChunkOffset,
};
use irys_types::{EpochConfig, H256List};
use openssl::sha;
use reth_db::Database;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, RwLock, RwLockReadGuard},
};

use tracing::{debug, error, trace, warn};

use crate::block_index_service::BlockIndexReadGuard;
use crate::broadcast_mining_service::{BroadcastMiningService, BroadcastPartitionsExpiration};
use crate::services::Stop;

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

//==============================================================================
// CommitmentState
//------------------------------------------------------------------------------
#[derive(Debug, Default, Clone)]
pub struct CommitmentStateEntry {
    id: IrysTransactionId,
    commitment_status: CommitmentStatus,
    partition_hash: Option<H256>,
    signer: Address,
    /// Irys token amount in atomic units
    #[allow(dead_code)]
    amount: u64,
}

#[derive(Debug, Default)]
pub struct CommitmentState {
    pub stake_commitments: BTreeMap<Address, CommitmentStateEntry>,
    pub pledge_commitments: BTreeMap<Address, Vec<CommitmentStateEntry>>,
}

/// Temporarily track all of the ledger definitions inside the epoch service actor
#[derive(Debug)]
pub struct EpochServiceActor {
    /// Source of randomness derived from previous epoch
    pub last_epoch_hash: H256,
    /// Protocol-managed data ledgers (one permanent, N term)
    pub ledgers: Arc<RwLock<Ledgers>>,
    /// Tracks active mining assignments for partitions (by hash)
    pub partition_assignments: Arc<RwLock<PartitionAssignments>>,
    /// Sequential list of activated partition hashes
    pub all_active_partitions: Vec<PartitionHash>,
    /// List of partition hashes not yet assigned to a mining address
    pub unassigned_partitions: Vec<PartitionHash>,
    /// Current partition & ledger parameters
    pub config: EpochConfig,
    ///
    pub miner_address: Address,
    ///
    pub num_partitions_per_slot: u64,
    ///
    pub num_chunks_in_partition: u64,
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// Computed commitment state
    commitment_state: Arc<RwLock<CommitmentState>>,
}

impl Actor for EpochServiceActor {
    type Context = Context<Self>;
}

/// Sent when a new epoch block is reached (and at genesis)
#[derive(Message, Debug)]
#[rtype(result = "Result<(),EpochServiceError>")]
pub struct NewEpochMessage {
    pub epoch_block: Arc<IrysBlockHeader>,
    pub commitments: Vec<CommitmentTransaction>,
}

impl Handler<NewEpochMessage> for EpochServiceActor {
    type Result = Result<(), EpochServiceError>;
    fn handle(&mut self, msg: NewEpochMessage, _ctx: &mut Self::Context) -> Self::Result {
        let new_epoch_block = msg.epoch_block;
        let commitments = msg.commitments;

        self.perform_epoch_tasks(new_epoch_block, commitments)?;

        Ok(())
    }
}

/// Reasons why the epoch service actors epoch tasks might fail
#[derive(Debug)]
pub enum EpochServiceError {
    /// Catchall error until more detailed errors are added
    InternalError,
    /// Attempted to do epoch tasks on a block that was not an epoch block
    NotAnEpochBlock,
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
// EpochServiceActor implementation
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

impl Handler<Stop> for EpochServiceActor {
    type Result = ();

    fn handle(&mut self, _msg: Stop, ctx: &mut Self::Context) {
        ctx.stop();
    }
}

impl EpochServiceActor {
    /// Create a new instance of the epoch service actor
    pub fn new(
        node_config: &NodeConfig,
        config: &ConsensusConfig,
        block_index_guard: BlockIndexReadGuard,
    ) -> Self {
        Self {
            last_epoch_hash: H256::zero(),
            ledgers: Arc::new(RwLock::new(Ledgers::new(config))),
            partition_assignments: Arc::new(RwLock::new(PartitionAssignments::new())),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            config: config.epoch.clone(),
            block_index_guard,
            commitment_state: Default::default(),
            num_partitions_per_slot: config.num_partitions_per_slot,
            num_chunks_in_partition: config.num_chunks_in_partition,
            miner_address: node_config.miner_address(),
        }
    }

    pub async fn initialize(
        &mut self,
        db: &DatabaseProvider,
        storage_module_config: StorageSubmodulesConfig,
    ) -> eyre::Result<Vec<StorageModuleInfo>> {
        let mut block_index = 0;

        // Loop though all the epoch blocks, starting with genesis (block_index = 0)
        loop {
            let block_height = {
                self.block_index_guard
                    .read()
                    .get_item(block_index)
                    .map(|block| block.clone())
                    .clone()
            };

            match block_height {
                Some(b) => {
                    let tx = &db.tx().expect("to create readonly mdbx tx");
                    let block_header = block_header_by_hash(tx, &b.block_hash, false)
                        .unwrap()
                        .expect(&format!(
                            "to find the block header at height {}",
                            block_index
                        ));

                    // Get the commitments_ledger from the block
                    let commitments_ledger = block_header
                        .system_ledgers
                        .iter()
                        .find(|b| b.ledger_id == SystemLedger::Commitment);

                    // Build a list of CommitmentTransactions from the commitments ledger txids
                    let mut commitments: Vec<CommitmentTransaction> = Vec::new();
                    if let Some(commitments_ledger) = commitments_ledger {
                        for commitment_txid in commitments_ledger.tx_ids.iter() {
                            let commitment_tx = commitment_tx_by_txid(tx, commitment_txid)
                                .expect("getting commitment tx should succeed");
                            if let Some(commitment_tx) = commitment_tx {
                                commitments.push(commitment_tx);
                            } else {
                                return Err(eyre::eyre!(
                                    "Commitment missing in database {:?}",
                                    commitment_txid.0.to_base58()
                                ));
                            }
                        }
                    }

                    // Process epoch tasks with the block header and commitments
                    match self.perform_epoch_tasks(Arc::new(block_header), commitments) {
                        Ok(_) => debug!(?block_index, "Processed epoch block"),
                        Err(e) => {
                            self.print_items(self.block_index_guard.clone(), db.clone());
                            return Err(eyre::eyre!("Error performing epoch tasks {:?}", e));
                        }
                    }
                    block_index += TryInto::<usize>::try_into(self.config.num_blocks_in_epoch)
                        .expect("Number of blocks in epoch is too large!");
                }
                None => {
                    debug!(
                        "Could not recover block at index during epoch service initialization {block_index:?}"
                    );
                    break;
                }
            }
        }

        let storage_module_info =
            self.map_storage_modules_to_partition_assignments(storage_module_config);

        Ok(storage_module_info)
    }

    fn print_items(&self, block_index_guard: BlockIndexReadGuard, db: DatabaseProvider) {
        let rg = block_index_guard.read();
        let tx = db.tx().unwrap();
        for i in 0..rg.num_blocks() {
            let item = rg.get_item(i as usize).unwrap();
            let block_hash = item.block_hash;
            let block = block_header_by_hash(&tx, &block_hash, false)
                .unwrap()
                .unwrap();
            debug!(
                "index: {} height: {} hash: {}",
                i,
                block.height,
                block_hash.0.to_base58()
            );
        }
    }

    /// Main worker function
    pub fn perform_epoch_tasks(
        &mut self,
        new_epoch_block: Arc<IrysBlockHeader>,
        commitments: Vec<CommitmentTransaction>,
    ) -> Result<(), EpochServiceError> {
        // Validate this is an epoch block height
        if new_epoch_block.height % self.config.num_blocks_in_epoch != 0 {
            error!(
                "Not an epoch block height: {} num_blocks_in_epoch: {}",
                new_epoch_block.height, self.config.num_blocks_in_epoch
            );
            return Err(EpochServiceError::NotAnEpochBlock);
        }

        debug!(
            "Performing epoch tasks for {} ({})",
            &new_epoch_block.block_hash, &new_epoch_block.height
        );

        // These commitment tx must be pre-validated
        self.compute_commitment_state(commitments);

        self.try_genesis_init(&new_epoch_block);

        self.expire_term_ledger_slots(&new_epoch_block);

        self.allocate_additional_ledger_slots(&new_epoch_block);

        self.backfill_missing_partitions();

        self.allocate_additional_capacity();

        self.assign_partition_hashes_to_pledges();

        Ok(())
    }

    /// Initializes genesis state when a genesis block is processed for the first time
    ///
    /// This function performs critical one-time setup when processing the genesis block:
    /// 1. Stores the genesis epoch hash
    /// 2. Allocates initial slots for each data ledger
    /// 3. Creates the necessary capacity partitions based on data partitions
    /// 4. Assigns capacity partitions to pledge commitments
    ///
    /// The function only executes if:
    /// - No active partitions exist yet (indicating first run)
    /// - The block is a genesis block
    ///
    /// # Arguments
    /// * `new_epoch_block` - The genesis block to initialize from
    fn try_genesis_init(&mut self, new_epoch_block: &IrysBlockHeader) {
        if self.all_active_partitions.is_empty() && new_epoch_block.is_genesis() {
            debug!("Performing genesis init");
            // Store the genesis epoch hash
            self.last_epoch_hash = new_epoch_block.last_epoch_hash;

            // Allocate 1 slot to each ledger and calculate the number of partitions
            let mut num_data_partitions = 0;
            {
                // Create a scope for the write lock to expire with
                let mut ledgers = self.ledgers.write().unwrap();
                for ledger in DataLedger::iter() {
                    debug!("Allocating 1 slot for {:?}", &ledger);
                    num_data_partitions += ledgers[ledger].allocate_slots(1);
                }
            }

            // Calculate the total number of capacity partitions
            let projected_capacity_parts =
                Self::get_num_capacity_partitions(num_data_partitions, &self.config);

            // Determine the number of capacity partitions to create
            // We take the greater of:
            // 1. The config override (if specified)
            // 2. The projected number calculated from data partitions
            self.add_capacity_partitions(std::cmp::max(
                self.config
                    .num_capacity_partitions
                    .unwrap_or(projected_capacity_parts),
                projected_capacity_parts,
            ));

            // Assign capacity partition hashes to genesis pledge commitments
            // In previous single-node testnet, these were automatically assigned to the local node
            // Now, with multi-node support, we explicitly assign them to pledge commitments
            // This ensures backfill_missing_partitions() has properly assigned partitions
            // available for data ledger slots during genesis perform_epoch_tasks()
            self.assign_partition_hashes_to_pledges();
        } else {
            debug!(
                "Skipping genesis init - active parts empty? {}, epoch height: {}",
                self.all_active_partitions.is_empty(),
                new_epoch_block.height
            );
        }
    }

    /// Loops though all of the term ledgers and looks for slots that are older
    /// than the `epoch_length` (term length) of the ledger.
    fn expire_term_ledger_slots(&self, new_epoch_block: &IrysBlockHeader) {
        let epoch_height = new_epoch_block.height;
        let expired_hashes: Vec<H256>;
        {
            let mut ledgers = self.ledgers.write().unwrap();
            expired_hashes = ledgers.get_expired_partition_hashes(epoch_height);
        }

        let mining_broadcaster_addr = BroadcastMiningService::from_registry();
        mining_broadcaster_addr.do_send(BroadcastPartitionsExpiration(H256List(
            expired_hashes.clone(),
        )));

        // Update expired data partitions assignments marking them as capacity partitions
        for partition_hash in expired_hashes {
            self.return_expired_partition_to_capacity(partition_hash);
        }
    }

    /// Loops though all the ledgers both perm and term, checking to see if any
    /// require additional ledger slots added to accommodate data ingress.
    fn allocate_additional_ledger_slots(&self, new_epoch_block: &IrysBlockHeader) {
        for ledger in DataLedger::iter() {
            let part_slots = self.calculate_additional_slots(new_epoch_block, ledger);
            {
                let mut ledgers = self.ledgers.write().unwrap();
                debug!("Allocating {} slots for ledger {:?}", &part_slots, &ledger);
                ledgers[ledger].allocate_slots(part_slots);
            }
        }
    }

    /// Based on the amount of data in each ledger, this function calculates
    /// the number of partitions the protocol should be managing and allocates
    /// additional partitions (and their state) as needed.
    fn allocate_additional_capacity(&mut self) {
        debug!("Allocating additional capacity");
        // Calculate total number of active partitions based on the amount of data stored
        let total_parts: u64;
        {
            let pa = self.partition_assignments.read().unwrap();
            let num_data_partitions = pa.data_partitions.len() as u64;
            let num_capacity_partitions =
                Self::get_num_capacity_partitions(num_data_partitions, &self.config);
            total_parts = num_capacity_partitions + num_data_partitions;
        }

        // Add additional capacity partitions as needed
        if total_parts > self.all_active_partitions.len() as u64 {
            let parts_to_add = total_parts - self.all_active_partitions.len() as u64;
            self.add_capacity_partitions(parts_to_add);
        }
    }

    /// Visits all of the slots in all of the ledgers and see if the need
    /// capacity partitions assigned to maintain their replica counts
    fn backfill_missing_partitions(&mut self) {
        debug!("Backfilling missing partitions...");
        // Start with a sorted list of capacity partitions (sorted by hash)
        let mut capacity_partitions: Vec<H256>;
        {
            let pa = self.partition_assignments.read().unwrap();
            capacity_partitions = pa.capacity_partitions.keys().copied().collect();
        }

        // Sort partitions using `sort_unstable` for better performance.
        // Stability isn't needed/affected as each partition hash is unique.
        capacity_partitions.sort_unstable();

        // Use the previous epoch hash as a seed/entropy to the prng
        let seed = self.last_epoch_hash.to_u32();
        let mut rng = SimpleRNG::new(seed);

        // Loop though all of the ledgers processing their slot needs
        for ledger in DataLedger::iter() {
            self.process_slot_needs(ledger, &mut capacity_partitions, &mut rng);
        }
    }

    /// Process slot needs for a given ledger, assigning partitions to each slot
    /// as needed.
    pub fn process_slot_needs(
        &mut self,
        ledger: DataLedger,
        capacity_partitions: &mut Vec<H256>,
        rng: &mut SimpleRNG,
    ) {
        debug!("Processing slot needs for ledger {:?}", &ledger);
        // Get slot needs for the specified ledger
        let slot_needs: Vec<(usize, usize)>;
        {
            let ledgers = self.ledgers.read().unwrap();
            slot_needs = ledgers.get_slot_needs(ledger);
        }
        let mut capacity_count: u32 = capacity_partitions
            .len()
            .try_into()
            .expect("Value exceeds u32::MAX");

        // Iterate over slots that need partitions and assign them
        for (slot_index, num_needed) in slot_needs {
            for _ in 0..num_needed {
                if capacity_count == 0 {
                    warn!(
                        "No available capacity partitions (needs {}) for slot {} of ledger {:?}",
                        &num_needed, &slot_index, &ledger
                    );
                    break; // Exit if no more available hashes
                }

                // Pick a random capacity partition hash and assign it
                let part_index = rng.next_range(capacity_count) as usize;
                let partition_hash = capacity_partitions.swap_remove(part_index);
                capacity_count -= 1;

                // Update local PartitionAssignment state and add to data_partitions
                self.assign_partition_to_slot(partition_hash, ledger, slot_index);

                // Push the newly assigned partition hash to the appropriate slot
                // in the ledger
                {
                    let mut ledgers = self.ledgers.write().unwrap();
                    debug!(
                        "Assigning partition hash {} to slot {} for  {:?}",
                        &partition_hash, &slot_index, &ledger
                    );

                    ledgers.push_partition_to_slot(ledger, slot_index, partition_hash);
                }
            }
        }
    }

    /// Computes active capacity partitions available for pledges based on
    /// data partitions and scaling factor
    pub fn get_num_capacity_partitions(
        num_data_partitions: u64,
        capacity_scalar: &ConsensusConfig,
    ) -> u64 {
        // Every ledger needs at least one slot filled with data partitions
        let min_count = DataLedger::ALL.len() as u64 * config.num_partitions_per_slot;
        let base_count = std::cmp::max(num_data_partitions, min_count);
        let log_10 = (base_count as f64).log10();
        let trunc = truncate_to_3_decimals(log_10);
        let scaled = truncate_to_3_decimals(trunc * config.epoch.capacity_scalar as f64);

        truncate_to_3_decimals(scaled).ceil() as u64
    }

    /// Adds new capacity partition hashes to the protocols pool of active partition hashes. This
    /// follows the process of sequentially hashing the previous partitions
    /// hash to compute the next partitions hash.
    fn add_capacity_partitions(&mut self, parts_to_add: u64) {
        let mut prev_partition_hash = *match self.all_active_partitions.last() {
            Some(last_hash) => last_hash,
            None => &self.last_epoch_hash,
        };

        debug!("Adding {} capacity partitions", &parts_to_add);
        // Compute the partition hashes for all of the added partitions
        for _i in 0..parts_to_add {
            let next_part_hash = H256(hash_sha256(&prev_partition_hash.0).unwrap());
            trace!(
                "Adding partition with hash: {} (prev: {})",
                next_part_hash.0.to_base58(),
                prev_partition_hash.0.to_base58()
            );
            self.all_active_partitions.push(next_part_hash);
            // All partition_hashes begin as unassigned capacity partitions
            self.unassigned_partitions.push(next_part_hash);
            prev_partition_hash = next_part_hash;
        }
    }

    // Updates PartitionAssignment information about a partition hash, marking
    // it as expired (or unassigned to a slot in a data ledger)
    fn return_expired_partition_to_capacity(&self, partition_hash: H256) {
        let mut pa = self.partition_assignments.write().unwrap();
        // Convert data partition to capacity partition if it exists
        if let Some(mut assignment) = pa.data_partitions.remove(&partition_hash) {
            {
                // Remove the partition hash from the slots state
                let ledger: DataLedger =
                    DataLedger::try_from(assignment.ledger_id.unwrap()).unwrap();
                let partition_hash = assignment.partition_hash;
                let slot_index = assignment.slot_index.unwrap();
                let mut write = self.ledgers.write().unwrap();
                write.remove_partition_from_slot(ledger, slot_index, &partition_hash);
            }

            // Clear ledger assignment
            assignment.ledger_id = None;
            assignment.slot_index = None;

            // Return the partition hash to the capacity pool
            pa.capacity_partitions.insert(partition_hash, assignment);
        }
    }

    /// Takes a capacity partition hash and updates its `PartitionAssignment`
    /// state to indicate it is part of a data ledger
    fn assign_partition_to_slot(
        &self,
        partition_hash: H256,
        ledger: DataLedger,
        slot_index: usize,
    ) {
        debug!(
            "Assigning partition {} to slot {} of ledger {:?}",
            &partition_hash.0.to_base58(),
            &slot_index,
            &ledger
        );
        let mut pa = self.partition_assignments.write().unwrap();
        if let Some(mut assignment) = pa.capacity_partitions.remove(&partition_hash) {
            assignment.ledger_id = Some(ledger as u32);
            assignment.slot_index = Some(slot_index);
            pa.data_partitions.insert(partition_hash, assignment);
        }
    }

    /// For a given ledger indicated by `Ledger`, calculate the number of
    /// partition slots to add to the ledger based on remaining capacity
    /// and data ingress this epoch
    fn calculate_additional_slots(
        &self,
        new_epoch_block: &IrysBlockHeader,
        ledger: DataLedger,
    ) -> u64 {
        let num_slots: u64;
        {
            let ledgers = self.ledgers.read().unwrap();
            let ledger = &ledgers[ledger];
            num_slots = ledger.slot_count() as u64;
        }
        let max_chunk_capacity = num_slots * self.num_chunks_in_partition;
        let ledger_size = new_epoch_block.data_ledgers[ledger].max_chunk_offset;

        // Add capacity slots if ledger usage exceeds 50% of partition size from max capacity
        let add_capacity_threshold =
            max_chunk_capacity.saturating_sub(self.num_chunks_in_partition / 2);
        let mut slots_to_add: u64 = 0;
        if ledger_size >= add_capacity_threshold {
            // Add 1 slot for buffer plus enough slots to handle size above threshold
            let excess = ledger_size.saturating_sub(max_chunk_capacity);
            slots_to_add = 1 + (excess / self.num_chunks_in_partition);

            // Check if we need to add an additional slot for excess > half of
            // the partition size
            if excess % self.num_chunks_in_partition >= self.num_chunks_in_partition / 2 {
                slots_to_add += 1;
            }
        }

        // Compute Data uploaded to the ledger last epoch
        if new_epoch_block.height >= self.config.num_blocks_in_epoch {
            let rg = self.block_index_guard.read();
            let previous_epoch_block_height: usize = (new_epoch_block.height
                - self.config.num_blocks_in_epoch)
                .try_into()
                .expect("Height is too large!");
            let last_epoch_block = rg.get_item(previous_epoch_block_height).expect(&format!(
                "Needed previous epoch block with height {} is not available in block index!",
                previous_epoch_block_height
            ));
            let data_added = ledger_size - last_epoch_block.ledgers[ledger].max_chunk_offset;
            slots_to_add += u64::div_ceil(data_added, self.num_chunks_in_partition);
        }

        slots_to_add
    }

    /// Computes the commitment state based on an epoch block and commitment transactions
    ///
    /// This function processes stake and pledge commitments to build a complete
    /// commitment state representation. It validates that all commitment references
    /// in the ledger have corresponding transaction data.
    ///
    /// TODO: Support unpledging and unstaking
    pub fn compute_commitment_state(&mut self, commitments: Vec<CommitmentTransaction>) {
        // Categorize commitments by their type for separate processing
        let mut stake_commitments: Vec<CommitmentTransaction> = Vec::new();
        let mut pledge_commitments: Vec<CommitmentTransaction> = Vec::new();
        for commitment_tx in commitments {
            match commitment_tx.commitment_type {
                irys_primitives::CommitmentType::Stake => stake_commitments.push(commitment_tx),
                irys_primitives::CommitmentType::Pledge => pledge_commitments.push(commitment_tx),
                _ => unimplemented!(),
            }
        }

        let mut commitment_state = self
            .commitment_state
            .write()
            .expect("to create a writeable commitment state");

        // Process stake commitments - these represent miners joining the network
        for stake_commitment in stake_commitments {
            // Register the commitment in the state
            // Assumption: Commitments are pre-validated, so we don't check for duplicates
            let value = CommitmentStateEntry {
                id: stake_commitment.id.into(),
                commitment_status: CommitmentStatus::Active,
                partition_hash: None,
                signer: stake_commitment.signer.clone(),
                // TODO: implement the staking cost lookups and use that value here
                amount: 0,
            };
            commitment_state
                .stake_commitments
                .insert(stake_commitment.signer, value);
        }

        // Process pledge commitments - miners committing resources to the network
        for pledge_commitment in pledge_commitments {
            let address = pledge_commitment.signer;

            // Skip pledges that don't have a corresponding active stake
            // This ensures only staked miners can make pledges
            if !commitment_state
                .stake_commitments
                .get(&address)
                .is_some_and(|c| c.commitment_status == CommitmentStatus::Active)
            {
                continue;
            } else {
                // TODO: Consider raising an error - this condition should have been caught in pre-validation
            }

            // Create the state entry for the pledge commitment
            let value = CommitmentStateEntry {
                id: pledge_commitment.id.into(),
                commitment_status: CommitmentStatus::Active,
                partition_hash: None,
                signer: pledge_commitment.signer,
                // TODO: implement the pledging cost lookups and use that value here
                amount: 0,
            };

            // Add the pledge state to the signer's collection (or create a new collection if first pledge)
            commitment_state
                .pledge_commitments
                .entry(address)
                .or_insert_with(Vec::new)
                .push(value);
        }
    }

    /// Assigns partition hashes to unassigned pledge commitments
    ///
    /// This function pairs unassigned partition hashes with active pledge commitments
    /// that have no partition hash assigned. It:
    ///
    /// 1. Takes partition hashes from self.unassigned_partitions
    /// 2. Assigns them to active pledges in commitment_state that need partitions
    /// 3. Updates PartitionAssignments to track the assignments
    /// 4. Removes assigned partitions from the unassigned_partitions list
    ///
    /// The assignment is deterministic, using sorted lists of both pledges and
    /// partition hashes to ensure consistent results.
    pub fn assign_partition_hashes_to_pledges(&mut self) {
        // Exit early if no partitions available
        if self.unassigned_partitions.is_empty() {
            return;
        }

        // Sort all the unassigned capacity partition_hashes
        let mut unassigned_partition_hashes = self.unassigned_partitions.clone();
        unassigned_partition_hashes.sort_unstable();
        let mut unassigned_parts: VecDeque<H256> = unassigned_partition_hashes.into();

        let mut commitment_state = self
            .commitment_state
            .write()
            .expect("to create writeable commitment state");

        // Make a list of all the active pledges with no assigned partition hash
        let mut unassigned_pledges: Vec<CommitmentStateEntry> = commitment_state
            .pledge_commitments
            .values()
            .flat_map(|entries| entries.iter())
            .filter(|entry| {
                entry.commitment_status == CommitmentStatus::Active
                    && entry.partition_hash.is_none()
            })
            .cloned()
            .collect();

        // Exit early if no unassigned pledges available
        if unassigned_pledges.is_empty() {
            return;
        }

        // Sort all the unassigned pledges by their ids, having a sorted list
        // of pledges and unassigned hashes leads to deterministic pledge assignment
        unassigned_pledges.sort_unstable_by(|a, b| a.id.cmp(&b.id));

        // Loop though both lists assigning capacity partitions to pledges
        for pledge in &unassigned_pledges {
            let partition_hash = unassigned_parts.pop_front().unwrap(); // Safe because we check isEmpty above

            // Initialize flag to track if we assigned a pledge commitment to a partition_hash
            let mut pledge_assigned = false;

            // Get the pledges for this signer
            if let Some(entries) = commitment_state.pledge_commitments.get_mut(&pledge.signer) {
                // Look for the matching pledge
                for entry in entries.iter_mut() {
                    if entry.id == pledge.id {
                        // Record the assigned partition hash in the pledge entry
                        entry.partition_hash = Some(partition_hash);
                        pledge_assigned = true;
                        break;
                    }
                }
            }

            // If the pledge commitment was updated, update partition assignments state to match
            if pledge_assigned {
                let mut pa = self.partition_assignments.write().unwrap();
                pa.capacity_partitions.insert(
                    partition_hash,
                    PartitionAssignment {
                        partition_hash,
                        miner_address: pledge.signer,
                        ledger_id: None,
                        slot_index: None,
                    },
                );
                // Remove the partition_hash from the unassigned partitions list
                self.unassigned_partitions
                    .retain(|&hash| hash != partition_hash);
            }
        }
    }

    /// Returns a vector of all partition assignments associated with the provided miner address.
    ///
    /// This function extracts assignments from both data and capacity partitions where
    /// the miner_address matches the input. Data partition assignments are added first,
    /// followed by capacity partition assignments. Within each category, the ordering is
    /// determined by the underlying BTreeMap implementation which orders entries by their
    /// partition hash keys, ensuring consistent and deterministic iteration order.
    ///
    /// # Arguments
    /// * `miner_address` - The address of the miner to get assignments for
    ///
    /// # Returns
    /// * `Vec<PartitionAssignment>` - A vector containing all matching partition assignments
    pub fn get_partition_assignments(&self, miner_address: Address) -> Vec<PartitionAssignment> {
        let mut assignments = Vec::new();

        // Get a read only view of the partition assignments
        let pa = self.partition_assignments.read().unwrap();

        // Filter the data ledgers and get assignments matching the miner_address
        let assigned_data_partitions: Vec<&PartitionAssignment> = pa
            .data_partitions
            .iter()
            .filter(|(_, ass)| ass.miner_address == miner_address)
            .map(|(_, ass)| ass)
            .collect();

        assignments.extend(assigned_data_partitions);

        let assigned_capacity_partitions: Vec<&PartitionAssignment> = pa
            .capacity_partitions
            .iter()
            .filter(|(_, ass)| ass.miner_address == miner_address)
            .map(|(_, ass)| ass)
            .collect();

        assignments.extend(assigned_capacity_partitions);

        assignments
    }

    /// Maps storage modules to partition assignments for the local node.
    ///
    /// This function creates [`StorageModuleInfo`] instances that link storage modules to specific
    /// partition assignments. It processes assignments in the following priority order:
    /// 1. Publish ledger partitions (first priority)
    /// 2. Submit ledger partitions (second priority)
    /// 3. Capacity partitions (used for remaining storage modules)
    ///
    /// The function respects the BTreeMap's deterministic ordering when processing assignments
    /// within each category, ensuring consistent mapping across node restarts.
    ///
    /// # Note
    /// This function has the same configuration dependency as [`system_ledger::get_genesis_commitments()`].
    /// When updating configuration related to StorageModule/submodule functionality, both functions
    /// will need corresponding updates.
    ///
    /// # Arguments
    /// * `storage_module_config` - Configuration containing paths for storage submodules
    ///
    /// # Returns
    /// * `Vec<StorageModuleInfo>` - Vector of storage module information with assigned partitions
    pub fn map_storage_modules_to_partition_assignments(
        &self,
        storage_module_config: StorageSubmodulesConfig,
    ) -> Vec<StorageModuleInfo> {
        let miner_address = self.miner_address;
        // Retrieve all partition assignments for the current miner
        let miner_assignments = self.get_partition_assignments(miner_address);

        let num_chunks_in_partition = self.num_chunks_in_partition as u32;
        let pa = self.partition_assignments.read().unwrap();
        let sm_paths = storage_module_config.submodule_paths;

        // STEP 1: Process Publish ledger partition assignments
        // Filter assignments to only include those for the Publish ledger
        let publish_assignments = miner_assignments
            .iter()
            .filter(|pa| pa.ledger_id == Some(DataLedger::Publish as u32));

        // Create StorageModuleInfo for each Publish ledger assignment
        let mut module_infos: Vec<StorageModuleInfo> = Vec::new();
        for (idx, assign) in publish_assignments.enumerate() {
            module_infos.push(StorageModuleInfo {
                id: idx,
                partition_assignment: Some(*assign),
                submodules: vec![(
                    partition_chunk_offset_ie!(0, num_chunks_in_partition),
                    sm_paths[idx].clone(),
                )],
            });
        }

        // Remember current index for Submit ledger assignments
        let idx_start = module_infos.len();

        // STEP 2: Process Submit ledger partition assignments
        // Filter assignments to only include those for the Submit ledger
        let submit_assignments = miner_assignments
            .iter()
            .filter(|pa| pa.ledger_id == Some(DataLedger::Submit as u32));

        for (idx, assign) in submit_assignments.enumerate() {
            module_infos.push(StorageModuleInfo {
                id: idx_start + idx,
                partition_assignment: Some(*assign),
                submodules: vec![(
                    partition_chunk_offset_ie!(0, num_chunks_in_partition),
                    sm_paths[idx_start + idx].clone(),
                )],
            });
        }

        // Exit early if no capacity partitions exist
        if pa.capacity_partitions.is_empty() {
            return module_infos;
        }

        // STEP 3: Process Capacity partition assignments (ledger_id == None)
        // Note: The order is preserved from the BTreeMap's natural ordering by partition_hash
        let capacity_assignments = miner_assignments.iter().filter(|pa| pa.ledger_id == None);

        // Populate remaining storage modules with capacity partition assignments
        // Only use as many capacity assignments as we have remaining storage modules
        let idx_start = module_infos.len();
        let modules_remaining = sm_paths.len() - module_infos.len();

        for (idx, assign) in capacity_assignments.enumerate() {
            // Stop if we've filled all available storage modules
            if idx == modules_remaining {
                break;
            }
            module_infos.push(StorageModuleInfo {
                id: idx_start + idx,
                partition_assignment: Some(*assign),
                submodules: vec![(
                    partition_chunk_offset_ie!(0, num_chunks_in_partition),
                    sm_paths[idx_start + idx].clone(),
                )],
            });
        }

        module_infos
    }
}

/// SHA256 hash the message parameter
fn hash_sha256(message: &[u8]) -> Result<[u8; 32], Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    let result = hasher.finish();
    Ok(result)
}

fn truncate_to_3_decimals(value: f64) -> f64 {
    (value * 1000.0).trunc() / 1000.0
}
