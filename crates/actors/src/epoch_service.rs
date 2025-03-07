use actix::SystemService;
use actix::{Actor, Context, Handler, Message, MessageResponse};
use base58::ToBase58;
use eyre::{Error, Result};
use irys_config::StorageSubmodulesConfig;
use irys_database::{block_header_by_hash, data_ledger::*, database};
use irys_storage::{ie, StorageModuleInfo};
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    DatabaseProvider, IrysBlockHeader, SimpleRNG, StorageConfig, H256,
};
use irys_types::{partition_chunk_offset_ie, PartitionChunkOffset};
use irys_types::{Config, H256List};
use openssl::sha;
use reth_db::Database;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock, RwLockReadGuard},
};

use tracing::{debug, error, trace, warn};

use crate::block_index_service::{
    BlockIndexReadGuard, BlockIndexService, GetBlockIndexGuardMessage,
};
use crate::broadcast_mining_service::{BroadcastMiningService, BroadcastPartitionsExpiration};

/// Allows for overriding of the consensus parameters for ledgers and partitions
#[derive(Debug, Clone, Default)]
pub struct EpochServiceConfig {
    /// Capacity partitions are allocated on a logarithmic curve, this scalar
    /// shifts the curve on the Y axis. Allowing there to be more or less
    /// capacity partitions relative to data partitions.
    pub capacity_scalar: u64,
    /// The length of an epoch denominated in block heights
    pub num_blocks_in_epoch: u64,
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
    pub data_partitions: HashMap<PartitionHash, PartitionAssignment>,
    /// Available capacity partitions mapped by partition hash
    pub capacity_partitions: HashMap<PartitionHash, PartitionAssignment>,
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
            data_partitions: HashMap::new(),
            capacity_partitions: HashMap::new(),
        }
    }

    /// Retrieves a `PartitionAssignment` by partition hash if it exists
    pub fn get_assignment(&self, partition_hash: H256) -> Option<PartitionAssignment> {
        self.data_partitions
            .get(&partition_hash)
            .copied()
            .or(self.capacity_partitions.get(&partition_hash).copied())
    }
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
    /// Current partition & ledger parameters
    pub config: EpochServiceConfig,
}

impl Actor for EpochServiceActor {
    type Context = Context<Self>;
}

/// Sent when a new epoch block is reached (and at genesis)
#[derive(Message, Debug)]
#[rtype(result = "Result<(),EpochServiceError>")]
pub struct NewEpochMessage(pub Arc<IrysBlockHeader>);

impl Handler<NewEpochMessage> for EpochServiceActor {
    type Result = Result<(), EpochServiceError>;
    fn handle(&mut self, msg: NewEpochMessage, _ctx: &mut Self::Context) -> Self::Result {
        let new_epoch_block = msg.0;

        self.perform_epoch_tasks(new_epoch_block)?;

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
        self.get_genesis_storage_module_infos(msg.0)
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

impl EpochServiceActor {
    /// Create a new instance of the epoch service actor
    pub fn new(epoch_config: EpochServiceConfig, config: &Config) -> Self {
        Self {
            last_epoch_hash: H256::zero(),
            ledgers: Arc::new(RwLock::new(Ledgers::new(config))),
            partition_assignments: Arc::new(RwLock::new(PartitionAssignments::new())),
            all_active_partitions: Vec::new(),
            config: epoch_config,
        }
    }

    pub async fn initialize(&mut self, db: &DatabaseProvider) {
        let addr = BlockIndexService::from_registry();

        let read_guard = addr.send(GetBlockIndexGuardMessage).await.unwrap();

        let rg = read_guard.read();

        let block_index = 0;
        // TODO: restore epoch block loops as now we are not triggering NewEpochMessage
        match rg.get_item(block_index) {
            Some(b) => {
                let block_header = database::block_header_by_hash(&db.tx().unwrap(), &b.block_hash)
                    .unwrap()
                    .unwrap();
                match self.perform_epoch_tasks(Arc::new(block_header)) {
                    Ok(_) => debug!(?block_index, "Processed epoch block"),
                    Err(e) => {
                        self.print_items(read_guard.clone(), db.clone());
                        panic!("{:?}", e);
                    }
                }
            }
            None => {
                panic!(
                    "Could not recover block at index during epoch service initialization {block_index:?}"
                );
            }
        }
    }

    fn print_items(&self, block_index_guard: BlockIndexReadGuard, db: DatabaseProvider) {
        let rg = block_index_guard.read();
        let tx = db.tx().unwrap();
        for i in 0..rg.num_blocks() {
            let item = rg.get_item(i as usize).unwrap();
            let block_hash = item.block_hash;
            let block = block_header_by_hash(&tx, &block_hash).unwrap().unwrap();
            println!(
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

        self.try_genesis_init(&new_epoch_block);

        // Future: Validate partition assignments against stake/pledge
        // requirements and remove any that no longer meet minimum thresholds

        self.expire_term_ledger_slots(&new_epoch_block);

        self.allocate_additional_ledger_slots(&new_epoch_block);

        self.backfill_missing_partitions();

        self.allocate_additional_capacity();

        Ok(())
    }

    /// Initialize genesis state by generating initial capacity partition hashes
    /// if none exist
    fn try_genesis_init(&mut self, new_epoch_block: &IrysBlockHeader) {
        if self.all_active_partitions.is_empty() && new_epoch_block.height == 0 {
            debug!("Performing genesis init");
            // Store the genesis epoch hash
            self.last_epoch_hash = new_epoch_block.last_epoch_hash;

            // Allocate 1 slot to each ledger and calculate the number of partitions
            let mut num_data_partitions = 0;

            // Create a scope for the write lock to expire with
            {
                let mut ledgers = self.ledgers.write().unwrap();
                for ledger in Ledger::iter() {
                    debug!("Allocating 1 slot for {:?}", &ledger);
                    num_data_partitions += ledgers[ledger].allocate_slots(1);
                }
            }

            // Calculate the total number of partitions
            let num_partitions = num_data_partitions
                + Self::get_num_capacity_partitions(num_data_partitions, &self.config);

            self.add_capacity_partitions(std::cmp::max(
                self.config
                    .num_capacity_partitions
                    .unwrap_or(num_partitions),
                num_partitions,
            ));
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
        for ledger in Ledger::iter() {
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
        for ledger in Ledger::iter() {
            self.process_slot_needs(ledger, &mut capacity_partitions, &mut rng);
        }
    }

    /// Process slot needs for a given ledger, assigning partitions to each slot
    /// as needed.
    pub fn process_slot_needs(
        &mut self,
        ledger: Ledger,
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
        let min_count = Ledger::ALL.len() as u64 * config.storage_config.num_partitions_in_slot;
        let base_count = std::cmp::max(num_data_partitions, min_count);
        let log_10 = (base_count as f64).log10();
        let trunc = truncate_to_3_decimals(log_10);
        let scaled = truncate_to_3_decimals(trunc * config.capacity_scalar as f64);

        truncate_to_3_decimals(scaled).ceil() as u64
    }

    /// Adds new capacity partitions to the protocols pool of partitions. This
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
            prev_partition_hash = next_part_hash;
        }

        // Create partition assignments for all the partitions to the local miners address
        // TODO: Change this ^^ when pledging and staking exist
        let mut pa = self.partition_assignments.write().unwrap();
        for partition_hash in &self.all_active_partitions {
            pa.capacity_partitions.insert(
                *partition_hash,
                PartitionAssignment {
                    partition_hash: *partition_hash,
                    miner_address: self.config.storage_config.miner_address,
                    ledger_id: None,
                    slot_index: None,
                },
            );
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
                let ledger: Ledger = Ledger::try_from(assignment.ledger_id.unwrap()).unwrap();
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
    fn assign_partition_to_slot(&self, partition_hash: H256, ledger: Ledger, slot_index: usize) {
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
    fn calculate_additional_slots(&self, new_epoch_block: &IrysBlockHeader, ledger: Ledger) -> u64 {
        let num_slots: u64;
        {
            let ledgers = self.ledgers.read().unwrap();
            let ledger = &ledgers[ledger];
            num_slots = ledger.slot_count() as u64;
        }
        let partition_chunk_count = self.config.storage_config.num_chunks_in_partition;
        let max_chunk_capacity = num_slots * partition_chunk_count;
        let ledger_size = new_epoch_block.ledgers[ledger as usize].max_chunk_offset;

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
        // TODO: need a block index to do this
        if new_epoch_block.height >= self.config.num_blocks_in_epoch {
            // let last_epoch_block =
            //     block_index.get(new_new_epoch_block.height - NUM_BLOCKS_IN_EPOCH);
            // let data_added: u64 = ledger_size - last_epoch_block.ledger_size;
            // slots_to_add += Math::ceil(data_added / PARTITION_SIZE)
        }

        slots_to_add
    }

    /// Configure storage modules for genesis partition assignments
    pub fn get_genesis_storage_module_infos(
        &self,
        storage_module_config: StorageSubmodulesConfig,
    ) -> Vec<StorageModuleInfo> {
        let ledgers = self.ledgers.read().unwrap();
        let num_part_chunks = self.config.storage_config.num_chunks_in_partition as u32;

        let pa = self.partition_assignments.read().unwrap();

        let sm_paths = storage_module_config.submodule_paths;
        // Configure publish ledger storage
        let mut module_infos = ledgers
            .get_slots(Ledger::Publish)
            .iter()
            .flat_map(|slot| &slot.partitions)
            .enumerate()
            .map(|(idx, partition)| StorageModuleInfo {
                id: idx,
                partition_assignment: Some(*pa.data_partitions.get(partition).unwrap()),
                submodules: vec![(
                    partition_chunk_offset_ie!(0, num_part_chunks),
                    sm_paths[idx].clone(),
                )],
            })
            .collect::<Vec<_>>();

        let idx_start = module_infos.len();

        // Configure submit ledger storage
        let submit_infos = ledgers
            .get_slots(Ledger::Submit)
            .iter()
            .flat_map(|slot| &slot.partitions)
            .enumerate()
            .map(|(idx, partition)| StorageModuleInfo {
                id: idx_start + idx,
                partition_assignment: Some(*pa.data_partitions.get(partition).unwrap()),
                submodules: vec![(
                    partition_chunk_offset_ie!(0, num_part_chunks),
                    sm_paths[idx_start + idx].clone(),
                )],
            })
            .collect::<Vec<_>>();

        module_infos.extend(submit_infos);

        // Sort the active capacity partitions by hash
        let mut capacity_partitions: Vec<H256> = pa.capacity_partitions.keys().copied().collect();
        capacity_partitions.sort_unstable();
        let mut cap_parts: VecDeque<H256> = capacity_partitions.into();

        // Add initial capacity partition config
        let start_idx = module_infos.len();

        for i in start_idx..sm_paths.len() {
            let cap_part = cap_parts.pop_front().unwrap();
            let sm_info = StorageModuleInfo {
                id: i,
                partition_assignment: Some(*pa.capacity_partitions.get(&cap_part).unwrap()),
                submodules: vec![(
                    partition_chunk_offset_ie!(0, num_part_chunks),
                    sm_paths[i].clone(),
                )],
            };
            module_infos.push(sm_info);
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

    use actix::{actors::mocker::Mocker, Addr, Arbiter, Recipient};
    use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_storage::{ie, StorageModule, StorageModuleVec};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{partition_chunk_offset_ie, Address, PartitionChunkRange};
    use reth::revm::primitives::hex;
    use tokio::time::sleep;

    use crate::{
        mining::PartitionMiningActor,
        packing::{PackingActor, PackingRequest},
        BlockProducerMockActor, MockedBlockProducerAddr, SolutionFoundMessage,
    };
    use irys_vdf::vdf_state::{VdfState, VdfStepsReadGuard};

    use super::*;

    #[actix::test]
    async fn genesis_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        let testnet_config = Config::testnet();
        genesis_block.height = 0;

        // Create epoch service with random miner address
        let config = EpochServiceConfig::new(&testnet_config);
        let mut epoch_service = EpochServiceActor::new(config.clone(), &testnet_config);
        let miner_address = config.storage_config.miner_address;

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let _ = epoch_service.handle(NewEpochMessage(genesis_block.into()), &mut Context::new());

        {
            // Verify the correct number of ledgers have been added
            let ledgers = epoch_service.ledgers.read().unwrap();
            let expected_ledger_count = Ledger::ALL.len();
            assert_eq!(ledgers.len(), expected_ledger_count);

            // Verify each ledger has one slot and the correct number of partitions
            let pub_slots = ledgers.get_slots(Ledger::Publish);
            let sub_slots = ledgers.get_slots(Ledger::Submit);

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
                            ledger_id: Some(Ledger::Publish.into()),
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
                            ledger_id: Some(Ledger::Submit.into()),
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

        // Create epoch service
        let config = EpochServiceConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            storage_config,
            num_capacity_partitions: Some(123),
        };
        let num_blocks_in_epoch = config.num_blocks_in_epoch;

        let mut epoch_service = EpochServiceActor::new(config, &testnet_config);

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let mut ctx = Context::new();
        let _ = epoch_service.handle(NewEpochMessage(genesis_block.into()), &mut ctx);

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.height = num_blocks_in_epoch;
        new_epoch_block.ledgers[Ledger::Submit].max_chunk_offset = num_chunks_in_partition / 2;

        let _ = epoch_service.handle(NewEpochMessage(new_epoch_block.into()), &mut ctx);

        // Verify each ledger has one slot and the correct number of partitions
        {
            let ledgers = epoch_service.ledgers.read().unwrap();
            let pub_slots = ledgers.get_slots(Ledger::Publish);
            let sub_slots = ledgers.get_slots(Ledger::Submit);
            assert_eq!(pub_slots.len(), 1);
            assert_eq!(sub_slots.len(), 2);
        }

        // Simulate a subsequent epoch block that adds multiple ledger slots
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.height = num_blocks_in_epoch * 2;
        new_epoch_block.ledgers[Ledger::Submit].max_chunk_offset =
            (num_chunks_in_partition as f64 * 2.5) as u64;
        new_epoch_block.ledgers[Ledger::Publish as usize].max_chunk_offset =
            (num_chunks_in_partition as f64 * 0.75) as u64;

        let _ = epoch_service.handle(NewEpochMessage(new_epoch_block.into()), &mut ctx);

        // Validate the correct number of ledgers slots were added to each ledger
        {
            let ledgers = epoch_service.ledgers.read().unwrap();
            let pub_slots = ledgers.get_slots(Ledger::Publish);
            let sub_slots = ledgers.get_slots(Ledger::Submit);
            assert_eq!(pub_slots.len(), 2);
            assert_eq!(sub_slots.len(), 4);
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
            ..Config::testnet()
        };
        let mining_address = testnet_config.miner_address();

        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;

        // Create a storage config for testing
        let storage_config = StorageConfig::new(&testnet_config);
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;
        let tmp_dir = setup_tracing_and_temp_dir(Some("partition_expiration_test"), false);
        let base_path = tmp_dir.path().to_path_buf();

        let num_blocks_in_epoch = testnet_config.num_blocks_in_epoch;

        // Create epoch service
        let config = EpochServiceConfig::new(&testnet_config);
        let epoch_service = EpochServiceActor::new(config, &testnet_config);
        let epoch_service_actor = epoch_service.start();

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let _ = epoch_service_actor
            .send(NewEpochMessage(genesis_block.into()))
            .await
            .unwrap();

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.height =
            (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
        new_epoch_block.ledgers[Ledger::Submit].max_chunk_offset = num_chunks_in_partition / 2;

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

        let submit_partition_hash = {
            let partition_assignments_read = epoch_service_actor
                .send(GetPartitionAssignmentsGuardMessage)
                .await
                .unwrap();

            let partition_hash = partition_assignments_read
                .read()
                .data_partitions
                .iter()
                .find(|(_hash, assignment)| assignment.ledger_id == Some(Ledger::Submit.get_id()))
                .map(|(hash, _)| hash.clone())
                .expect("There should be a partition assigned to submit ledger");

            partition_hash
        };

        let (publish_partition_hash, submit_partition_hash) = {
            let ledgers = epoch_service_actor
                .send(GetLedgersGuardMessage)
                .await
                .unwrap();

            let pub_slots = ledgers.read().get_slots(Ledger::Publish).clone();
            let sub_slots = ledgers.read().get_slots(Ledger::Submit).clone();
            assert_eq!(pub_slots.len(), 1);
            assert_eq!(sub_slots.len(), 1);

            (pub_slots[0].partitions[0], sub_slots[0].partitions[0])
        };

        assert_eq!(submit_partition_hash, submit_partition_hash);

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

        let _ = epoch_service_actor
            .send(NewEpochMessage(new_epoch_block.into()))
            .await
            .unwrap();
        // give computation time for broadcaster to receive and broadcast expiration
        sleep(Duration::from_secs(1)).await;

        // busypoll the solution context rwlock
        let pack_req = 'outer: loop {
            match arc_rwlock.try_read() {
                Ok(lck) => {
                    if lck.is_none() {
                        debug!("Packing request not ready waiting!");
                        sleep(Duration::from_millis(50)).await;
                    } else {
                        debug!("Packing request received ready!");
                        break 'outer lck.as_ref().unwrap().clone();
                    }
                }
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
        };

        // check a new slots is inserted with a partition assigned to it, and slot 0 expires and partition is removed
        let (publish_partition, submit_partition_1, submit_partition_2) = {
            let ledgers = epoch_service_actor
                .send(GetLedgersGuardMessage)
                .await
                .unwrap();

            let pub_slots = ledgers.read().get_slots(Ledger::Publish).clone();
            let sub_slots = ledgers.read().get_slots(Ledger::Submit).clone();
            assert_eq!(
                pub_slots.len(),
                1,
                "Publish should still have only one slot"
            );
            assert_eq!(sub_slots.len(), 3, "Submit slots should have two new not expired slots with a new fresh partition from available previous capacity ones!");
            assert!(
                sub_slots[0].is_expired && sub_slots[0].partitions.len() == 0,
                "Slot 0 should have expired and have no assigned partition!"
            );
            assert!(!sub_slots[1].is_expired
                    && sub_slots[1].partitions.len() == 1
                    && (capacity_partitions.contains(&sub_slots[1].partitions[0])
                        || sub_slots[1].partitions[0] == submit_partition_hash),
                    "Slot 1 should not be expired and have a new fresh partition from previous capacity ones or the expired one!");
            assert!(!sub_slots[2].is_expired
                    && sub_slots[2].partitions.len() == 1
                    && (capacity_partitions.contains(&sub_slots[2].partitions[0])
                        || sub_slots[2].partitions[0] == submit_partition_hash),
                    "Slot 2 should not be expired and have a new fresh partition from previous capacity ones or the expired one!");

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

        // check expired partition is in capacity and repacking request is for it and its whole interval range, and partitions assignments are consistent
        {
            let partition_assignments_read = epoch_service_actor
                .send(GetPartitionAssignmentsGuardMessage)
                .await
                .unwrap();

            assert!(
                partition_assignments_read
                    .read()
                    .capacity_partitions
                    .contains_key(&submit_partition_hash),
                "Expired partition should be in capacity partitions"
            );

            assert_eq!(
                partition_assignments_read.read().data_partitions.len(),
                3,
                "Should have three partitions assignments"
            );

            if let Some(publish_assignment) = partition_assignments_read
                .read()
                .data_partitions
                .get(&publish_partition)
            {
                assert_eq!(
                    publish_assignment.ledger_id,
                    Some(Ledger::Publish.get_id()),
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
                .get(&submit_partition_1)
            {
                assert_eq!(
                    submit_assignment.ledger_id,
                    Some(Ledger::Submit.get_id()),
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
                .get(&submit_partition_2)
            {
                assert_eq!(
                    submit_assignment.ledger_id,
                    Some(Ledger::Submit.get_id()),
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
    async fn partitions_assignment_determinism_test() {
        let testnet_config = Config::testnet();
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.last_epoch_hash = H256::zero(); // for partitions hash determinism
        genesis_block.height = 0;

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
            chain_id: 1,
        };
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;

        // Create epoch service
        let config = EpochServiceConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            num_capacity_partitions: None,
            storage_config,
        };
        let num_blocks_in_epoch = config.num_blocks_in_epoch;

        let mut epoch_service = EpochServiceActor::new(config.clone(), &testnet_config);

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let mut ctx = Context::new();
        let _ = epoch_service.handle(NewEpochMessage(genesis_block.into()), &mut ctx);

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let total_epoch_messages = 6;
        let mut epoch_num = 1;
        let mut new_epoch_block = IrysBlockHeader::new_mock_header();
        new_epoch_block.ledgers[Ledger::Submit].max_chunk_offset = num_chunks_in_partition;
        new_epoch_block.ledgers[Ledger::Publish].max_chunk_offset = num_chunks_in_partition;

        while epoch_num <= total_epoch_messages {
            new_epoch_block.height =
                (testnet_config.submit_ledger_epoch_length * epoch_num) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
            let _ = epoch_service.handle(NewEpochMessage(new_epoch_block.clone().into()), &mut ctx);
            epoch_num += 1;
        }

        // Check determinism in assigned partitions
        let publish_slot_0 = H256(
            hex::decode("12771355e46cd47c71ed1721fd5319b383cca3a1f9fce3aa1c8cd3bd37af20d7")
                .unwrap()
                .try_into()
                .unwrap(),
        );

        if let Some(publish_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&publish_slot_0)
        {
            assert_eq!(
                publish_assignment.ledger_id,
                Some(Ledger::Publish.get_id()),
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

        let publish_slot_1 = H256(
            hex::decode("12d7fd61841c114d9a4011710874c5b4857c35266ef13b1ac3f3b476780e9b53")
                .unwrap()
                .try_into()
                .unwrap(),
        );

        if let Some(publish_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&publish_slot_1)
        {
            assert_eq!(
                publish_assignment.ledger_id,
                Some(Ledger::Publish.get_id()),
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

        let submit_slot_11 = H256(
            hex::decode("07a0f53a2326c4e19d72d9901769c7275a5dfeddd68b49c6ed77c96e19bb6f2d")
                .unwrap()
                .try_into()
                .unwrap(),
        );

        if let Some(submit_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&submit_slot_11)
        {
            assert_eq!(
                submit_assignment.ledger_id,
                Some(Ledger::Submit.get_id()),
                "Should be assigned to submit ledger"
            );
            assert_eq!(
                submit_assignment.slot_index,
                Some(11),
                "Should be assigned to slot 11"
            );
        } else {
            panic!("Should have an assignment");
        };

        let submit_slot_12 = H256(
            hex::decode("fe4af4eb44d9b92afdc3113bc3fba48531502d6367ad42de3a7f1d1ea4065ba4")
                .unwrap()
                .try_into()
                .unwrap(),
        );

        if let Some(submit_assignment) = epoch_service
            .partition_assignments
            .read()
            .unwrap()
            .data_partitions
            .get(&submit_slot_12)
        {
            assert_eq!(
                submit_assignment.ledger_id,
                Some(Ledger::Submit.get_id()),
                "Should be assigned to submit ledger"
            );
            assert_eq!(
                submit_assignment.slot_index,
                Some(12),
                "Should be assigned to slot 12"
            );
        } else {
            panic!("Should have an assignment");
        };
    }
}
