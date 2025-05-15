use actix::SystemService;
use actix::{Actor, Context};
use base58::ToBase58;
use eyre::{Error, Result};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::{data_ledger::*, SystemLedger};
use irys_primitives::CommitmentStatus;
use irys_storage::{ie, StorageModuleInfo};
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    DataLedger, IrysBlockHeader, SimpleRNG, H256,
};
use irys_types::{
    partition_chunk_offset_ie, Address, CommitmentTransaction, ConsensusConfig,
    PartitionChunkOffset,
};
use irys_types::{Config, H256List};
use openssl::sha;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
};

use tracing::{debug, error, trace, warn};

use crate::broadcast_mining_service::{BroadcastMiningService, BroadcastPartitionsExpiration};
use crate::services::ServiceSenders;
use crate::StorageModuleServiceMessage;

use super::{CommitmentState, CommitmentStateEntry, EpochReplayData, PartitionAssignments};

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
    /// Reference to mpsc service channels
    pub service_senders: ServiceSenders,
    /// Submodules config
    pub storage_submodules_config: StorageSubmodulesConfig,
    /// Current partition & ledger parameters
    pub config: Config,
    /// Computed commitment state
    pub(super) commitment_state: Arc<RwLock<CommitmentState>>,
}

impl Actor for EpochServiceActor {
    type Context = Context<Self>;
}

/// Reasons why the epoch service actors epoch tasks might fail
#[derive(Debug)]
pub enum EpochServiceError {
    /// Catchall error until more detailed errors are added
    InternalError,
    /// Attempted to do epoch tasks on a block that was not an epoch block
    NotAnEpochBlock,
    /// Provided an incorrect previous epoch block
    IncorrectPreviousEpochBlock,
    /// Validation of commitments failed
    InvalidCommitments,
}

impl EpochServiceActor {
    /// Create a new instance of the epoch service actor
    pub fn new(
        service_senders: &ServiceSenders,
        storage_submodules_config: &StorageSubmodulesConfig,
        config: &Config,
    ) -> Self {
        Self {
            last_epoch_hash: H256::zero(),
            ledgers: Arc::new(RwLock::new(Ledgers::new(&config.consensus))),
            partition_assignments: Arc::new(RwLock::new(PartitionAssignments::new())),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            service_senders: service_senders.clone(),
            storage_submodules_config: storage_submodules_config.clone(),
            config: config.clone(),
            commitment_state: Default::default(),
        }
    }

    pub fn initialize(
        &mut self,
        genesis_block: IrysBlockHeader,
        commitments: Vec<CommitmentTransaction>,
    ) -> eyre::Result<Vec<StorageModuleInfo>> {
        Self::validate_commitments(&genesis_block, &commitments)?;

        match self.perform_epoch_tasks(&None, &genesis_block, commitments) {
            Ok(_) => debug!("Processed genesis epoch block"),
            Err(e) => {
                return Err(eyre::eyre!("Error performing genesis epoch tasks {:?}", e));
            }
        }

        let storage_module_info =
            self.map_storage_modules_to_partition_assignments(&self.storage_submodules_config);

        Ok(storage_module_info)
    }

    pub fn replay_epoch_data(
        &mut self,
        epoch_replay_data: Vec<EpochReplayData>,
    ) -> eyre::Result<Vec<StorageModuleInfo>> {
        // Initialize as None for the first iteration
        let mut previous_epoch_block: Option<IrysBlockHeader> = None;

        for replay_data in epoch_replay_data {
            let block_header = replay_data.epoch_block;
            let commitments = replay_data.commitments;

            match self.perform_epoch_tasks(&previous_epoch_block, &block_header, commitments) {
                Ok(_) => debug!("Processed replay epoch block"),
                Err(e) => {
                    return Err(eyre::eyre!("Error performing epoch tasks {:?}", e));
                }
            }

            // Store the owned block_header, not a reference
            previous_epoch_block = Some(block_header);
        }

        let storage_module_info =
            self.map_storage_modules_to_partition_assignments(&self.storage_submodules_config);

        Ok(storage_module_info)
    }

    fn validate_commitments(
        block_header: &IrysBlockHeader,
        commitments: &Vec<CommitmentTransaction>,
    ) -> eyre::Result<()> {
        // Extract the commitments ledger from the system ledgers in the epoch block
        let commitment_ledger = block_header
            .system_ledgers
            .iter()
            .find(|b| b.ledger_id == SystemLedger::Commitment);

        // Verify that each commitment transaction ID referenced in the commitments ledger has a
        // corresponding commitment transaction in the replay data
        if let Some(commitment_ledger) = commitment_ledger {
            for txid in commitment_ledger.tx_ids.iter() {
                // If we can't find the commitment transaction for a referenced txid, return an error
                if commitments.iter().find(|c| c.id == *txid).is_none() {
                    return Err(eyre::eyre!(
                        "Missing commitment transaction {} for block {}",
                        txid.0.to_base58(),
                        block_header.block_hash.0.to_base58()
                    ));
                }
            }
        }
        Ok(())
    }

    fn is_epoch_block(&self, block_header: &IrysBlockHeader) -> Result<(), EpochServiceError> {
        if block_header.height % self.config.consensus.epoch.num_blocks_in_epoch != 0 {
            error!(
                "Not an epoch block height: {} num_blocks_in_epoch: {}",
                block_header.height, self.config.consensus.epoch.num_blocks_in_epoch
            );
            return Err(EpochServiceError::NotAnEpochBlock);
        }
        Ok(())
    }

    /// Main worker function
    pub fn perform_epoch_tasks(
        &mut self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
        new_epoch_commitments: Vec<CommitmentTransaction>,
    ) -> Result<(), EpochServiceError> {
        // Validate the epoch blocks
        self.is_epoch_block(new_epoch_block)?;

        // Skip previous block validation for genesis block (height 0)
        if new_epoch_block.height <= self.config.consensus.epoch.num_blocks_in_epoch {
            // Continue with validation logic for commitments
        } else {
            // For non-genesis blocks, previous epoch block must exist and have correct height
            let prev_block = previous_epoch_block
                .as_ref()
                .ok_or(EpochServiceError::IncorrectPreviousEpochBlock)?;

            // Validate the previous epoch block is the correct height
            if prev_block.height + self.config.consensus.epoch.num_blocks_in_epoch
                != new_epoch_block.height
            {
                return Err(EpochServiceError::IncorrectPreviousEpochBlock);
            }
        }

        // Validate the commitments
        Self::validate_commitments(&new_epoch_block, &new_epoch_commitments)
            .map_err(|_| EpochServiceError::InvalidCommitments)?;

        debug!(
            "Performing epoch tasks for {} ({})",
            &new_epoch_block.block_hash, &new_epoch_block.height
        );

        self.compute_commitment_state(new_epoch_commitments);

        self.try_genesis_init(new_epoch_block);

        self.allocate_additional_ledger_slots(previous_epoch_block, new_epoch_block);

        self.expire_term_ledger_slots(new_epoch_block);

        self.backfill_missing_partitions();

        self.allocate_additional_capacity();

        self.assign_partition_hashes_to_pledges();

        self.notify_storage_module_service();

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
                    num_data_partitions +=
                        ledgers[ledger].allocate_slots(1, new_epoch_block.height);
                }
            }

            // Calculate the total number of capacity partitions
            let projected_capacity_parts =
                Self::get_num_capacity_partitions(num_data_partitions, &self.config.consensus);

            // Determine the number of capacity partitions to create
            // We take the greater of:
            // 1. The config override (if specified)
            // 2. The projected number calculated from data partitions
            self.add_capacity_partitions(std::cmp::max(
                self.config
                    .consensus
                    .epoch
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

        let mut ledgers = self.ledgers.write().unwrap();
        expired_hashes = ledgers.get_expired_partition_hashes(epoch_height);
        drop(ledgers);

        // Return early if there's no more work to do
        if expired_hashes.is_empty() {
            return;
        }

        debug!("Expiring Hashes: {:?}", expired_hashes);

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
    fn allocate_additional_ledger_slots(
        &self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
    ) {
        for ledger in DataLedger::iter() {
            let part_slots =
                self.calculate_additional_slots(previous_epoch_block, new_epoch_block, ledger);
            let mut ledgers = self.ledgers.write().unwrap();
            debug!("Allocating {} slots for ledger {:?}", &part_slots, &ledger);
            ledgers[ledger].allocate_slots(part_slots, new_epoch_block.height);
            drop(ledgers);
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
                Self::get_num_capacity_partitions(num_data_partitions, &self.config.consensus);
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
    pub fn get_num_capacity_partitions(num_data_partitions: u64, config: &ConsensusConfig) -> u64 {
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
            // Remove the partition hash from the slots state
            let ledger: DataLedger = DataLedger::try_from(assignment.ledger_id.unwrap()).unwrap();
            let partition_hash = assignment.partition_hash;
            let slot_index = assignment.slot_index.unwrap();
            let mut write = self.ledgers.write().unwrap();
            write.remove_partition_from_slot(ledger, slot_index, &partition_hash);
            drop(write);

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

    /// Calculate partition slots to add to a ledger based on current utilization and growth rate
    ///
    /// This function implements the dynamic capacity management algorithm with two strategies:
    /// 1. Threshold-based: Adds slots when current utilization approaches capacity limit
    /// 2. Growth-based: Adds slots based on data ingress rate from previous epoch
    ///
    /// @param previous_epoch_block Optional header from previous epoch for growth calculation (can be None if genesis block is previous)
    /// @param new_epoch_block Current epoch header containing ledger state
    /// @param ledger Target data ledger to evaluate for expansion
    /// @return Number of partition slots to add
    fn calculate_additional_slots(
        &self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
        ledger: DataLedger,
    ) -> u64 {
        // Get current ledger state
        let ledgers = self.ledgers.read().unwrap();
        let data_ledger = &ledgers[ledger];
        let num_slots = data_ledger.slot_count() as u64;
        drop(ledgers);

        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;
        let max_ledger_capacity = num_slots * num_chunks_in_partition;
        let ledger_size = new_epoch_block.data_ledgers[ledger].max_chunk_offset;

        // STRATEGY 1: Threshold-based capacity expansion
        // Add slots when utilization reaches within half partition of max capacity
        let add_capacity_threshold =
            max_ledger_capacity.saturating_sub(num_chunks_in_partition / 2);

        let mut slots_to_add: u64 = 0;
        if ledger_size >= add_capacity_threshold {
            slots_to_add = 2;
        }

        // STRATEGY 2: Growth-based capacity expansion
        // Add slots proportional to data ingress rate from previous epoch
        if new_epoch_block.height >= self.config.consensus.epoch.num_blocks_in_epoch {
            let previous_ledger_size = previous_epoch_block
                .as_ref()
                .map_or(0, |prev| prev.data_ledgers[ledger].max_chunk_offset);

            let data_added = ledger_size - previous_ledger_size;

            // If data added exceeds a full partition, scale capacity proportionally
            if data_added > num_chunks_in_partition {
                slots_to_add += u64::div_ceil(data_added, num_chunks_in_partition);
            }
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
        drop(commitment_state); // Ensure the writes are recorded

        let mut commitment_state = self
            .commitment_state
            .write()
            .expect("to create a writeable commitment state");

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
                panic!("Invalid commitments found in epoch block");
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

        // Loop through both lists assigning capacity partitions to pledges
        for pledge in &unassigned_pledges {
            // Get the next available partition hash
            let Some(partition_hash) = unassigned_parts.pop_front() else {
                break; // No more partitions available
            };

            // Try to get all the pledge commitments for a particular address
            let Some(entries) = commitment_state.pledge_commitments.get_mut(&pledge.signer) else {
                continue; // No pledges for this signer, skip to next
            };

            // Find the specific pledge entry by ID
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == pledge.id) else {
                continue; // Pledge not found, skip to next
            };

            // Assign the partition hash to this pledge
            entry.partition_hash = Some(partition_hash);

            // Update partition assignments state to reference the assigned address
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

            debug!(
                "Assigned partition_hash {} to address {}",
                partition_hash, pledge.signer
            );

            // Remove the hash from unassigned partitions
            self.unassigned_partitions
                .retain(|&hash| hash != partition_hash);
        }
    }

    /// Notifies the StorageModuleService about partition assignment changes.
    ///
    /// Maps current storage modules to their partition assignments and sends
    /// this information to the StorageModuleService for processing.
    ///
    /// NOTE: Currently sends updates on every epoch block. Future optimization:
    /// only send when assignments actually change.
    ///
    /// # Returns
    /// Result indicating success or failure of the message send operation.
    pub fn notify_storage_module_service(&self) {
        let storage_module_infos =
            self.map_storage_modules_to_partition_assignments(&self.storage_submodules_config);
        // Create the message
        let message = StorageModuleServiceMessage::PartitionAssignmentsUpdated {
            storage_module_infos: storage_module_infos.into(),
        };

        // Send the message
        if let Err(e) = self.service_senders.storage_modules.send(message) {
            warn!("Failed to send partition assignments update: {}", e);
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
        cfg: &StorageSubmodulesConfig,
    ) -> Vec<StorageModuleInfo> {
        let miner = self.config.node_config.miner_address();
        let assignments = self.get_partition_assignments(miner);
        let num_chunks = self.config.consensus.num_chunks_in_partition as u32;
        let paths = &cfg.submodule_paths;

        let mut module_infos = Vec::new();

        // STEP 1: Publish ledger
        for pa in assignments
            .iter()
            .filter(|pa| pa.ledger_id == Some(DataLedger::Publish as u32))
        {
            let id = module_infos.len();
            let path = match paths.get(id) {
                Some(p) => p.clone(),
                None => {
                    error!("No available storage modules for partition assignment!");
                    return module_infos;
                }
            };
            module_infos.push(StorageModuleInfo {
                id,
                partition_assignment: Some(*pa),
                submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), path)],
            });
        }

        // STEP 2: Submit ledger
        for pa in assignments
            .iter()
            .filter(|pa| pa.ledger_id == Some(DataLedger::Submit as u32))
        {
            let id = module_infos.len();
            let path = match paths.get(id) {
                Some(p) => p.clone(),
                None => {
                    error!("No available storage modules for partition assignment!");
                    return module_infos;
                }
            };
            module_infos.push(StorageModuleInfo {
                id,
                partition_assignment: Some(*pa),
                submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), path)],
            });
        }

        // STEP 3: Capacity
        let remaining = paths.len().saturating_sub(module_infos.len());
        for pa in assignments
            .iter()
            .filter(|pa| pa.ledger_id.is_none())
            .take(remaining)
        {
            let id = module_infos.len();
            let path = match paths.get(id) {
                Some(p) => p.clone(),
                None => {
                    error!("No available storage modules for partition assignment!");
                    return module_infos;
                }
            };
            module_infos.push(StorageModuleInfo {
                id,
                partition_assignment: Some(*pa),
                submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), path)],
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
