use actix::{Actor, ArbiterService, Context, Handler, Message, MessageResponse};
use eyre::{Error, Result};
use irys_database::{data_ledger::*, database};
use irys_storage::{ie, StorageModuleInfo};
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    DatabaseProvider, IrysBlockHeader, SimpleRNG, StorageConfig, CONFIG, H256,
};
use openssl::sha;
use reth_db::Database;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock, RwLockReadGuard},
};

use crate::block_index_service::{BlockIndexService, GetBlockIndexGuardMessage};

/// Allows for overriding of the consensus parameters for ledgers and partitions
#[derive(Debug, Clone)]
pub struct EpochServiceConfig {
    /// Capacity partitions are allocated on a logarithmic curve, this scalar
    /// shifts the curve on the Y axis. Allowing there to be more or less
    /// capacity partitions relative to data partitions.
    pub capacity_scalar: u64,
    /// The length of an epoch denominated in block heights
    pub num_blocks_in_epoch: u64,
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
}

impl Default for EpochServiceConfig {
    fn default() -> Self {
        Self {
            capacity_scalar: CONFIG.capacity_scalar,
            num_blocks_in_epoch: CONFIG.num_blocks_in_epoch,
            storage_config: StorageConfig::default(),
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
    /// Creates a new `ReadGard` for Ledgers
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
    /// Creates a new `ReadGard` for Ledgers
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
pub struct GetGenesisStorageModulesMessage;

impl Handler<GetGenesisStorageModulesMessage> for EpochServiceActor {
    type Result = Vec<StorageModuleInfo>;

    fn handle(
        &mut self,
        _msg: GetGenesisStorageModulesMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.get_genesis_storage_module_infos([] as [PathBuf; 0])
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
    pub fn new(config: Option<EpochServiceConfig>) -> Self {
        let config = match config {
            Some(cfg) => cfg,
            // If no config was provided, use the default protocol parameters
            None => EpochServiceConfig::default(),
        };

        Self {
            last_epoch_hash: H256::zero(),
            ledgers: Arc::new(RwLock::new(Ledgers::new())),
            partition_assignments: Arc::new(RwLock::new(PartitionAssignments::new())),
            all_active_partitions: Vec::new(),
            config,
        }
    }

    pub async fn initialize(&mut self, db: &DatabaseProvider) {
        let addr = BlockIndexService::from_registry();

        let read_guard = addr.send(GetBlockIndexGuardMessage).await.unwrap();

        let rg = read_guard.read();

        let mut block_index = 0_u64;

        loop {
            let block = rg.get_item(block_index.try_into().unwrap());

            match block {
                Some(b) => {
                    let block_header =
                        database::block_header_by_hash(&db.tx().unwrap(), &b.block_hash)
                            .unwrap()
                            .unwrap();
                    self.perform_epoch_tasks(Arc::new(block_header)).unwrap();
                    block_index += self.config.num_blocks_in_epoch;
                }
                None => {
                    break;
                }
            }
        }
    }

    /// Main worker function
    pub fn perform_epoch_tasks(
        &mut self,
        new_epoch_block: Arc<IrysBlockHeader>,
    ) -> Result<(), EpochServiceError> {
        // Validate this is an epoch block height
        if new_epoch_block.height % CONFIG.num_blocks_in_epoch != 0 {
            return Err(EpochServiceError::NotAnEpochBlock);
        }

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
            // Store the genesis epoch hash
            self.last_epoch_hash = new_epoch_block.last_epoch_hash;

            // Allocate 1 slot to each ledger and calculate the number of partitions
            let mut num_data_partitions = 0;

            // Create a scope for the write lock to expire with
            {
                let mut ledgers = self.ledgers.write().unwrap();
                for ledger_num in Ledger::iter() {
                    num_data_partitions += ledgers[ledger_num].allocate_slots(1);
                }
            }

            // Calculate the total number of partitions
            let num_partitions = num_data_partitions
                + Self::get_num_capacity_partitions(num_data_partitions, &self.config);

            let num_cap_parts = if CONFIG.num_capacity_partitions == 0 {
                num_partitions
            } else {
                CONFIG.num_capacity_partitions
            };
            self.add_capacity_partitions(num_cap_parts);
        }
    }

    /// Loops though all of the term ledgers and looks for slots that are older
    /// than the `epoch_length` (term length) of the ledger.
    fn expire_term_ledger_slots(&mut self, new_epoch_block: &IrysBlockHeader) {
        let epoch_height = new_epoch_block.height;
        let expired_hashes: Vec<H256>;
        {
            let mut ledgers = self.ledgers.write().unwrap();
            expired_hashes = ledgers.get_expired_partition_hashes(epoch_height);
        }

        // Update expired data partitions assignments marking them as capacity partitions
        for partition_hash in expired_hashes {
            self.mark_partition_as_expired(partition_hash);
        }
    }

    /// Loops though all the ledgers both perm and term, checking to see if any
    /// require additional ledger slots added to accommodate data ingress.
    fn allocate_additional_ledger_slots(&mut self, new_epoch_block: &IrysBlockHeader) {
        for ledger_num in Ledger::iter() {
            let part_slots = self.calculate_additional_slots(new_epoch_block, ledger_num);
            {
                let mut ledgers = self.ledgers.write().unwrap();
                ledgers[ledger_num].allocate_slots(part_slots);
            }
        }
    }

    /// Based on the amount of data in each ledger, this function calculates
    /// the number of partitions the protocol should be managing and allocates
    /// additional partitions (and their state) as needed.
    fn allocate_additional_capacity(&mut self) {
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

        // println!(
        //     "- base_count: {}, log_10: {}, trunc: {}, scaled: {}, rounded: {}",
        //     base_count, log_10, trunc, scaled, rounded
        // );
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

        // Compute the partition hashes for all of the added partitions
        for _i in 0..parts_to_add {
            let next_part_hash = H256(hash_sha256(&prev_partition_hash.0).unwrap());
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
                    ledger_num: None,
                    slot_index: None,
                },
            );
        }
    }

    // Updates PartitionAssignment information about a partition hash, marking
    // it as expired (or unassigned to a slot in a data ledger)
    fn mark_partition_as_expired(&mut self, partition_hash: H256) {
        let mut pa = self.partition_assignments.write().unwrap();
        // Convert data partition to capacity partition if it exists
        if let Some(mut assignment) = pa.data_partitions.remove(&partition_hash) {
            // Clear ledger assignment
            assignment.ledger_num = None;
            assignment.slot_index = None;

            // Add to capacity pool
            pa.capacity_partitions.insert(partition_hash, assignment);
        }
    }

    /// Takes a capacity partition hash and updates its `PartitionAssignment`
    /// state to indicate it is part of a data ledger
    fn assign_partition_to_slot(
        &mut self,
        partition_hash: H256,
        ledger: Ledger,
        slot_index: usize,
    ) {
        let mut pa = self.partition_assignments.write().unwrap();
        if let Some(mut assignment) = pa.capacity_partitions.remove(&partition_hash) {
            assignment.ledger_num = Some(ledger as u64);
            assignment.slot_index = Some(slot_index);
            pa.data_partitions.insert(partition_hash, assignment);
        }
    }

    /// For a given ledger indicated by `ledger_num`, calculate the number of
    /// partition slots to add to the ledger based on remaining capacity
    /// and data ingress this epoch
    fn calculate_additional_slots(
        &self,
        new_epoch_block: &IrysBlockHeader,
        ledger_num: Ledger,
    ) -> u64 {
        let num_slots: u64;
        {
            let ledgers = self.ledgers.read().unwrap();
            let ledger = &ledgers[ledger_num];
            num_slots = ledger.slot_count() as u64;
        }
        let partition_chunk_count = self.config.storage_config.num_chunks_in_partition;
        let max_chunk_capacity = num_slots * partition_chunk_count;
        let ledger_size = new_epoch_block.ledgers[ledger_num as usize].max_chunk_offset;

        // Add capacity slots if ledger usage exceeds 50% of partition size from max capacity
        let add_capacity_threshold = max_chunk_capacity - partition_chunk_count / 2;
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
        _paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Vec<StorageModuleInfo> {
        let ledgers = self.ledgers.read().unwrap();
        let num_part_chunks = self.config.storage_config.num_chunks_in_partition as u32;

        let pa = self.partition_assignments.read().unwrap();

        // Configure publish ledger storage
        let mut module_infos = ledgers
            .get_slots(Ledger::Publish)
            .iter()
            .flat_map(|slot| &slot.partitions)
            .enumerate()
            .map(|(idx, partition)| StorageModuleInfo {
                id: idx,
                partition_assignment: Some(*pa.data_partitions.get(partition).unwrap()),
                submodules: vec![(ie(0, num_part_chunks), format!("submodule_{}", idx).into())],
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
                    ie(0, num_part_chunks),
                    format!("submodule_{}", idx_start + idx).into(),
                )],
            })
            .collect::<Vec<_>>();

        module_infos.extend(submit_infos);

        // Sort the active capacity partitions by hash
        let mut capacity_partitions: Vec<H256> = pa.capacity_partitions.keys().copied().collect();
        capacity_partitions.sort_unstable();

        // Add initial capacity partition config
        let cap_part = capacity_partitions.first().unwrap();
        let idx = module_infos.len();
        let cap_info = StorageModuleInfo {
            id: idx,
            partition_assignment: Some(*pa.capacity_partitions.get(cap_part).unwrap()),
            submodules: vec![(ie(0, num_part_chunks), format!("submodule_{}", idx).into())],
        };

        module_infos.push(cap_info);
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
    use irys_types::{Address, CONFIG};

    use super::*;

    #[actix::test]
    async fn genesis_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;

        // Create epoch service with random miner address
        let config = EpochServiceConfig::default();
        let mut epoch_service = EpochServiceActor::new(Some(config.clone()));
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

            let pub_ledger_num = Ledger::Publish as u64;
            let sub_ledger_num = Ledger::Submit as u64;

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
                            ledger_num: Some(pub_ledger_num),
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
                            ledger_num: Some(sub_ledger_num),
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
                        ledger_num: None,
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

        let infos = epoch_service.get_genesis_storage_module_infos([] as [PathBuf; 0]);
        println!("{:#?}", infos);
    }

    #[actix::test]
    async fn add_slots_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;

        // Create a storage config for testing
        let storage_config = StorageConfig {
            chunk_size: 32,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: Address::random(),
            min_writes_before_sync: 1,
            entropy_packing_iterations: CONFIG.entropy_packing_iterations,
            num_confirmations_for_finality: 1, // Testnet / single node config
        };
        let num_chunks_in_partition = storage_config.num_chunks_in_partition;

        // Create epoch service
        let config = EpochServiceConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            storage_config,
        };
        let num_blocks_in_epoch = config.num_blocks_in_epoch;

        let mut epoch_service = EpochServiceActor::new(Some(config));

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let mut ctx = Context::new();
        let _ = epoch_service.handle(NewEpochMessage(genesis_block.into()), &mut ctx);

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new();
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
        let mut new_epoch_block = IrysBlockHeader::new();
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
    async fn expire_slots_test() {}

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
}
