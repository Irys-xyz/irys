use super::{CommitmentState, PartitionAssignments, PledgeEntry, StakeEntry};
use crate::{EpochBlockData, PACKING_PARAMS_FILE_NAME, PackingParams, StorageModuleInfo};
use eyre::{Error, Result};
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::{ExpiringPartitionInfo, Ledgers};
use irys_types::Config;
use irys_types::{
    CommitmentStatus, DataLedger, H256, IrysBlockHeader, NodeConfig, SimpleRNG, SystemLedger,
    partition::{PartitionAssignment, PartitionHash},
};
use irys_types::{
    CommitmentTransaction, ConsensusConfig, IrysAddress, PartitionChunkOffset,
    partition_chunk_offset_ie,
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
    /// Partition that expired with this snapshot (only happens at epoch boundaries)
    pub expired_partition_infos: Option<Vec<ExpiringPartitionInfo>>,
    /// Epoch block height this snapshot was computed for
    pub epoch_height: u64,
}

impl Default for EpochSnapshot {
    fn default() -> Self {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        Self {
            ledgers: Ledgers::new(&config.consensus, false),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: None, // This is only ever valid for test scenarios where epochs don't matter
            config: config.clone(),
            commitment_state: CommitmentState::default(),
            epoch_block: IrysBlockHeader::default(),
            previous_epoch_block: None,
            expired_partition_infos: None,
            epoch_height: IrysBlockHeader::default().height,
        }
    }
}

/// Reasons why the EpochSnapshot functions might fail
#[derive(Debug, thiserror::Error)]
pub enum EpochSnapshotError {
    /// Catchall error until more detailed errors are added
    #[error("internal epoch snapshot error")]
    InternalError,
    /// Attempted to do epoch tasks on a block that was not an epoch block
    #[error("target block is not an epoch boundary")]
    NotAnEpochBlock,
    /// Provided an incorrect previous epoch block
    #[error("provided previous epoch block does not match snapshot history")]
    IncorrectPreviousEpochBlock,
    /// Validation of commitments failed
    #[error("validation of epoch commitments failed: {0}")]
    InvalidCommitments(String),
    /// Unpledge targeted partition not found in assignments
    #[error(
        "unpledge target partition {partition_hash:?} for signer {signer:?} not found in assignments"
    )]
    UnpledgeTargetNotFound {
        partition_hash: H256,
        signer: IrysAddress,
    },
    /// Unpledge signer does not match assignment owner
    #[error(
        "unpledge signer mismatch for partition {partition_hash:?}: expected {expected:?}, got {actual:?}"
    )]
    UnpledgeSignerMismatch {
        partition_hash: H256,
        expected: IrysAddress,
        actual: IrysAddress,
    },
    /// Inconsistent state: partition appears in both capacity and data maps
    #[error("partition {partition_hash:?} present in both capacity and data assignments")]
    UnpledgeTargetInBothMaps { partition_hash: H256 },
    /// Unstake signer does not have an active stake
    #[error("unstake signer {signer:?} is not currently staked")]
    UnstakeSignerNotStaked { signer: IrysAddress },
    /// Unstake attempted while pledges still active
    #[error("unstake for signer {signer:?} has active pledges: {remaining}")]
    UnstakeHasActivePledges { signer: IrysAddress, remaining: u64 },
    /// UpdateRewardAddress signer does not have an active stake
    #[error("update reward address signer {signer:?} is not currently staked")]
    UpdateRewardAddressSignerNotStaked { signer: IrysAddress },
}

impl EpochSnapshot {
    /// Create a new instance of the epoch service actor
    pub fn new(
        storage_submodules_config: &StorageSubmodulesConfig,
        genesis_block: IrysBlockHeader,
        commitments: Vec<CommitmentTransaction>,
        config: &Config,
    ) -> Self {
        let cascade_active = config
            .consensus
            .hardforks
            .is_cascade_active(genesis_block.height);
        let mut new_self = Self {
            ledgers: Ledgers::new(&config.consensus, cascade_active),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: Some(storage_submodules_config.clone()),
            config: config.clone(),
            commitment_state: Default::default(),
            epoch_block: genesis_block.clone(),
            previous_epoch_block: None,
            expired_partition_infos: None,
            epoch_height: genesis_block.height,
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

    /// Resolves a miner address to their configured reward address.
    /// Returns the reward_address from the stake entry if present, falling back to miner_address
    /// if there is no stake entry.
    pub fn resolve_reward_address(&self, miner_address: IrysAddress) -> IrysAddress {
        self.commitment_state
            .stake_commitments
            .get(&miner_address)
            .map(|entry| entry.reward_address)
            .unwrap_or(miner_address)
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
                if !commitments.iter().any(|c| c.id() == *txid) {
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
                if !commitment_ledger.tx_ids.contains(&commitment.id()) {
                    return Err(eyre::eyre!(
                        "Extra commitment transaction {} not referenced in block {} ledger",
                        commitment.id(),
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
        if !block_header
            .height
            .is_multiple_of(self.config.consensus.epoch.num_blocks_in_epoch)
        {
            error!(
                "Not an epoch block height: {} num_blocks_in_epoch: {}",
                block_header.height, self.config.consensus.epoch.num_blocks_in_epoch
            );
            return Err(EpochSnapshotError::NotAnEpochBlock);
        }
        Ok(())
    }

    /// Main worker function
    #[tracing::instrument(level = "trace", skip_all, err, fields(
        block.hash = ?new_epoch_block.block_hash
    ))]
    pub fn perform_epoch_tasks(
        &mut self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
        new_epoch_commitments: Vec<CommitmentTransaction>,
    ) -> Result<(), EpochSnapshotError> {
        // Validate the epoch blocks
        self.is_epoch_block(new_epoch_block)?;

        self.epoch_height = new_epoch_block.height;

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
            .map_err(|e| EpochSnapshotError::InvalidCommitments(e.to_string()))?;

        debug!(
            block.height = new_epoch_block.height,
            block.hash = %new_epoch_block.block_hash,
            "\u{001b}[32mProcessing epoch block\u{001b}[0m"
        );

        self.epoch_block = new_epoch_block.clone();
        self.previous_epoch_block = previous_epoch_block.clone();

        // Activate Cascade hardfork ledgers at the activation epoch boundary
        if self
            .config
            .consensus
            .hardforks
            .is_cascade_active(new_epoch_block.height)
        {
            self.ledgers.activate_cascade(&self.config.consensus);
        }

        self.compute_commitment_state(&new_epoch_commitments);

        self.try_genesis_init(new_epoch_block);

        self.allocate_additional_ledger_slots(previous_epoch_block, new_epoch_block);

        self.expire_term_ledger_slots(new_epoch_block);

        self.apply_unpledges(&new_epoch_commitments)?;

        self.apply_unstakes(&new_epoch_commitments)?;

        self.apply_update_reward_addresses(&new_epoch_commitments)?;

        self.backfill_missing_partitions();

        self.allocate_additional_capacity();

        self.assign_partition_hashes_to_pledges();

        Ok(())
    }

    /// Applies unstake operations found in a set of epoch commitments.
    ///
    /// Behavior:
    /// - For each Unstake commitment, verify the signer has no remaining pledges
    ///   (after unpledges were applied). If any remain, return an error to invalidate the epoch.
    /// - Remove the signer's stake from the commitment state; failure to find a stake is an error.
    pub fn apply_unstakes(
        &mut self,
        commitments: &[CommitmentTransaction],
    ) -> Result<(), EpochSnapshotError> {
        for commitment in commitments {
            let irys_types::CommitmentTypeV2::Unstake = &commitment.commitment_type() else {
                continue;
            };
            let signer = commitment.signer();

            // Effective pledges after unpledges applied
            let remaining = self
                .commitment_state
                .pledge_commitments
                .get(&signer)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            if remaining > 0 {
                return Err(EpochSnapshotError::UnstakeHasActivePledges { signer, remaining });
            }

            // Remove stake entry; error if not found
            let existed = self.commitment_state.stake_commitments.remove(&signer);
            if existed.is_none() {
                return Err(EpochSnapshotError::UnstakeSignerNotStaked { signer });
            }

            debug!(tx.signer = %signer, "unstake_applied");
        }
        Ok(())
    }

    /// Applies update reward address operations found in a set of epoch commitments.
    ///
    /// Behavior:
    /// - Verifies the signer has an active stake before applying.
    /// - Updates the stake entry's reward_address field to the new address.
    /// - Panics if duplicate signers are detected (CommitmentSnapshot guarantees uniqueness).
    pub fn apply_update_reward_addresses(
        &mut self,
        commitments: &[CommitmentTransaction],
    ) -> Result<(), EpochSnapshotError> {
        let mut seen_signers: HashSet<IrysAddress> = HashSet::new();

        for commitment in commitments {
            let irys_types::CommitmentTypeV2::UpdateRewardAddress { new_reward_address } =
                &commitment.commitment_type()
            else {
                continue;
            };
            let signer = commitment.signer();

            // Panic on duplicate - CommitmentSnapshot should guarantee uniqueness
            assert!(
                seen_signers.insert(signer),
                "Data consistency fault: multiple UpdateRewardAddress transactions for signer {}",
                signer
            );

            let stake_entry = self
                .commitment_state
                .stake_commitments
                .get_mut(&signer)
                .ok_or(EpochSnapshotError::UpdateRewardAddressSignerNotStaked { signer })?;

            stake_entry.reward_address = *new_reward_address;

            debug!(
                tx.signer = %signer,
                tx.new_reward_address = %new_reward_address,
                "update_reward_address_applied"
            );
        }
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
            // Allocate 1 slot to each active ledger and calculate the number of partitions
            let mut num_data_partitions = 0;
            let active_ledgers = self.ledgers.active_ledgers();
            {
                // Create a scope for the write lock to expire with
                for ledger in &active_ledgers {
                    debug!("Allocating 1 slot for {:?}", ledger);
                    num_data_partitions +=
                        self.ledgers[*ledger].allocate_slots(1, new_epoch_block.height);
                }
            }

            // Calculate the total number of capacity partitions
            let projected_capacity_parts = Self::get_num_capacity_partitions(
                num_data_partitions,
                &self.config.consensus,
                active_ledgers.len() as u64,
            );

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
        let expired_partitions: Vec<ExpiringPartitionInfo> =
            self.ledgers.expire_term_partitions(epoch_height);

        // Return early if there's no more work to do
        if expired_partitions.is_empty() {
            return;
        }

        // NOTE: We used to do a broadcast here, now performed by the block_tree
        // when the epoch block is fully validated.
        // let mining_broadcaster_addr = BroadcastMiningService::from_registry();
        // mining_broadcaster_addr.do_send(BroadcastPartitionsExpiration(H256List(
        //     expired_hashes.clone(),
        // )));

        // Update expired data partitions assignments marking them as capacity partitions
        for partition_info in expired_partitions.iter() {
            self.return_expired_partition_to_capacity(partition_info.partition_hash);
        }

        self.expired_partition_infos = Some(expired_partitions);
    }

    /// Loops though all the ledgers both perm and term, checking to see if any
    /// require additional ledger slots added to accommodate data ingress.
    fn allocate_additional_ledger_slots(
        &mut self,
        previous_epoch_block: &Option<IrysBlockHeader>,
        new_epoch_block: &IrysBlockHeader,
    ) {
        for ledger in self.ledgers.active_ledgers() {
            // Skip ledgers not present in the block (e.g. pre-hardfork blocks)
            if !new_epoch_block
                .data_ledgers
                .iter()
                .any(|l| l.ledger_id == ledger as u32)
            {
                continue;
            }
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
        let num_capacity_partitions = Self::get_num_capacity_partitions(
            num_data_partitions,
            &self.config.consensus,
            self.ledgers.active_ledgers().len() as u64,
        );
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

        // Loop though all of the active ledgers processing their slot needs
        for ledger in self.ledgers.active_ledgers() {
            self.process_slot_needs(ledger, &mut capacity_partitions, &mut rng);
        }
    }

    /// Process slot needs for a given ledger, assigning partitions to each slot
    /// as needed. Accounts for not assigning the same mining address to multiple
    /// replicas of a single slot.
    #[tracing::instrument(level = "trace", skip_all, fields(ledger = ?ledger))]
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
    pub fn get_num_capacity_partitions(
        num_data_partitions: u64,
        config: &ConsensusConfig,
        num_active_ledgers: u64,
    ) -> u64 {
        use num_bigfloat::BigFloat;

        // Every active ledger needs at least one slot filled with data partitions
        let min_count = num_active_ledgers * config.num_partitions_per_slot;
        let base_count = std::cmp::max(num_data_partitions, min_count);

        // Deterministic computation using num-bigfloat (pure Rust, ~40 decimal digits)
        let base_f = BigFloat::from_u64(base_count);
        let log10 = base_f.log10();
        let trunc = bigfloat_truncate_to_3_decimals(&log10);

        let scalar = BigFloat::from_u64(config.epoch.capacity_scalar);
        let scaled = trunc * scalar;
        let scaled_trunc = bigfloat_truncate_to_3_decimals(&scaled);

        // Deterministic ceil to integer, then convert to u64
        scaled_trunc
            .ceil()
            .to_u64()
            .expect("ceiled value must be in 0..=u64::MAX")
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
                next_part_hash, prev_partition_hash
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

        // Safe lookup: the ledger may not exist in the block header (e.g. pre-Cascade blocks)
        let ledger_size = new_epoch_block
            .data_ledgers
            .iter()
            .find(|dl| dl.ledger_id == ledger as u32)
            .map(|dl| dl.total_chunks)
            .unwrap_or(0);

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
            let previous_ledger_size = previous_epoch_block.as_ref().map_or(0, |prev| {
                prev.data_ledgers
                    .iter()
                    .find(|dl| dl.ledger_id == ledger as u32)
                    .map(|dl| dl.total_chunks)
                    .unwrap_or(0)
            });

            let data_added = ledger_size.saturating_sub(previous_ledger_size);

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
    pub fn compute_commitment_state(&mut self, commitments: &[CommitmentTransaction]) {
        // Categorize commitments by their type for separate processing
        let mut stake_commitments: Vec<&CommitmentTransaction> = Vec::new();
        let mut pledge_commitments: Vec<&CommitmentTransaction> = Vec::new();
        for commitment_tx in commitments.iter() {
            match commitment_tx.commitment_type() {
                irys_types::CommitmentTypeV2::Stake => stake_commitments.push(commitment_tx),
                irys_types::CommitmentTypeV2::Pledge { .. } => {
                    pledge_commitments.push(commitment_tx)
                }
                // Unpledges are handled by `apply_unpledges()` during epoch processing
                irys_types::CommitmentTypeV2::Unpledge { .. } => {}
                // Unstakes are applied in `apply_unstakes()` after pledges have been processed
                irys_types::CommitmentTypeV2::Unstake => {}
                // UpdateRewardAddress is handled by `apply_update_reward_addresses()` during epoch processing
                irys_types::CommitmentTypeV2::UpdateRewardAddress { .. } => {}
            }
        }

        // Process stake commitments - these represent miners joining the network
        for stake_commitment in stake_commitments {
            // Register the commitment in the state
            // Assumption: Commitments are pre-validated, so we don't check for duplicates
            let value = StakeEntry {
                id: stake_commitment.id(),
                commitment_status: CommitmentStatus::Active,
                signer: stake_commitment.signer(),
                amount: stake_commitment.value(),
                // Initialize reward_address to signer - can be updated via UpdateRewardAddress tx
                reward_address: stake_commitment.signer(),
            };
            self.commitment_state
                .stake_commitments
                .insert(stake_commitment.signer(), value);
        }

        // Process pledge commitments - miners committing resources to the network
        for pledge_commitment in pledge_commitments {
            let address = pledge_commitment.signer();

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
            let value = PledgeEntry {
                id: pledge_commitment.id(),
                commitment_status: CommitmentStatus::Active,
                signer: pledge_commitment.signer(),
                amount: pledge_commitment.value(),
                partition_hash: None, // Assigned later via assign_partition_hashes_to_pledges
            };

            // Add the pledge state to the signer's collection (or create a new collection if first pledge)
            self.commitment_state
                .pledge_commitments
                .entry(address)
                .or_default()
                .push(value);
        }
    }

    /// Applies unpledge operations found in a set of epoch commitments.
    ///
    /// Behavior:
    /// - If the targeted partition is in `data_partitions`, remove it from the ledger slot
    ///   and from `data_partitions`, then add the partition hash to `unassigned_partitions`.
    /// - If the targeted partition is in `capacity_partitions`, remove it from that map and
    ///   add it to `unassigned_partitions`.
    /// - In all cases the partition remains in `all_active_partitions` (global ordering).
    /// - If the signer does not own the targeted partition return `UnpledgeSignerMismatch`.
    /// - If the targeted partition is not found in either map return `UnpledgeTargetNotFound`.
    /// - Remove the matching pledge entry (by `partition_hash`) from `commitment_state` when present.
    pub fn apply_unpledges(
        &mut self,
        commitments: &[CommitmentTransaction],
    ) -> Result<(), EpochSnapshotError> {
        for commitment in commitments {
            let irys_types::CommitmentTypeV2::Unpledge { partition_hash, .. } =
                &commitment.commitment_type()
            else {
                continue;
            };
            let signer = commitment.signer();
            let ph: irys_types::H256 = *partition_hash;

            let cap_present = self.partition_assignments.capacity_partitions.remove(&ph);
            let data_present = self.partition_assignments.data_partitions.remove(&ph);
            let partition_miner_address = match (data_present, cap_present) {
                (None, None) => {
                    warn!(
                        tx.partition_hash = %ph,
                        tx.signer = %signer,
                        "unpledge_target_not_found"
                    );
                    return Err(EpochSnapshotError::UnpledgeTargetNotFound {
                        partition_hash: ph,
                        signer,
                    });
                }
                (None, Some(capacity_partition)) => {
                    // no extra special handling
                    capacity_partition.miner_address
                }
                (Some(data_partition), None) => {
                    let ledger: DataLedger =
                        DataLedger::try_from(data_partition.ledger_id.unwrap())
                            .expect("valid ledger id");
                    let slot_index = data_partition
                        .slot_index
                        .expect("data partitions must have slot index present");
                    self.ledgers
                        .remove_partition_from_slot(ledger, slot_index, &ph);
                    data_partition.miner_address
                }
                (Some(_), Some(_)) => {
                    warn!(tx.partition_hash = %ph, "unpledge_target_in_both_maps");
                    return Err(EpochSnapshotError::UnpledgeTargetInBothMaps {
                        partition_hash: ph,
                    });
                }
            };
            if partition_miner_address != signer {
                // note: this should never happen as commitment_snapshot should validate the ownership beforehand
                return Err(EpochSnapshotError::UnpledgeSignerMismatch {
                    partition_hash: ph,
                    expected: partition_miner_address,
                    actual: signer,
                });
            }

            // Make available for reassignment
            self.add_to_unassigned_if_absent(ph);

            // Remove pledge entry by targeted partition hash (best-effort)
            if let Some(entries) = self.commitment_state.pledge_commitments.get_mut(&signer)
                && let Some(pos) = entries.iter().position(|e| e.partition_hash == Some(ph))
            {
                entries.remove(pos);
            }

            debug!(
                tx.partition_hash = %ph,
                tx.signer = %signer,
                "unpledge_retired_to_unassigned"
            );
        }
        Ok(())
    }

    /// Adds a partition hash to `unassigned_partitions`.
    fn add_to_unassigned_if_absent(&mut self, partition_hash: H256) {
        if self.unassigned_partitions.contains(&partition_hash) {
            return;
        }
        self.unassigned_partitions.push(partition_hash);
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
        let mut unassigned_pledges: Vec<PledgeEntry> = self
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
                "Assigned partition_hash {} to address {} for pledge {}",
                &partition_hash, &pledge.signer, &pledge.id
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
    pub fn get_partition_assignments(
        &self,
        miner_address: IrysAddress,
    ) -> Vec<PartitionAssignment> {
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
    /// This function has the same configuration dependency as [`irys_database::system_ledger::get_genesis_commitments`].
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
                            pa.last_updated_height.unwrap_or(self.epoch_height + 1) > self.epoch_height
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

    /// Returns partitions expiring at height + 1 to this [EpochSnapshot]. Empty unless next block is an epoch block with expiring partitions.
    ///
    /// Used during block production to produce epoch blocks with the correct term fee distributions.
    pub fn get_expiring_partition_info(&self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
        // expiring at next next block
        let ledgers = self.ledgers.clone();
        ledgers.get_expiring_term_partitions(epoch_height)
    }

    pub fn get_first_unexpired_slot_index(&self, ledger_id: DataLedger) -> usize {
        let slots = self.ledgers[ledger_id].get_slots();

        // Try to find the first unexpired slot
        for i in 0..slots.len() {
            if !slots[i].is_expired {
                return i;
            }
        }
        // This should only be the case with the published/permanent ledger data
        0
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
        miner: IrysAddress,
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
                last_updated_height: Some(self.epoch_height),
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

    pub fn is_staked(&self, miner_address: IrysAddress) -> bool {
        self.commitment_state.is_staked(miner_address)
    }

    // NON CANONICAL HASH
    // SHOULD BE USED FOR DEBUGGING ONLY
    pub fn get_hash(&self) -> String {
        let mut hasher = std::hash::DefaultHasher::new();
        use std::hash::Hash as _;
        self.ledgers.hash(&mut hasher);
        self.partition_assignments.hash(&mut hasher);
        self.all_active_partitions.hash(&mut hasher);
        self.unassigned_partitions.hash(&mut hasher);
        self.commitment_state.hash(&mut hasher);
        self.previous_epoch_block
            .as_ref()
            .map(|blk| blk.block_hash)
            .unwrap_or(H256::zero())
            .hash(&mut hasher);
        self.expired_partition_infos.hash(&mut hasher);
        self.epoch_height.hash(&mut hasher);
        use std::hash::Hasher as _;

        let res = hasher.finish();
        use base58::ToBase58 as _;
        res.to_le_bytes().to_base58()
    }
}

/// SHA256 hash the message parameter
fn hash_sha256(message: &[u8]) -> Result<[u8; 32], Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    let result = hasher.finish();
    Ok(result)
}

/// Truncate a `BigFloat` value to exactly 3 decimal places deterministically.
///
/// Multiplies by 1000, floors to integer, divides by 1000. Pure-Rust
/// `num-bigfloat` (~40 decimal digits) is deterministic across all platforms.
fn bigfloat_truncate_to_3_decimals(value: &num_bigfloat::BigFloat) -> num_bigfloat::BigFloat {
    let thousand = num_bigfloat::BigFloat::from_u32(1000);
    let tmp = *value * thousand;
    tmp.floor() / thousand
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
        let config = Config::new_with_random_peer_id(node_config);
        let mut snapshot = EpochSnapshot {
            ledgers: Ledgers::new(&config.consensus, false),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: None,
            config: config.clone(),
            commitment_state: CommitmentState::default(),
            epoch_block: IrysBlockHeader::default(),
            previous_epoch_block: None,
            expired_partition_infos: None,
            epoch_height: 0,
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
            header.data_ledgers[DataLedger::Submit].total_chunks = offset;
            let slots_to_add =
                snapshot.calculate_additional_slots(&None, &header, DataLedger::Submit);
            assert_eq!(slots_to_add, 0, "offset: {:?}", offset);
        }

        // Modify mock block so the submit ledger has an offset between 35 and 40 chunks. With the
        // correct capacity formula (4 slots * 10 chunks), this is above the
        // allocation threshold and should trigger two additional slots.
        for offset in 35..=40 {
            header.data_ledgers[DataLedger::Submit].total_chunks = offset;
            let slots_to_add =
                snapshot.calculate_additional_slots(&None, &header, DataLedger::Submit);
            assert_eq!(slots_to_add, 2, "offset: {:?}", offset);
        }

        // and test for more than 40 chunks
        // should produce the same result as 35..=40 as new slots are capped at 2 using the Threshold-based capacity expansion
        for offset in 41..=99 {
            header.data_ledgers[DataLedger::Submit].total_chunks = offset;
            let slots_to_add =
                snapshot.calculate_additional_slots(&None, &header, DataLedger::Submit);
            assert_eq!(slots_to_add, 2, "offset: {:?}", offset);
        }
    }

    mod validate_commitments {
        use crate::EpochSnapshot;
        use irys_types::SystemLedger;
        use irys_types::{ConsensusConfig, H256List, SystemTransactionLedger};

        #[test]
        fn should_check_that_all_commitments_are_included() {
            let config = ConsensusConfig::testing();
            let mut mocked_block = irys_types::IrysBlockHeader::new_mock_header();
            let mut comm_tx_1 = irys_types::CommitmentTransaction::new(&config);
            let mut comm_tx_2 = irys_types::CommitmentTransaction::new(&config);
            let mut unrelated_tx = irys_types::CommitmentTransaction::new(&config);

            comm_tx_1.set_id([1; 32].into());
            comm_tx_2.set_id([2; 32].into());
            unrelated_tx.set_id([3; 32].into());

            mocked_block.system_ledgers.push(SystemTransactionLedger {
                ledger_id: SystemLedger::Commitment.into(),
                tx_ids: H256List(vec![comm_tx_1.id(), comm_tx_2.id()]),
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
            assert_eq!(
                &err_str,
                "Commitment count mismatch for block 11111111111111111111111111111111: ledger has 2 commitments, but 1 commitments provided"
            );

            let err_str = EpochSnapshot::validate_commitments(&mocked_block, &too_many_commitments)
                .expect_err("Expected error for too many commitments")
                .to_string();
            assert_eq!(
                &err_str,
                "Commitment count mismatch for block 11111111111111111111111111111111: ledger has 2 commitments, but 3 commitments provided"
            );

            let err_str =
                EpochSnapshot::validate_commitments(&mocked_block, &valid_count_but_invalid_id)
                    .expect_err("Expected error for the wrong commitment ids")
                    .to_string();
            assert_eq!(
                err_str,
                "Missing commitment transaction 8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR for block 11111111111111111111111111111111"
            );
        }
    }

    mod unpledge_processing {
        use super::*;
        use irys_types::CommitmentStatus;
        use irys_types::{U256, transaction::CommitmentTypeV2};

        fn setup_snapshot_with_assignment(ph: H256, is_data: bool) -> EpochSnapshot {
            let mut snapshot = EpochSnapshot::default();
            let miner = snapshot.config.node_config.miner_address();

            // Add the partition to the appropriate assignment map
            let assignment = PartitionAssignment {
                partition_hash: ph,
                miner_address: miner,
                ledger_id: if is_data {
                    Some(DataLedger::Publish as u32)
                } else {
                    None
                },
                slot_index: if is_data { Some(0) } else { None },
            };

            if is_data {
                // Ensure a slot exists and the ledger contains the partition
                snapshot.ledgers[DataLedger::Publish].allocate_slots(1, snapshot.epoch_height);
                snapshot
                    .ledgers
                    .push_partition_to_slot(DataLedger::Publish, 0, ph);
                snapshot
                    .partition_assignments
                    .data_partitions
                    .insert(ph, assignment);
            } else {
                snapshot
                    .partition_assignments
                    .capacity_partitions
                    .insert(ph, assignment);
            }

            // Track as active
            snapshot.all_active_partitions.push(ph);

            // Seed a pledge entry tied to the partition hash
            let pledge_entry = PledgeEntry {
                id: H256::zero(),
                commitment_status: CommitmentStatus::Active,
                signer: miner,
                amount: U256::zero(),
                partition_hash: Some(ph),
            };
            snapshot
                .commitment_state
                .pledge_commitments
                .entry(miner)
                .or_default()
                .push(pledge_entry);

            snapshot
        }

        fn make_unpledge_tx(
            config: &ConsensusConfig,
            signer: IrysAddress,
            ph: H256,
        ) -> CommitmentTransaction {
            let mut tx = CommitmentTransaction::new(config);
            tx.set_signer(signer);
            tx.set_commitment_type(CommitmentTypeV2::Unpledge {
                pledge_count_before_executing: 1,
                partition_hash: ph,
            });
            tx
        }

        #[test]
        fn unpledge_removes_capacity_partition_and_pledge() {
            let ph = H256::random();
            let mut snapshot = setup_snapshot_with_assignment(ph, false);
            let signer = snapshot.config.node_config.miner_address();
            let tx = make_unpledge_tx(&snapshot.config.consensus, signer, ph);

            snapshot.apply_unpledges(&[tx]).unwrap();

            // Removed from capacity map
            assert!(
                !snapshot
                    .partition_assignments
                    .capacity_partitions
                    .contains_key(&ph)
            );

            // Still present in active set
            assert!(snapshot.all_active_partitions.contains(&ph));

            // Added to unassigned list
            assert!(snapshot.unassigned_partitions.contains(&ph));

            // Removed pledge entry
            let entries = snapshot
                .commitment_state
                .pledge_commitments
                .get(&signer)
                .unwrap();
            assert!(entries.is_empty());
        }

        #[test]
        fn unpledge_removes_data_partition_and_pledge() {
            let ph = H256::random();
            let mut snapshot = setup_snapshot_with_assignment(ph, true);
            let signer = snapshot.config.node_config.miner_address();
            let tx = make_unpledge_tx(&snapshot.config.consensus, signer, ph);

            snapshot.apply_unpledges(&[tx]).unwrap();

            // Removed from data map
            assert!(
                !snapshot
                    .partition_assignments
                    .data_partitions
                    .contains_key(&ph)
            );

            // Still present in active set
            assert!(snapshot.all_active_partitions.contains(&ph));

            // Removed from the ledger slot
            let slots = snapshot.ledgers.get_slots(DataLedger::Publish);
            let slot0 = &slots[0];
            assert!(!slot0.partitions.contains(&ph));

            // Added to unassigned list
            assert!(snapshot.unassigned_partitions.contains(&ph));

            // Removed pledge entry
            let entries = snapshot
                .commitment_state
                .pledge_commitments
                .get(&signer)
                .unwrap();
            assert!(entries.is_empty());
        }

        #[test]
        fn unpledge_target_not_found_returns_error() {
            let ph = H256::random();
            let mut snapshot = EpochSnapshot::default();
            let signer = snapshot.config.node_config.miner_address();
            let tx = make_unpledge_tx(&snapshot.config.consensus, signer, ph);

            // Should return an error and not add to unassigned
            let res = snapshot.apply_unpledges(&[tx]);
            assert!(res.is_err());
            assert!(!snapshot.unassigned_partitions.contains(&ph));
        }

        #[test]
        fn unpledge_duplicate_in_same_epoch_is_not_duplicated() {
            let ph = H256::random();
            let mut snapshot = setup_snapshot_with_assignment(ph, false);
            let signer = snapshot.config.node_config.miner_address();
            let tx = make_unpledge_tx(&snapshot.config.consensus, signer, ph);

            // First application succeeds
            snapshot.apply_unpledges(std::slice::from_ref(&tx)).unwrap();

            // Second application errors (target not found anymore)
            let res = snapshot.apply_unpledges(&[tx]);
            assert!(res.is_err());

            // Only one entry in unassigned
            let count = snapshot
                .unassigned_partitions
                .iter()
                .filter(|h| **h == ph)
                .count();
            assert_eq!(count, 1);
        }
    }
}
