use super::{CommitmentState, CommitmentStateEntry, PartitionAssignments};
use crate::{EpochBlockData, PackingParams, StorageModuleInfo, PACKING_PARAMS_FILE_NAME};
use eyre::{Error, Result};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::{data_ledger::*, SystemLedger};
use irys_primitives::CommitmentStatus;
use irys_storage::ie;
use irys_types::Config;
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    IrysBlockHeader, NodeConfig, SimpleRNG, H256,
};
use irys_types::{
    partition_chunk_offset_ie, Address, CommitmentTransaction, ConsensusConfig, DataLedger,
    PartitionChunkOffset,
};
use openssl::sha;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use tracing::{debug, error, trace, warn};

/// Temporarily track all of the ledger definitions inside the epoch service actor
#[derive(Debug, Clone)]
pub struct EpochSnapshot {
    /// Protocol-managed data ledgers (one permanent, N term)
    pub ledgers: Ledgers,
    /// Tracks active mining assignments for partitions (by hash)
    pub partition_assignments: PartitionAssignments,
    /// Sequential list of activated partition hashes
    pub all_active_partitions: Vec<PartitionHash>,
    /// List of partition hashes not yet assigned to a mining address
    pub unassigned_partitions: Vec<PartitionHash>,
    /// Submodules config
    pub storage_submodules_config: Option<StorageSubmodulesConfig>,
    /// Current partition & ledger parameters
    pub config: Config,
    /// Commitment state (all stakes and pledges) as computed at this epoch's start
    pub commitment_state: CommitmentState,
    /// The epoch block that was used to compute this snapshot
    pub epoch_block: IrysBlockHeader,
    /// The prior epoch block
    pub previous_epoch_block: Option<IrysBlockHeader>,
    /// Partition hashes that expired with this snapshot
    pub expired_partition_hashes: Vec<PartitionHash>,
    /// Epoch block height this snapshot was computed for
    pub height: u64,
}

impl Default for EpochSnapshot {
    fn default() -> Self {
        let node_config = NodeConfig::testing();
        let config = Config::new(node_config);
        Self {
            ledgers: Ledgers::new(&config.consensus),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: None, // This is only ever valid for test scenarios where epochs don't matter
            config: config.clone(),
            commitment_state: CommitmentState::default(),
            epoch_block: IrysBlockHeader::default(),
            previous_epoch_block: None,
            expired_partition_hashes: Vec::new(),
            height: IrysBlockHeader::default().height,
        }
    }
}

/// Reasons why the EpochSnapshot functions might fail
#[derive(Debug)]
pub enum EpochSnapshotError {
    /// Catchall error until more detailed errors are added
    InternalError,
    /// Attempted to do epoch tasks on a block that was not an epoch block
    NotAnEpochBlock,
    /// Provided an incorrect previous epoch block
    IncorrectPreviousEpochBlock,
    /// Validation of commitments failed
    InvalidCommitments,
}

impl EpochSnapshot {
    /// Create a new instance of the epoch service actor
    pub fn new(
        storage_submodules_config: &StorageSubmodulesConfig,
        genesis_block: IrysBlockHeader,
        commitments: Vec<CommitmentTransaction>,
        config: &Config,
    ) -> Self {
        let mut new_self = Self {
            ledgers: Ledgers::new(&config.consensus),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: Some(storage_submodules_config.clone()),
            config: config.clone(),
            commitment_state: Default::default(),
            epoch_block: genesis_block.clone(),
            previous_epoch_block: None,
            expired_partition_hashes: Vec::new(),
            height: genesis_block.height,
        };

        match new_self.perform_epoch_tasks(&None, &genesis_block, commitments) {
            Ok(_) => debug!("Initialized Epoch Snapshot"),
            Err(e) => {
                panic!("Error performing init tasks {:?}", e);
            }
        }

        // While we don't return these module infos, we call this method for validating of the storage modules
        let _storage_module_info = new_self.map_storage_modules_to_partition_assignments();

        new_self
    }

    pub fn replay_epoch_data(
        &mut self,
        epoch_replay_data: Vec<EpochBlockData>,
    ) -> eyre::Result<Vec<StorageModuleInfo>> {
        // Initialize as None for the first iteration
        let mut previous_epoch_block: Option<IrysBlockHeader> = self.previous_epoch_block.clone();

        for replay_data in epoch_replay_data {
            let block_header = replay_data.epoch_block;
            let commitments = replay_data.commitments;

            match self.perform_epoch_tasks(&previous_epoch_block, &block_header, commitments) {
                Ok(_expired_partition_hashes) => debug!("Processed replay epoch block"),
                Err(e) => {
                    return Err(eyre::eyre!("Error performing epoch tasks {:?}", e));
                }
            }

            // Store the owned block_header, not a reference
            previous_epoch_block = Some(block_header);
        }

        let storage_module_info = self.map_storage_modules_to_partition_assignments();

        Ok(storage_module_info)
    }

    fn validate_commitments(
        block_header: &IrysBlockHeader,
        commitments: &[CommitmentTransaction],
    ) -> eyre::Result<()> {
        // Extract the commitments ledger from the system ledgers in the epoch block
        let commitment_ledger = block_header
            .system_ledgers
            .iter()
            .find(|b| b.ledger_id == SystemLedger::Commitment);

        // Verify that each commitment transaction ID referenced in the commitments ledger has a
        // corresponding commitment transaction in the replay data
        if let Some(commitment_ledger) = commitment_ledger {
            // Ensure the counts match exactly - no extra commitments allowed
            if commitment_ledger.tx_ids.len() != commitments.len() {
                return Err(eyre::eyre!(
                    "Commitment count mismatch for block {:?}: ledger has {} commitments, but {} commitments provided",
                    block_header.block_hash,
                    commitment_ledger.tx_ids.len(),
                    commitments.len()
                ));
            }

            // Verify each commitment transaction ID in the ledger has a corresponding commitment
            for txid in commitment_ledger.tx_ids.iter() {
                // If we can't find the commitment transaction for a referenced txid, return an error
                if !commitments.iter().any(|c| c.id == *txid) {
                    return Err(eyre::eyre!(
                        "Missing commitment transaction {} for block {}",
                        txid,
                        block_header.block_hash
                    ));
                }
            }

            // Also check the other way around to verify each provided commitment is referenced in
            // the ledger (no extra commitments)
            for commitment in commitments.iter() {
                if !commitment_ledger.tx_ids.contains(&commitment.id) {
                    return Err(eyre::eyre!(
                        "Extra commitment transaction {} not referenced in block {} ledger",
                        commitment.id,
                        block_header.block_hash
                    ));
                }
            }
        } else {
            // If no commitment ledger exists, there should be no commitments provided
            if !commitments.is_empty() {
                return Err(eyre::eyre!(
                    "Block {} has no commitment ledger, but {} commitments were provided",
                    block_header.block_hash,
                    commitments.len()
                ));
            }
        }
        Ok(())
    }

    fn is_epoch_block(&self, block_header: &IrysBlockHeader) -> Result<(), EpochSnapshotError> {
        if block_header.height % self.config.consensus.epoch.num_blocks_in_epoch != 0 {
            error!(
                "Not an epoch block height: {} num_blocks_in_epoch: {}",
                block_header.height, self.config.consensus.epoch.num_blocks_in_epoch
            );
            return Err(EpochSnapshotError::NotAnEpochBlock);
        }
        Ok(())
    }

    /// Main worker function
    pub fn perform_epoch_tasks(
        &mut self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
        new_epoch_commitments: Vec<CommitmentTransaction>,
    ) -> Result<(), EpochSnapshotError> {
        // Validate the epoch blocks
        self.is_epoch_block(new_epoch_block)?;

        self.height = new_epoch_block.height;

        // Skip previous block validation for genesis block (height 0)
        if new_epoch_block.height <= self.config.consensus.epoch.num_blocks_in_epoch {
            // Continue with validation logic for commitments
        } else {
            // For non-genesis blocks, previous epoch block must exist and have correct height
            let prev_block = previous_epoch_block
                .as_ref()
                .ok_or(EpochSnapshotError::IncorrectPreviousEpochBlock)?;

            // Validate the previous epoch block is the correct height
            if prev_block.height + self.config.consensus.epoch.num_blocks_in_epoch
                != new_epoch_block.height
            {
                return Err(EpochSnapshotError::IncorrectPreviousEpochBlock);
            }
        }

        // Validate the commitments
        Self::validate_commitments(new_epoch_block, &new_epoch_commitments)
            .map_err(|_| EpochSnapshotError::InvalidCommitments)?;

        debug!(
            height = new_epoch_block.height,
            block_hash = %new_epoch_block.block_hash,
            "\u{001b}[32mProcessing epoch block\u{001b}[0m"
        );

        self.epoch_block = new_epoch_block.clone();
        self.previous_epoch_block = previous_epoch_block.clone();

        self.compute_commitment_state(new_epoch_commitments);

        self.try_genesis_init(new_epoch_block);

        self.allocate_additional_ledger_slots(previous_epoch_block, new_epoch_block);

        self.expire_term_ledger_slots(new_epoch_block);

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
            // Allocate 1 slot to each ledger and calculate the number of partitions
            let mut num_data_partitions = 0;
            {
                // Create a scope for the write lock to expire with
                for ledger in DataLedger::iter() {
                    debug!("Allocating 1 slot for {:?}", &ledger);
                    num_data_partitions +=
                        self.ledgers[ledger].allocate_slots(1, new_epoch_block.height);
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
    /// Stores a vec of expired partition hashes in the epoch snapshot
    fn expire_term_ledger_slots(&mut self, new_epoch_block: &IrysBlockHeader) {
        let epoch_height = new_epoch_block.height;
        let expired_hashes: Vec<H256> = self.ledgers.get_expired_partition_hashes(epoch_height);

        // Return early if there's no more work to do
        if expired_hashes.is_empty() {
            return;
        }

        // NOTE: We used to do a broadcast here, now performed by the block_tree
        // when the epoch block is fully validated.
        // let mining_broadcaster_addr = BroadcastMiningService::from_registry();
        // mining_broadcaster_addr.do_send(BroadcastPartitionsExpiration(H256List(
        //     expired_hashes.clone(),
        // )));

        // Update expired data partitions assignments marking them as capacity partitions
        for partition_hash in expired_hashes.iter() {
            self.return_expired_partition_to_capacity(*partition_hash);
        }

        self.expired_partition_hashes = expired_hashes;
    }

    /// Loops though all the ledgers both perm and term, checking to see if any
    /// require additional ledger slots added to accommodate data ingress.
    fn allocate_additional_ledger_slots(
        &mut self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
    ) {
        for ledger in DataLedger::iter() {
            let part_slots =
                self.calculate_additional_slots(previous_epoch_block, new_epoch_block, ledger);
            debug!("Allocating {} slots for ledger {:?}", &part_slots, &ledger);
            self.ledgers[ledger].allocate_slots(part_slots, new_epoch_block.height);
        }
    }

    /// Based on the amount of data in each ledger, this function calculates
    /// the number of partitions the protocol should be managing and allocates
    /// additional partitions (and their state) as needed.
    fn allocate_additional_capacity(&mut self) {
        debug!("Allocating additional capacity");
        // Calculate total number of active partitions based on the amount of data stored
        let pa = &self.partition_assignments;
        let num_data_partitions = pa.data_partitions.len() as u64;
        let num_capacity_partitions =
            Self::get_num_capacity_partitions(num_data_partitions, &self.config.consensus);
        let total_parts = num_capacity_partitions + num_data_partitions;

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
        // Now collecting both the hash and the partition assignment

        let mut capacity_partitions: Vec<(H256, PartitionAssignment)> = self
            .partition_assignments
            .capacity_partitions
            .iter()
            .map(|(hash, assignment)| (*hash, *assignment))
            .collect();

        // Sort partitions by hash using `sort_unstable_by_key` for better performance.
        // Stability isn't needed/affected as each partition hash is unique.
        capacity_partitions.sort_unstable_by_key(|(hash, _)| *hash);

        // Use the previous epoch hash as a seed/entropy to the prng
        let seed = self.epoch_block.last_epoch_hash.to_u32();
        debug!("RNG seed: {}", self.epoch_block.last_epoch_hash);
        let mut rng = SimpleRNG::new(seed);

        // Loop though all of the ledgers processing their slot needs
        for ledger in DataLedger::iter() {
            self.process_slot_needs(ledger, &mut capacity_partitions, &mut rng);
        }
    }

    /// Process slot needs for a given ledger, assigning partitions to each slot
    /// as needed. Accounts for not assigning the same mining address to multiple
    /// replicas of a single slot.
    pub fn process_slot_needs(
        &mut self,
        ledger: DataLedger,
        capacity_partitions: &mut Vec<(H256, PartitionAssignment)>,
        rng: &mut SimpleRNG,
    ) {
        debug!("Processing slot needs for ledger {:?}", &ledger);
        // Get slot needs for the specified ledger
        let slot_needs = self.ledgers.get_slot_needs(ledger);

        // Iterate over slots that need partitions and assign them
        for (slot_index, num_needed) in slot_needs {
            // Build a set of the mining addresses already assigned to this slot
            let slot = self
                .ledgers
                .get_slots(ledger)
                .get(slot_index)
                .expect("slot index should exist in the ledger");

            let mut assigned_addresses: HashSet<_> = HashSet::new();
            for partition_hash in &slot.partitions {
                let pa = self
                    .get_data_partition_assignment(*partition_hash)
                    .expect("a partition in a data ledger slot should have a partition assignment");

                assigned_addresses.insert(pa.miner_address);
            }

            // Create a slot specific view of the capacity partitions
            let mut slot_capacity_partitions = capacity_partitions.clone();

            // Filter out any mining addresses already in the slot from the slot specific capacity view
            slot_capacity_partitions.retain(|(_, partition_assignment)| {
                !assigned_addresses.contains(&partition_assignment.miner_address)
            });

            for _ in 0..num_needed {
                if slot_capacity_partitions.is_empty() {
                    warn!(
                        "No available capacity partitions (needs {}) for slot {} of ledger {:?}",
                        &num_needed, &slot_index, &ledger
                    );
                    break; // Exit if no more available hashes
                }

                // Pick a random capacity partition hash and assign it
                let part_index = rng.next_range(slot_capacity_partitions.len() as u32) as usize;
                let (partition_hash, pa) = slot_capacity_partitions.swap_remove(part_index);

                // Update local PartitionAssignment state and add to data_partitions
                self.assign_partition_to_slot(partition_hash, ledger, slot_index);

                // Remove any partitions belonging to the assigned miner from the slot capacity view
                // So no more partitions from this miner will be assigned to this slot
                slot_capacity_partitions
                    .retain(|(_, part_assign)| part_assign.miner_address != pa.miner_address);

                // Remove the specific assigned partition hash from the global capacity partitions list
                capacity_partitions.retain(|(part_hash, _)| (*part_hash) != partition_hash);

                // Push the newly assigned partition hash to the appropriate slot
                // in the ledger
                debug!(
                    "Assigning partition hash {} to slot {} for {:?} addr: {}",
                    partition_hash, &slot_index, &ledger, pa.miner_address
                );

                self.ledgers
                    .push_partition_to_slot(ledger, slot_index, partition_hash);
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
        let mut prev_partition_hash = match self.all_active_partitions.last() {
            Some(last_hash) => *last_hash,
            None => self.epoch_block.last_epoch_hash,
        };

        debug!("Adding {} capacity partitions", &parts_to_add);
        // Compute the partition hashes for all of the added partitions
        for _i in 0..parts_to_add {
            let next_part_hash = H256(hash_sha256(&prev_partition_hash.0).unwrap());
            trace!(
                "Adding partition with hash: {} (prev: {})",
                next_part_hash,
                prev_partition_hash
            );
            self.all_active_partitions.push(next_part_hash);
            // All partition_hashes begin as unassigned capacity partitions
            self.unassigned_partitions.push(next_part_hash);
            prev_partition_hash = next_part_hash;
        }
    }

    // Updates PartitionAssignment information about a partition hash, marking
    // it as expired (or unassigned to a slot in a data ledger)
    fn return_expired_partition_to_capacity(&mut self, partition_hash: H256) {
        // Convert data partition to capacity partition if it exists
        if let Some(mut assignment) = self
            .partition_assignments
            .data_partitions
            .remove(&partition_hash)
        {
            // Remove the partition hash from the slots state
            let ledger: DataLedger = DataLedger::try_from(assignment.ledger_id.unwrap()).unwrap();
            let partition_hash = assignment.partition_hash;
            let slot_index = assignment.slot_index.unwrap();
            self.ledgers
                .remove_partition_from_slot(ledger, slot_index, &partition_hash);

            // Clear ledger assignment
            assignment.ledger_id = None;
            assignment.slot_index = None;

            // Return the partition hash to the capacity pool
            self.partition_assignments
                .capacity_partitions
                .insert(partition_hash, assignment);
        }
    }

    /// Takes a capacity partition hash and updates its `PartitionAssignment`
    /// state to indicate it is part of a data ledger
    fn assign_partition_to_slot(
        &mut self,
        partition_hash: H256,
        ledger: DataLedger,
        slot_index: usize,
    ) {
        debug!(
            "Assigning partition {} to slot {} of ledger {:?}",
            &partition_hash, &slot_index, &ledger
        );
        if let Some(mut assignment) = self
            .partition_assignments
            .capacity_partitions
            .remove(&partition_hash)
        {
            assignment.ledger_id = Some(ledger as u32);
            assignment.slot_index = Some(slot_index);
            self.partition_assignments
                .data_partitions
                .insert(partition_hash, assignment);
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
        let data_ledger = &self.ledgers[ledger];
        let num_slots = data_ledger.slot_count() as u64;

        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;

        // Ledger Partitioning Model:
        //
        // - Ledgers are divided into 16TB "slots" containing the canonical data
        // - Each slot is replicated across multiple "partitions" stored by different miners
        // - Ledger capacity is calculated from canonical data size, not total replica storage
        // - New slots are added when canonical data approaches capacity limits
        //
        // Example: A ledger with 32TB of data uses 2 slots, regardless of whether
        // there are 2 or 10 partition replicas of each slot across the network.
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
                irys_primitives::CommitmentType::Pledge { .. } => {
                    pledge_commitments.push(commitment_tx)
                }
                _ => unimplemented!(),
            }
        }

        // Process stake commitments - these represent miners joining the network
        for stake_commitment in stake_commitments {
            // Register the commitment in the state
            // Assumption: Commitments are pre-validated, so we don't check for duplicates
            let value = CommitmentStateEntry {
                id: stake_commitment.id,
                commitment_status: CommitmentStatus::Active,
                partition_hash: None,
                signer: stake_commitment.signer,
                amount: stake_commitment.commitment_value(),
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
                panic!("Invalid commitments found in epoch block");
            }

            // Create the state entry for the pledge commitment
            let value = CommitmentStateEntry {
                id: pledge_commitment.id,
                commitment_status: CommitmentStatus::Active,
                partition_hash: None,
                signer: pledge_commitment.signer,
                amount: pledge_commitment.commitment_value(),
            };

            // Add the pledge state to the signer's collection (or create a new collection if first pledge)
            self.commitment_state
                .pledge_commitments
                .entry(address)
                .or_default()
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

        // Loop through both lists assigning capacity partitions to pledges
        for pledge in &unassigned_pledges {
            // Get the next available partition hash
            let Some(partition_hash) = unassigned_parts.pop_front() else {
                break; // No more partitions available
            };

            // Try to get all the pledge commitments for a particular address
            let Some(entries) = self
                .commitment_state
                .pledge_commitments
                .get_mut(&pledge.signer)
            else {
                continue; // No pledges for this signer, skip to next
            };

            // Find the specific pledge entry by ID
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == pledge.id) else {
                continue; // Pledge not found, skip to next
            };

            // Assign the partition hash to this pledge
            entry.partition_hash = Some(partition_hash);

            // Update partition assignments state to reference the assigned address
            self.partition_assignments.capacity_partitions.insert(
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
        let pa = &self.partition_assignments;

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
    pub fn map_storage_modules_to_partition_assignments(&self) -> Vec<StorageModuleInfo> {
        let miner = self.config.node_config.miner_address();
        let assignments = self.get_partition_assignments(miner);
        let num_chunks = self.config.consensus.num_chunks_in_partition as u32;
        let paths = &self
            .storage_submodules_config
            .as_ref()
            .unwrap()
            .submodule_paths;

        // Collect existing storage module packing info
        let mut sm_packing_info = self.collect_packing_info(paths);
        debug!("Packing info: {:#?}", &sm_packing_info);

        let mut module_infos = Vec::new();

        // STEP 1: Publish ledger
        self.process_storage_module_assignments(
            &assignments,
            &mut sm_packing_info,
            &mut module_infos,
            Some(DataLedger::Publish as u32),
            miner,
            num_chunks,
        );

        // STEP 2: Submit ledger
        self.process_storage_module_assignments(
            &assignments,
            &mut sm_packing_info,
            &mut module_infos,
            Some(DataLedger::Submit as u32),
            miner,
            num_chunks,
        );

        // STEP 3: Capacity (with remaining slots limit)
        self.process_storage_module_assignments(
            &assignments,
            &mut sm_packing_info,
            &mut module_infos,
            None, // Capacity assignments have no ledger_id
            miner,
            num_chunks,
        );

        // STEP 4: Unassigned & assignments from the future
        for (original_idx, (path, _)) in
            sm_packing_info
                .iter()
                .enumerate()
                .filter(|(_, (_, params))| {
                    params.is_none()
                    // if an assignment exists, and is from the future, we pass it through
                        || params.is_some_and(|pa| {
                            pa.last_updated_height.unwrap_or(self.height + 1) > self.height
                        })
                })
        {
            module_infos.push(StorageModuleInfo {
                id: original_idx,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), path.clone())],
            });
        }

        module_infos
    }

    /// Loops though all the paths in the storage_submodules.toml and attempts to read the existing
    /// packing params from that path, building a list of (PathBuf, Option<PackingParams>) whose
    /// indexes map to the index of the path in the .toml
    fn collect_packing_info(&self, paths: &[PathBuf]) -> Vec<(PathBuf, Option<PackingParams>)> {
        paths
            .iter()
            .map(|path| {
                let sub_base_path = self.config.node_config.base_directory.join(path.clone());
                let params_path = sub_base_path.join(PACKING_PARAMS_FILE_NAME);

                let params = if params_path.exists() {
                    Some(PackingParams::from_toml(&params_path).expect("packing params to load"))
                } else {
                    None
                };

                (path.clone(), params)
            })
            .collect()
    }

    /// Processes partition assignments for a specific ledger, creating StorageModuleInfo entries and updating packing state.
    /// The StorageModuleInfos will reference existing storage modules if that store the partition assignment or use the first
    /// available storage module index with no partition assignments.
    fn process_storage_module_assignments(
        &self,
        assignments: &[PartitionAssignment],
        sm_packing_info: &mut [(PathBuf, Option<PackingParams>)],
        module_infos: &mut Vec<StorageModuleInfo>,
        target_ledger_id: Option<u32>,
        miner: Address,
        num_chunks: u32,
    ) {
        let filtered_assignments = assignments
            .iter()
            .filter(|pa| pa.ledger_id == target_ledger_id);

        let assignments_to_process = filtered_assignments.collect::<Vec<_>>();

        for pa in assignments_to_process {
            let id = self.find_or_assign_storage_module(sm_packing_info, pa);

            if id.is_none() {
                error!("No available storage modules for partition assignment!");
                return;
            }

            let id = id.unwrap();
            let path = sm_packing_info[id].0.clone();

            module_infos.push(StorageModuleInfo {
                id,
                partition_assignment: Some(*pa),
                submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), path)],
            });

            // Update packing info (so this storage module doesn't get assigned to something else)
            sm_packing_info[id].1 = Some(PackingParams {
                packing_address: miner,
                partition_hash: Some(pa.partition_hash),
                ledger: pa.ledger_id,
                slot: pa.slot_index,
                last_updated_height: Some(self.height),
            });
        }
    }

    /// Finds storage module index for partition: returns existing assignment or first
    /// available storage module index with no partition assignment.
    fn find_or_assign_storage_module(
        &self,
        sm_packing_info: &[(PathBuf, Option<PackingParams>)],
        pa: &PartitionAssignment,
    ) -> Option<usize> {
        // Try to find existing partition assignment
        let existing_id = sm_packing_info.iter().position(|(_, params)| {
            params
                .as_ref()
                .is_some_and(|pp| pp.partition_hash == Some(pa.partition_hash))
        });

        // If not found, find first available storage module index/id with no assignment
        existing_id.or_else(|| {
            sm_packing_info.iter().position(|(_, params)| match params {
                // If there's no packing params its a fresh storage module
                None => true,
                // but on restart the storage module may have an "empty" packing params
                // See StorageModule::new() for why.
                Some(p) => p.partition_hash.is_none(),
            })
        })
    }

    /// Gets a partitions assignment to a data partition by partition hash
    pub fn get_data_partition_assignment(
        &self,
        partition_hash: PartitionHash,
    ) -> Option<PartitionAssignment> {
        self.partition_assignments.get_assignment(partition_hash)
    }

    pub fn is_staked(&self, miner_address: Address) -> bool {
        self.commitment_state.is_staked(miner_address)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitmentState, PartitionAssignments};
    use irys_database::data_ledger::Ledgers;

    use irys_types::{
        Config, ConsensusConfig, ConsensusOptions, DataLedger, IrysBlockHeader, NodeConfig,
    };

    /// Validate that `calculate_additional_slots` allocates new slots when the
    /// ledger usage reaches the capacity threshold.
    #[test]
    fn threshold_based_allocation() {
        // Build a snapshot with a custom consensus configuration where each
        // slot can hold two partitions
        let mut node_config = NodeConfig::testing();
        node_config.consensus = ConsensusOptions::Custom(ConsensusConfig {
            num_partitions_per_slot: 2,
            ..node_config.consensus_config()
        });
        let config = Config::new(node_config);
        let mut snapshot = EpochSnapshot {
            ledgers: Ledgers::new(&config.consensus),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: None,
            config: config.clone(),
            commitment_state: CommitmentState::default(),
            epoch_block: IrysBlockHeader::default(),
            previous_epoch_block: None,
            expired_partition_hashes: Vec::new(),
            height: 0,
        };

        // Allocate four slots in the submit ledger so `slot_count()` returns 4.
        snapshot.ledgers[DataLedger::Submit].allocate_slots(4, 0);

        // mock header
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = 0;
        // Modify mock block so the submit ledger has an offset up to 34 chunks. With the
        // correct capacity formula (4 slots * 10 chunks), this is below the
        // allocation threshold and should trigger two additional slots.
        for offset in 1..=34 {
            header.data_ledgers[DataLedger::Submit].max_chunk_offset = offset;
            let slots_to_add =
                snapshot.calculate_additional_slots(&None, &header, DataLedger::Submit);
            assert_eq!(slots_to_add, 0, "offset: {:?}", offset);
        }

        // Modify mock block so the submit ledger has an offset between 35 and 40 chunks. With the
        // correct capacity formula (4 slots * 10 chunks), this is above the
        // allocation threshold and should trigger two additional slots.
        for offset in 35..=40 {
            header.data_ledgers[DataLedger::Submit].max_chunk_offset = offset;
            let slots_to_add =
                snapshot.calculate_additional_slots(&None, &header, DataLedger::Submit);
            assert_eq!(slots_to_add, 2, "offset: {:?}", offset);
        }

        // and test for more than 40 chunks
        // should produce the same result as 35..=40 as new slots are capped at 2 using the Threshold-based capacity expansion
        for offset in 41..=99 {
            header.data_ledgers[DataLedger::Submit].max_chunk_offset = offset;
            let slots_to_add =
                snapshot.calculate_additional_slots(&None, &header, DataLedger::Submit);
            assert_eq!(slots_to_add, 2, "offset: {:?}", offset);
        }
    }

    mod validate_commitments {
        use crate::EpochSnapshot;
        use irys_database::SystemLedger;
        use irys_types::{ConsensusConfig, H256List, SystemTransactionLedger};

        #[test]
        fn should_check_that_all_commitments_are_included() {
            let config = ConsensusConfig::testing();
            let mut mocked_block = irys_types::IrysBlockHeader::new_mock_header();
            let mut comm_tx_1 = irys_types::CommitmentTransaction::new(&config);
            let mut comm_tx_2 = irys_types::CommitmentTransaction::new(&config);
            let mut unrelated_tx = irys_types::CommitmentTransaction::new(&config);

            comm_tx_1.id = [1; 32].into();
            comm_tx_2.id = [2; 32].into();
            unrelated_tx.id = [3; 32].into();

            mocked_block.system_ledgers.push(SystemTransactionLedger {
                ledger_id: SystemLedger::Commitment.into(),
                tx_ids: H256List(vec![comm_tx_1.id, comm_tx_2.id]),
            });

            let valid_commitments = vec![comm_tx_1.clone(), comm_tx_2.clone()];
            let too_few_commitments = vec![comm_tx_1.clone()];
            let too_many_commitments = vec![comm_tx_1.clone(), comm_tx_2.clone(), comm_tx_2];
            let valid_count_but_invalid_id = vec![comm_tx_1, unrelated_tx];

            let res = EpochSnapshot::validate_commitments(&mocked_block, &valid_commitments);
            assert!(res.is_ok());

            let err_str = EpochSnapshot::validate_commitments(&mocked_block, &too_few_commitments)
                .expect_err("Expected error for too many commitments")
                .to_string();
            assert_eq!(&err_str, "Commitment count mismatch for block 11111111111111111111111111111111: ledger has 2 commitments, but 1 commitments provided");

            let err_str = EpochSnapshot::validate_commitments(&mocked_block, &too_many_commitments)
                .expect_err("Expected error for too many commitments")
                .to_string();
            assert_eq!(&err_str, "Commitment count mismatch for block 11111111111111111111111111111111: ledger has 2 commitments, but 3 commitments provided");

            let err_str =
                EpochSnapshot::validate_commitments(&mocked_block, &valid_count_but_invalid_id)
                    .expect_err("Expected error for the wrong commitment ids")
                    .to_string();
            assert_eq!(err_str, "Missing commitment transaction 8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR for block 11111111111111111111111111111111");
        }
    }
}
