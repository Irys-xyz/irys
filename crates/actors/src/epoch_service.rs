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
    DatabaseProvider, IrysBlockHeader, SimpleRNG, StorageConfig, H256,
};
use irys_types::{
    partition_chunk_offset_ie, Address, CommitmentTransaction, IrysTransactionId,
    PartitionChunkOffset,
};
use irys_types::{Config, H256List};
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

/// Allows for overriding of the consensus parameters for ledgers and partitions
#[derive(Debug, Clone, Default)]
pub struct EpochServiceConfig {
    /// Capacity partitions are allocated on a logarithmic curve, this scalar
    /// shifts the curve on the Y axis. Allowing there to be more or less
    /// capacity partitions relative to data partitions.
    pub capacity_scalar: u64,
    /// The length of an epoch denominated in block heights
    pub num_blocks_in_epoch: u64,
    /// Sets the minimum number of capacity partitions for the protocol.
    pub num_capacity_partitions: Option<u64>,
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
}

impl EpochServiceConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            capacity_scalar: config.capacity_scalar,
            num_blocks_in_epoch: config.num_blocks_in_epoch,
            num_capacity_partitions: config.num_capacity_partitions,
            storage_config: StorageConfig::new(config),
        }
    }
}

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
struct CommitmentStateEntry {
    id: IrysTransactionId,
    commitment_status: CommitmentStatus,
    partition_hash: Option<H256>,
    signer: Address,
    /// Irys token amount in atomic units
    #[allow(dead_code)]
    amount: u64,
}

#[derive(Debug, Default)]
struct CommitmentState {
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
    pub config: EpochServiceConfig,
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// Computed commitment state
    commitment_state: CommitmentState,
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
// EpochServiceActor implementation
//------------------------------------------------------------------------------

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "Vec<StorageModuleInfo>")]
pub struct GetGenesisStorageModulesMessage(pub StorageSubmodulesConfig);

impl Handler<GetGenesisStorageModulesMessage> for EpochServiceActor {
    type Result = Vec<StorageModuleInfo>;

    fn handle(
        &mut self,
        msg: GetGenesisStorageModulesMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.map_storage_modules_to_partition_assignments(msg.0)
    }
}

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
        epoch_config: EpochServiceConfig,
        config: &Config,
        block_index_guard: BlockIndexReadGuard,
    ) -> Self {
        Self {
            last_epoch_hash: H256::zero(),
            ledgers: Arc::new(RwLock::new(Ledgers::new(config))),
            partition_assignments: Arc::new(RwLock::new(PartitionAssignments::new())),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            config: epoch_config,
            block_index_guard,
            commitment_state: Default::default(),
        }
    }

    pub async fn initialize(
        &mut self,
        db: &DatabaseProvider,
        storage_module_config: StorageSubmodulesConfig,
    ) -> eyre::Result<Vec<StorageModuleInfo>> {
        let mut block_index = 0;

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

                    let commitments_ledger = block_header
                        .system_ledgers
                        .iter()
                        .find(|b| b.ledger_id == SystemLedger::Commitment);

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
        let mut capacity_count = capacity_partitions.len() as u32;

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
    fn get_num_capacity_partitions(num_data_partitions: u64, config: &EpochServiceConfig) -> u64 {
        // Every ledger needs at least one slot filled with data partitions
        let min_count = DataLedger::ALL.len() as u64 * config.storage_config.num_partitions_in_slot;
        let base_count = std::cmp::max(num_data_partitions, min_count);
        let log_10 = (base_count as f64).log10();
        let trunc = truncate_to_3_decimals(log_10);
        let scaled = truncate_to_3_decimals(trunc * config.capacity_scalar as f64);

        truncate_to_3_decimals(scaled).ceil() as u64
    }

    /// Adds new capacity partition hashes to the protocols pool of active parition hashes. This
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
        let partition_chunk_count = self.config.storage_config.num_chunks_in_partition;
        let max_chunk_capacity = num_slots * partition_chunk_count;
        let ledger_size = new_epoch_block.data_ledgers[ledger].max_chunk_offset;

        // Add capacity slots if ledger usage exceeds 50% of partition size from max capacity
        let add_capacity_threshold = max_chunk_capacity.saturating_sub(partition_chunk_count / 2);
        let mut slots_to_add: u64 = 0;
        if ledger_size >= add_capacity_threshold {
            // Add 1 slot for buffer plus enough slots to handle size above threshold
            let excess = ledger_size.saturating_sub(max_chunk_capacity);
            slots_to_add = 1 + (excess / partition_chunk_count);

            // Check if we need to add an additional slot for excess > half of
            // the partition size
            if excess % partition_chunk_count >= partition_chunk_count / 2 {
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
            slots_to_add += u64::div_ceil(
                data_added,
                self.config.storage_config.num_chunks_in_partition,
            );
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
                _ => {} // Ignore any other commitment types
            }
        }

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
            self.commitment_state
                .stake_commitments
                .insert(stake_commitment.signer, value);
        }

        // Process pledge commitments - miners committing resources to the network
        for pledge_commitment in pledge_commitments {
            let address = pledge_commitment.signer;

            // Skip pledges that don't have a corresponding active stake
            // This ensures only staked miners can make pledges
            if !self
                .commitment_state
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
            self.commitment_state
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

        // Make a list of all the active pledges with no assigned partition hash
        let mut unassigned_pledges: Vec<CommitmentStateEntry> = self
            .commitment_state
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
            if let Some(entries) = self
                .commitment_state
                .pledge_commitments
                .get_mut(&pledge.signer)
            {
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
        let miner_address = self.config.storage_config.miner_address;
        // Retrieve all partition assignments for the current miner
        let miner_assignments = self.get_partition_assignments(miner_address);

        let num_chunks_in_partition = self.config.storage_config.num_chunks_in_partition as u32;
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

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use std::{any::Any, collections::VecDeque, sync::atomic::AtomicU64, time::Duration};

    use crate::block_index_service::{BlockIndexService, GetBlockIndexGuardMessage};
    use actix::{actors::mocker::Mocker, Addr, Arbiter, Recipient, SystemRegistry};
    use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
    use irys_config::IrysNodeConfig;
    use irys_database::{
        add_genesis_commitments, add_test_commitments, insert_commitment_tx, open_or_create_db,
        tables::IrysTables, BlockIndex, Initialized,
    };
    use irys_storage::{ie, StorageModule, StorageModuleVec};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{partition_chunk_offset_ie, Address, PartitionChunkRange};
    use tokio::time::sleep;
    use tracing::info;

    use crate::{
        mining::PartitionMiningActor,
        packing::{PackingActor, PackingRequest},
        BlockFinalizedMessage, BlockProducerMockActor, MockedBlockProducerAddr,
        SolutionFoundMessage,
    };
    use irys_vdf::vdf_state::{VdfState, VdfStepsReadGuard};

    use super::*;

    #[actix::test]
    async fn genesis_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        let testnet_config = Config::testnet();
        genesis_block.height = 0;
        let commitments = add_genesis_commitments(&mut genesis_block, &testnet_config);

        // Create epoch service with random miner address
        let config = EpochServiceConfig::new(&testnet_config);
        let arc_config = Arc::new(IrysNodeConfig::default());
        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let storage_config = StorageConfig::default();
        let block_index_actor =
            BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
        SystemRegistry::set(block_index_actor.clone());

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let mut epoch_service =
            EpochServiceActor::new(config.clone(), &testnet_config, block_index_guard);
        let miner_address = config.storage_config.miner_address;

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let _ = epoch_service.handle(
            NewEpochMessage {
                epoch_block: genesis_block.into(),
                commitments,
            },
            &mut Context::new(),
        );

        {
            // Verify the correct number of ledgers have been added
            let ledgers = epoch_service.ledgers.read().unwrap();
            let expected_ledger_count = DataLedger::ALL.len();
            assert_eq!(ledgers.len(), expected_ledger_count);

            // Verify each ledger has one slot and the correct number of partitions
            let pub_slots = ledgers.get_slots(DataLedger::Publish);
            let sub_slots = ledgers.get_slots(DataLedger::Submit);

            assert_eq!(pub_slots.len(), 1);
            assert_eq!(sub_slots.len(), 1);

            assert_eq!(
                pub_slots[0].partitions.len() as u64,
                config.storage_config.num_partitions_in_slot
            );
            assert_eq!(
                sub_slots[0].partitions.len() as u64,
                config.storage_config.num_partitions_in_slot
            );

            // Verify data partition assignments match _PUBLISH_ ledger slots
            for (slot_idx, slot) in pub_slots.iter().enumerate() {
                let pa = epoch_service.partition_assignments.read().unwrap();
                for &partition_hash in &slot.partitions {
                    let assignment = pa
                        .data_partitions
                        .get(&partition_hash)
                        .expect("partition should be assigned");

                    assert_eq!(
                        assignment,
                        &PartitionAssignment {
                            partition_hash,
                            ledger_id: Some(DataLedger::Publish.into()),
                            slot_index: Some(slot_idx),
                            miner_address,
                        }
                    );
                }
                assert_eq!(
                    slot.partitions.len(),
                    config.storage_config.num_partitions_in_slot as usize
                );
            }

            // Verify data partition assignments match _SUBMIT_ledger slots
            for (slot_idx, slot) in sub_slots.iter().enumerate() {
                let pa = epoch_service.partition_assignments.read().unwrap();
                for &partition_hash in &slot.partitions {
                    let assignment = pa
                        .data_partitions
                        .get(&partition_hash)
                        .expect("partition should be assigned");

                    assert_eq!(
                        assignment,
                        &PartitionAssignment {
                            partition_hash,
                            ledger_id: Some(DataLedger::Submit.into()),
                            slot_index: Some(slot_idx),
                            miner_address,
                        }
                    );
                }
                assert_eq!(
                    slot.partitions.len(),
                    config.storage_config.num_partitions_in_slot as usize
                );
            }
        }

        // Verify the correct number of genesis partitions have been activated
        {
            let pa = epoch_service.partition_assignments.read().unwrap();
            let data_partition_count = pa.data_partitions.len() as u64;
            let expected_partitions = data_partition_count
                + EpochServiceActor::get_num_capacity_partitions(data_partition_count, &config);
            assert_eq!(
                epoch_service.all_active_partitions.len(),
                expected_partitions as usize
            );

            // Validate that all the capacity partitions are assigned to the
            // bootstrap miner but not assigned to any ledger
            for pair in &pa.capacity_partitions {
                let partition_hash = pair.0;
                let ass = pair.1;
                assert_eq!(
                    ass,
                    &PartitionAssignment {
                        partition_hash: *partition_hash,
                        ledger_id: None,
                        slot_index: None,
                        miner_address
                    }
                )
            }
        }

        // Debug output for verification
        // println!("Data Partitions: {:#?}", epoch_service.capacity_partitions);
        println!("Ledger State: {:#?}", epoch_service.ledgers);

        let ledgers = epoch_service.handle(GetLedgersGuardMessage, &mut Context::new());

        println!("{:?}", ledgers.read());

        // let infos = epoch_service.get_genesis_storage_module_infos();
        // println!("{:#?}", infos);
    }

    #[actix::test]
    async fn add_slots_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        let testnet_config = Config::testnet();
        genesis_block.height = 0;

        let commitments = add_genesis_commitments(&mut genesis_block, &testnet_config);

        // Create a storage config for testing
        let storage_config = StorageConfig {
            chunk_size: 32,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: Address::random(),
            min_writes_before_sync: 1,
            entropy_packing_iterations: testnet_config.entropy_packing_iterations,
            chunk_migration_depth: 1, // Testnet / single node config
            chain_id: 333,
        };
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;
        let tmp_dir = setup_tracing_and_temp_dir(Some("add_slots_test"), false);
        let base_path = tmp_dir.path().to_path_buf();

        let config = EpochServiceConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            num_capacity_partitions: Some(123),
            storage_config: storage_config.clone(),
        };
        let num_blocks_in_epoch = config.num_blocks_in_epoch;

        let arc_config = Arc::new(IrysNodeConfig {
            base_directory: base_path.clone(),
            ..IrysNodeConfig::default()
        });

        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let block_index_actor = BlockIndexService::new(block_index.clone(), storage_config).start();
        SystemRegistry::set(block_index_actor.clone());

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let mut epoch_service = EpochServiceActor::new(config, &testnet_config, block_index_guard);

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let mut ctx = Context::new();
        let _ = epoch_service.handle(
            NewEpochMessage {
                epoch_block: genesis_block.clone().into(),
                commitments,
            },
            &mut ctx,
        );

        let msg = BlockFinalizedMessage {
            block_header: Arc::new(genesis_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

        // index epoch previous blocks
        let mut height = 1;
        while height < num_blocks_in_epoch {
            new_epoch_block.height = height;
            let msg = BlockFinalizedMessage {
                block_header: Arc::new(new_epoch_block.clone()),
                all_txs: Arc::new(vec![]),
            };
            match block_index_actor.send(msg).await {
                Ok(_) => debug!("block indexed"),
                Err(err) => panic!("Failed to index block {:?}", err),
            }

            height += 1;
        }

        new_epoch_block.height = num_blocks_in_epoch;
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
            num_chunks_in_partition / 2;

        let _ = epoch_service.handle(
            NewEpochMessage {
                epoch_block: new_epoch_block.clone().into(),
                commitments: Vec::new(),
            },
            &mut ctx,
        );

        // Verify each ledger has one slot and the correct number of partitions
        {
            let ledgers = epoch_service.ledgers.read().unwrap();
            let pub_slots = ledgers.get_slots(DataLedger::Publish);
            let sub_slots = ledgers.get_slots(DataLedger::Submit);
            assert_eq!(pub_slots.len(), 1);
            assert_eq!(sub_slots.len(), 3); // TODO: check 1 expired, 2 new slots added
        }

        // index epoch block up to 2 * num_blocks_in_epoch
        while height < 2 * num_blocks_in_epoch {
            new_epoch_block.height = height;
            let msg = BlockFinalizedMessage {
                block_header: Arc::new(new_epoch_block.clone()),
                all_txs: Arc::new(vec![]),
            };
            match block_index_actor.send(msg).await {
                Ok(_) => debug!("block indexed"),
                Err(_) => panic!("Failed to index block"),
            }

            height += 1;
        }

        // Simulate a subsequent epoch block that adds multiple ledger slots
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.height = num_blocks_in_epoch * 2;
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
            (num_chunks_in_partition as f64 * 2.5) as u64;
        new_epoch_block.data_ledgers[DataLedger::Publish as usize].max_chunk_offset =
            (num_chunks_in_partition as f64 * 0.75) as u64;

        let _ = epoch_service.handle(
            NewEpochMessage {
                epoch_block: new_epoch_block.clone().into(),
                commitments: Vec::new(),
            },
            &mut ctx,
        );

        // Validate the correct number of ledgers slots were added to each ledger
        {
            let ledgers = epoch_service.ledgers.read().unwrap();
            let pub_slots = ledgers.get_slots(DataLedger::Publish);
            let sub_slots = ledgers.get_slots(DataLedger::Submit);
            assert_eq!(pub_slots.len(), 3);
            assert_eq!(sub_slots.len(), 7);
            println!("Ledger State: {:#?}", ledgers);
        }
    }

    #[actix::test]
    async fn capacity_projection_tests() {
        let max_data_parts = 1000;
        let config = EpochServiceConfig::default();
        for i in (0..max_data_parts).step_by(10) {
            let data_partition_count = i;
            let capacity_count =
                EpochServiceActor::get_num_capacity_partitions(data_partition_count, &config);
            let total = data_partition_count + capacity_count;
            println!(
                "data:{}, capacity:{}, total:{}",
                data_partition_count, capacity_count, total
            );
        }
    }

    #[actix::test]
    async fn partition_expiration_test() {
        // Initialize genesis block at height 0
        let chunk_size = 32;
        let chunk_count = 10;
        let testnet_config = Config {
            chunk_size,
            num_chunks_in_partition: chunk_count,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            num_writes_before_sync: 1,
            chunk_migration_depth: 1,
            capacity_scalar: 100,
            submit_ledger_epoch_length: 2,
            num_blocks_in_epoch: 5,
            ..Config::testnet()
        };
        let mining_address = testnet_config.miner_address();

        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;
        let commitments = add_test_commitments(&mut genesis_block, 5, &testnet_config);

        // Create a storage config for testing
        let storage_config = StorageConfig::new(&testnet_config);
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;
        let tmp_dir = setup_tracing_and_temp_dir(Some("partition_expiration_test"), false);
        let base_path = tmp_dir.path().to_path_buf();

        let num_blocks_in_epoch = testnet_config.num_blocks_in_epoch;

        // Create epoch service
        let config = EpochServiceConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: num_blocks_in_epoch,
            num_capacity_partitions: Some(123),
            storage_config: storage_config.clone(),
        };

        let arc_config = Arc::new(IrysNodeConfig::default());
        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let block_index_actor =
            BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
        SystemRegistry::set(block_index_actor.clone());

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let epoch_service = EpochServiceActor::new(config, &testnet_config, block_index_guard);
        let epoch_service_actor = epoch_service.start();

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let _ = epoch_service_actor
            .send(NewEpochMessage {
                epoch_block: genesis_block.into(),
                commitments,
            })
            .await
            .unwrap();

        let storage_module_config = StorageSubmodulesConfig::load(base_path.clone()).unwrap();
        // Get the genesis storage modules and their assigned partitions
        let storage_module_infos = epoch_service_actor
            .send(GetGenesisStorageModulesMessage(storage_module_config))
            .await
            .unwrap();

        let mut storage_modules: StorageModuleVec = Vec::new();
        // Create a list of storage modules wrapping the storage files
        for info in storage_module_infos {
            let arc_module = Arc::new(
                StorageModule::new(&base_path, &info, storage_config.clone())
                    // TODO: remove this unwrap
                    .unwrap(),
            );
            storage_modules.push(arc_module.clone());
        }

        let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
        let database_provider = DatabaseProvider(Arc::new(db));

        let rwlock: RwLock<Option<PackingRequest>> = RwLock::new(None);
        let arc_rwlock = Arc::new(rwlock);
        let closure_arc = arc_rwlock.clone();

        let mocked_block_producer = BlockProducerMockActor::mock(Box::new(move |_msg, _ctx| {
            let inner_result: eyre::Result<
                Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>,
            > = Ok(None);
            Box::new(Some(inner_result)) as Box<dyn Any>
        }));

        let block_producer_actor_addr: Addr<BlockProducerMockActor> = mocked_block_producer.start();
        let recipient: Recipient<SolutionFoundMessage> = block_producer_actor_addr.recipient();
        let mocked_addr = MockedBlockProducerAddr(recipient);

        let packing = Mocker::<PackingActor>::mock(Box::new(move |msg, _ctx| {
            let packing_req = *msg.downcast::<PackingRequest>().unwrap();
            debug!("Packing request arrived ...");

            {
                let mut lck = closure_arc.write().unwrap();
                lck.replace(packing_req);
            }

            debug!("Packing request result pushed ...");
            Box::new(Some(())) as Box<dyn Any>
        }));

        let vdf_steps_guard = VdfStepsReadGuard::new(Arc::new(RwLock::new(VdfState {
            max_seeds_num: 10,
            global_step: 0,
            seeds: VecDeque::new(),
        })));

        let packing_addr = packing.start();
        let mut part_actors = Vec::new();

        let atomic_global_step_number = Arc::new(AtomicU64::new(0));

        for sm in &storage_modules {
            let partition_mining_actor = PartitionMiningActor::new(
                mining_address,
                database_provider.clone(),
                mocked_addr.0.clone(),
                packing_addr.clone().recipient(),
                sm.clone(),
                true, // do not start mining automatically
                vdf_steps_guard.clone(),
                atomic_global_step_number.clone(),
            );

            let part_arbiter = Arbiter::new();
            let partition_address =
                PartitionMiningActor::start_in_arbiter(&part_arbiter.handle(), |_| {
                    partition_mining_actor
                });
            debug!("starting miner partition hash {:?}", sm.partition_hash());
            part_actors.push(partition_address);
        }

        let assign_submit_partition_hash = {
            let partition_assignments_read = epoch_service_actor
                .send(GetPartitionAssignmentsGuardMessage)
                .await
                .unwrap();

            let partition_hash = partition_assignments_read
                .read()
                .data_partitions
                .iter()
                .find(|(_hash, assignment)| {
                    assignment.ledger_id == Some(DataLedger::Submit.get_id())
                })
                .map(|(hash, _)| hash.clone())
                .expect("There should be a partition assigned to submit ledger");

            partition_hash
        };

        let (publish_partition_hash, submit_partition_hash) = {
            let ledgers = epoch_service_actor
                .send(GetLedgersGuardMessage)
                .await
                .unwrap();

            let pub_slots = ledgers.read().get_slots(DataLedger::Publish).clone();
            let sub_slots = ledgers.read().get_slots(DataLedger::Submit).clone();
            assert_eq!(pub_slots.len(), 1);
            assert_eq!(sub_slots.len(), 1);

            (pub_slots[0].partitions[0], sub_slots[0].partitions[0])
        };

        assert_eq!(assign_submit_partition_hash, submit_partition_hash);

        let capacity_partitions = {
            let partition_assignments_read = epoch_service_actor
                .send(GetPartitionAssignmentsGuardMessage)
                .await
                .unwrap();

            let capacity_partitions: Vec<H256> = partition_assignments_read
                .read()
                .capacity_partitions
                .keys()
                .map(|partition| partition.clone())
                .collect();

            assert!(
                !capacity_partitions.contains(&publish_partition_hash),
                "Publish partition should not be in capacity partitions"
            );

            assert!(
                !capacity_partitions.contains(&submit_partition_hash),
                "Submit partition should not be in capacity partitions"
            );

            capacity_partitions
        };

        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;

        let msg = BlockFinalizedMessage {
            block_header: Arc::new(genesis_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

        // index epoch previous blocks
        let mut height = 1;
        while height < (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch {
            new_epoch_block.height = height;
            let msg = BlockFinalizedMessage {
                block_header: Arc::new(new_epoch_block.clone()),
                all_txs: Arc::new(vec![]),
            };
            match block_index_actor.send(msg).await {
                Ok(_) => debug!("block indexed {}", height),
                Err(_) => panic!("Failed to index block {}", height),
            }

            height += 1;
        }

        new_epoch_block.height =
            (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
            num_chunks_in_partition / 2;

        let _ = epoch_service_actor
            .send(NewEpochMessage {
                epoch_block: new_epoch_block.into(),
                commitments: Vec::new(),
            })
            .await
            .unwrap();

        // busypoll the solution context rwlock
        let mut max_pools = 10;
        let pack_req = 'outer: loop {
            if max_pools == 0 {
                panic!("Max. retries reached");
            } else {
                max_pools -= 1;
            }
            match arc_rwlock.try_read() {
                Ok(lck) => {
                    if lck.is_none() {
                        debug!("Packing request not ready waiting!");
                    } else {
                        debug!("Packing request received ready!");
                        break 'outer lck.as_ref().unwrap().clone();
                    }
                }
                Err(err) => {
                    debug!("Packing request read error {:?}", err);
                }
            }
            sleep(Duration::from_millis(50)).await;
        };

        // check a new slots is inserted with a partition assigned to it, and slot 0 expired and its partition was removed
        let (publish_partition, submit_partition, submit_partition2) = {
            let ledgers = epoch_service_actor
                .send(GetLedgersGuardMessage)
                .await
                .unwrap();

            let pub_slots = ledgers.read().get_slots(DataLedger::Publish).clone();
            let sub_slots = ledgers.read().get_slots(DataLedger::Submit).clone();
            assert_eq!(
                pub_slots.len(),
                1,
                "Publish should still have only one slot"
            );
            debug!("Ledger State: {:#?}", ledgers);

            assert_eq!(sub_slots.len(), 4, "Submit slots should have two new not expired slots with a new fresh partition from available previous capacity ones!");
            assert!(
                sub_slots[0].is_expired && sub_slots[0].partitions.len() == 0,
                "Slot 0 should have expired and have no assigned partition!"
            );

            assert!(
                !sub_slots[1].is_expired
                    && sub_slots[1].partitions.len() == 1
                    && (capacity_partitions.contains(&sub_slots[1].partitions[0])
                        || submit_partition_hash == sub_slots[1].partitions[0]),
                "Slot 1 should not be expired and have a capacity or the just expired partition"
            );
            assert!(
                !sub_slots[2].is_expired
                    && sub_slots[2].partitions.len() == 1
                    && (capacity_partitions.contains(&sub_slots[2].partitions[0])
                        || submit_partition_hash == sub_slots[2].partitions[0]),
                "Slot 2 should not be expired and have a capacity or the just expired partition"
            );

            println!("{}", serde_json::to_string_pretty(&sub_slots).unwrap());

            assert!(
                !sub_slots[3].is_expired
                    && sub_slots[3].partitions.len() == 1
                    && (capacity_partitions.contains(&sub_slots[3].partitions[0])
                        || submit_partition_hash == sub_slots[3].partitions[0]),
                "Slot 3 should not be expired have a capacity or the just expired partition"
            );

            let publish_partition = pub_slots[0]
                .partitions
                .get(0)
                .expect("publish ledger slot 0 should have a partition assigned")
                .clone();
            let submit_partition = sub_slots[1]
                .partitions
                .get(0)
                .expect("submit ledger slot 1 should have a partition assigned")
                .clone();
            let submit_partition2 = sub_slots[2]
                .partitions
                .get(0)
                .expect("submit ledger slot 2 should have a partition assigned")
                .clone();

            (publish_partition, submit_partition, submit_partition2)
        };

        // check repacking request expired partition for its whole interval range, and partitions assignments are consistent
        {
            let partition_assignments_read = epoch_service_actor
                .send(GetPartitionAssignmentsGuardMessage)
                .await
                .unwrap();

            assert_eq!(
                partition_assignments_read.read().data_partitions.len(),
                4,
                "Should have four partitions assignments"
            );

            if let Some(publish_assignment) = partition_assignments_read
                .read()
                .data_partitions
                .get(&publish_partition)
            {
                assert_eq!(
                    publish_assignment.ledger_id,
                    Some(DataLedger::Publish.get_id()),
                    "Should be assigned to publish ledger"
                );
                assert_eq!(
                    publish_assignment.slot_index,
                    Some(0),
                    "Should be assigned to slot 0"
                );
            } else {
                panic!("Should have an assignment");
            };

            if let Some(submit_assignment) = partition_assignments_read
                .read()
                .data_partitions
                .get(&submit_partition)
            {
                assert_eq!(
                    submit_assignment.ledger_id,
                    Some(DataLedger::Submit.get_id()),
                    "Should be assigned to submit ledger"
                );
                assert_eq!(
                    submit_assignment.slot_index,
                    Some(1),
                    "Should be assigned to slot 1"
                );
            } else {
                panic!("Should have an assignment");
            };

            if let Some(submit_assignment) = partition_assignments_read
                .read()
                .data_partitions
                .get(&submit_partition2)
            {
                assert_eq!(
                    submit_assignment.ledger_id,
                    Some(DataLedger::Submit.get_id()),
                    "Should be assigned to submit ledger"
                );
                assert_eq!(
                    submit_assignment.slot_index,
                    Some(2),
                    "Should be assigned to slot 2"
                );
            } else {
                panic!("Should have an assignment");
            };
        }

        assert_eq!(
            pack_req.storage_module.partition_hash(),
            Some(submit_partition_hash),
            "Partition hashes should be equal"
        );
        assert_eq!(
            pack_req.chunk_range,
            PartitionChunkRange(partition_chunk_offset_ie!(0, chunk_count as u32)),
            "The whole partition should be repacked"
        );
    }

    #[actix::test]
    async fn epoch_blocks_reinitialization_test() {
        let testnet_config = Config {
            chunk_size: 32,
            ..Config::testnet()
        };

        // Create a storage config for testing
        let storage_config = StorageConfig {
            chunk_size: testnet_config.chunk_size,
            num_chunks_in_partition: testnet_config.num_chunks_in_partition,
            num_chunks_in_recall_range: testnet_config.num_chunks_in_recall_range,
            num_partitions_in_slot: testnet_config.num_partitions_per_slot,
            miner_address: Address::random(),
            min_writes_before_sync: testnet_config.num_writes_before_sync,
            entropy_packing_iterations: testnet_config.entropy_packing_iterations,
            chunk_migration_depth: testnet_config.chunk_migration_depth, // Testnet / single node config
            chain_id: testnet_config.chain_id,
        };
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;
        let tmp_dir = setup_tracing_and_temp_dir(Some("epoch_block_reinitialization_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let storage_module_config = StorageSubmodulesConfig::load(base_path.clone()).unwrap();

        let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
        let database_provider = DatabaseProvider(Arc::new(db));

        let config = EpochServiceConfig {
            capacity_scalar: testnet_config.capacity_scalar,
            num_blocks_in_epoch: testnet_config.num_blocks_in_epoch,
            num_capacity_partitions: Some(200),
            storage_config: storage_config.clone(),
        };
        let num_blocks_in_epoch = config.num_blocks_in_epoch;

        let arc_config = Arc::new(IrysNodeConfig {
            base_directory: base_path.clone(),
            ..IrysNodeConfig::default()
        });

        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let block_index_actor =
            BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
        SystemRegistry::set(block_index_actor.clone());

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let mut epoch_service =
            EpochServiceActor::new(config.clone(), &testnet_config, block_index_guard.clone());

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let mut ctx = Context::new();
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;
        let pledge_count = config.num_capacity_partitions.unwrap_or(31) as u8;
        let commitments = add_test_commitments(&mut genesis_block, pledge_count, &testnet_config);
        database_provider
            .update_eyre(|tx| {
                commitments
                    .iter()
                    .map(|commitment| insert_commitment_tx(tx, commitment))
                    .collect::<eyre::Result<()>>()
            })
            .expect("inserting commitment tx should succeed");

        database_provider
            .update_eyre(|tx| irys_database::insert_block_header(tx, &genesis_block))
            .unwrap();

        // Get the genesis storage modules and their assigned partitions
        let storage_module_infos = epoch_service
            .initialize(&database_provider, storage_module_config.clone())
            .await
            .unwrap(); // epoch_service.handle(GetGenesisStorageModulesMessage(storage_module_config.clone()), &mut ctx);
        debug!("{:#?}", storage_module_infos);

        genesis_block.block_hash = H256::from_slice(&[0; 32]);
        let _ = epoch_service.handle(
            NewEpochMessage {
                epoch_block: genesis_block.clone().into(),
                commitments,
            },
            &mut ctx,
        );

        let pa_read_guard = epoch_service.handle(GetPartitionAssignmentsGuardMessage, &mut ctx);

        // database_provider
        //     .update_eyre(|tx| irys_database::insert_block_header(tx, &genesis_block))
        //     .unwrap();

        let msg = BlockFinalizedMessage {
            block_header: Arc::new(genesis_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        {
            let mut storage_modules: StorageModuleVec = Vec::new();

            // Create a list of storage modules wrapping the storage files
            for info in storage_module_infos {
                let arc_module = Arc::new(
                    StorageModule::new(
                        &arc_config.storage_module_dir(),
                        &info,
                        storage_config.clone(),
                    )
                    .unwrap(),
                );
                storage_modules.push(arc_module.clone());
            }
        }

        //         +---+
        //         |sm0|
        //         +-+-+  |    |
        // Publish 0----+----+----+---
        //           |    |    |
        //           0    1    2
        //         +---+
        //         |sm1|
        //         +-+-+  |    |
        // Submit  1----+----+----+---
        //           |    |    |
        //           0    1    2
        // Capacity +---+
        //          |sm2|
        //          +-+-+

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

        // index and store in db blocks
        let mut height = 1;
        while height <= (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch {
            new_epoch_block.height = height;
            new_epoch_block.block_hash = H256::random();

            if height == (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch {
                new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
                    num_chunks_in_partition / 2;
            }

            let msg = BlockFinalizedMessage {
                block_header: Arc::new(new_epoch_block.clone()),
                all_txs: Arc::new(vec![]),
            };
            match block_index_actor.send(msg).await {
                Ok(_) => (), // debug!("block indexed"),
                Err(err) => panic!("Failed to index block {:?}", err),
            }

            if height % num_blocks_in_epoch == 0 {
                debug!("epoch block {}", height);
                let _ = epoch_service.handle(
                    NewEpochMessage {
                        epoch_block: new_epoch_block.clone().into(),
                        commitments: Vec::new(),
                    },
                    &mut ctx,
                );
            }

            database_provider
                .update_eyre(|tx| irys_database::insert_block_header(tx, &new_epoch_block))
                .unwrap();
            height += 1;
        }

        // Verify each ledger has one slot and the correct number of partitions
        {
            let ledgers = epoch_service.ledgers.read().unwrap();
            debug!("{:#?}", ledgers);
            let pub_slots = ledgers.get_slots(DataLedger::Publish);
            let sub_slots = ledgers.get_slots(DataLedger::Submit);
            assert_eq!(pub_slots.len(), 1);
            assert_eq!(sub_slots.len(), 5); // TODO: check slot 2 expired, 3 new slots added
        }

        //            +---+
        //            |sm0|
        //            +-|-+  |    |
        // Publish 0----+----+----+---
        //              |    |    |
        //              0    1    2
        //                  +---+ +---+ +---+
        //                  |sm2| |sm1| | ? |
        //                  +-|-+ +-|-+ +-|-+
        // Submit 1 +----+----+-----+-----+----+---
        //          |    |    |     |     |
        //          0    1    2     3     4
        // Capacity

        pa_read_guard.read().print_assignments();

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        debug!(
            "num blocks in block_index: {}",
            block_index_guard.read().num_blocks()
        );

        // Get the genesis storage modules and their assigned partitions
        let mut epoch_service = EpochServiceActor::new(config, &testnet_config, block_index_guard);
        let storage_module_infos = epoch_service
            .initialize(&database_provider, storage_module_config.clone())
            .await
            .unwrap();
        debug!("{:#?}", storage_module_infos);

        // Check partition hashes have not changed in storage modules
        {
            let mut storage_modules: StorageModuleVec = Vec::new();

            // Create a list of storage modules wrapping the storage files
            for info in storage_module_infos {
                let arc_module = Arc::new(
                    StorageModule::new(
                        &arc_config.storage_module_dir(),
                        &info,
                        storage_config.clone(),
                    )
                    // TODO: remove this unwrap
                    .unwrap(),
                );
                storage_modules.push(arc_module.clone());
            }
        }
    }

    #[actix::test]
    async fn partitions_assignment_determinism_test() {
        let testnet_config = Config {
            submit_ledger_epoch_length: 2,
            ..Config::testnet()
        };
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.last_epoch_hash = H256::zero(); // for partitions hash determinism
        genesis_block.height = 0;
        let pledge_count = 20;
        let commitments = add_test_commitments(&mut genesis_block, pledge_count, &testnet_config);

        // TODO: need a test method that pledges X partitions regardless of the storage config

        // Create a storage config for testing
        let storage_config = StorageConfig {
            chunk_size: 32,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: testnet_config.miner_address(),
            min_writes_before_sync: 1,
            entropy_packing_iterations: testnet_config.entropy_packing_iterations,
            chunk_migration_depth: 1, // Testnet / single node config
            chain_id: 1,
        };
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;

        // Create epoch service
        let config = EpochServiceConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            num_capacity_partitions: None,
            storage_config: storage_config.clone(),
        };
        let num_blocks_in_epoch = config.num_blocks_in_epoch;

        let tmp_dir = setup_tracing_and_temp_dir(Some("epoch_block_reinitialization_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let arc_config = Arc::new(IrysNodeConfig {
            base_directory: base_path.clone(),
            ..IrysNodeConfig::default()
        });

        let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
            BlockIndex::default()
                .reset(&arc_config.clone())
                .unwrap()
                .init(arc_config.clone())
                .await
                .unwrap(),
        ));

        let block_index_actor =
            BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
        SystemRegistry::set(block_index_actor.clone());

        let block_index_guard = block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .unwrap();

        let mut epoch_service =
            EpochServiceActor::new(config.clone(), &testnet_config, block_index_guard.clone());
        let mut ctx = Context::new();
        let _ = epoch_service.handle(
            NewEpochMessage {
                epoch_block: genesis_block.clone().into(),
                commitments,
            },
            &mut ctx,
        );

        let pa_read_guard = epoch_service.handle(GetPartitionAssignmentsGuardMessage, &mut ctx);
        pa_read_guard.read().print_assignments();

        let msg = BlockFinalizedMessage {
            block_header: Arc::new(genesis_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let total_epoch_messages = 6;
        let mut epoch_num = 1;
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = num_chunks_in_partition;
        new_epoch_block.data_ledgers[DataLedger::Publish].max_chunk_offset =
            num_chunks_in_partition;

        let mut height = 1;
        while epoch_num <= total_epoch_messages {
            new_epoch_block.height = height;
            //(testnet_config.submit_ledger_epoch_length * epoch_num) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
            let msg = BlockFinalizedMessage {
                block_header: Arc::new(new_epoch_block.clone()),
                all_txs: Arc::new(vec![]),
            };
            match block_index_actor.send(msg).await {
                Ok(_) => (), // debug!("block indexed"),
                Err(err) => panic!("Failed to index block {:?}", err),
            }

            if height % num_blocks_in_epoch == 0 {
                epoch_num += 1;
                debug!("epoch block {}", height);
                let _ = epoch_service.handle(
                    NewEpochMessage {
                        epoch_block: new_epoch_block.clone().into(),
                        commitments: Vec::new(),
                    },
                    &mut ctx,
                );
            }
            height += 1;
        }

        // pa_read_guard.read().print_assignments();
        // debug!(
        //     "\nAll Partitions({})\n{}",
        //     &epoch_service.all_active_partitions.len(),
        //     serde_json::to_string_pretty(&epoch_service.all_active_partitions).unwrap()
        // );

        // Check determinism in assigned partitions
        let publish_slot_0 = H256::from_base58("2F5eg8FE2VmXGcgpyUKTzBrLzSmVXMKqawUJeDgKC1vW");
        debug!("expected publish[0] -> {}", publish_slot_0.0.to_base58());

        if let Some(publish_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&publish_slot_0)
        {
            assert_eq!(
                publish_assignment.ledger_id,
                Some(DataLedger::Publish.get_id()),
                "Should be assigned to publish ledger"
            );
            assert_eq!(
                publish_assignment.slot_index,
                Some(0),
                "Should be assigned to slot 0"
            );
        } else {
            panic!("Should have an assignment");
        };

        let publish_slot_1 = H256::from_base58("2HVmW86qVyKTw1DYJMX6NoNvVxATLNZHSAyMceEWPtLC");
        debug!("expected publish[1] -> {}", publish_slot_1.0.to_base58());

        if let Some(publish_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&publish_slot_1)
        {
            assert_eq!(
                publish_assignment.ledger_id,
                Some(DataLedger::Publish.get_id()),
                "Should be assigned to publish ledger"
            );
            assert_eq!(
                publish_assignment.slot_index,
                Some(1),
                "Should be assigned to slot 1"
            );
        } else {
            panic!("Should have an assignment");
        };

        let capacity_partition = H256::from_base58("5Wvv6erYhpk9aAzdrS9i6noQf57dBXHgLaMz46mNZeds");

        if let Some(capacity_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .capacity_partitions
            .get(&capacity_partition)
        {
            assert_eq!(
                capacity_assignment.ledger_id, None,
                "Should not be assigned to data ledger"
            );
            assert_eq!(
                capacity_assignment.slot_index, None,
                "Should not be assigned a slot index"
            );
        } else {
            panic!("Should have an assignment");
        };

        let submit_slot_15 = H256::from_base58("AtGjnuZ1EKmp8sP1FT3aMPbJWCY8tdEoksVFDB6PKXr1");

        if let Some(submit_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&submit_slot_15)
        {
            assert_eq!(
                submit_assignment.ledger_id,
                Some(DataLedger::Submit.get_id()),
                "Should be assigned to submit ledger"
            );
            assert_eq!(
                submit_assignment.slot_index,
                Some(15),
                "Should be assigned to slot 15"
            );
        } else {
            panic!("Should have an assignment");
        };
    }
}
