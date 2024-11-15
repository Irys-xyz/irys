use actix::{Actor, Context, Handler, Message};
use database::data_ledger::*;
use eyre::{Error, Result};
use irys_types::{
    Address, IrysBlockHeader, CAPACITY_SCALAR, H256, NUM_BLOCKS_IN_EPOCH, NUM_PARTITIONS_PER_SLOT,
    PARTITION_SIZE, U256,
};
use openssl::sha;
use std::collections::HashMap;

/// Temporarily track all of the ledger definitions inside the epoch service actor
#[derive(Debug)]
pub struct EpochServiceActor {
    /// The previous epoch hash is used in the current epochs tasks as a reliable source of randomness
    pub last_epoch_hash: H256,
    /// Encapsulates a list of data ledgers currently managed by the protocol
    pub ledgers: Ledgers,
    /// Data partition assignments mapped by partition hash
    pub data_partitions: HashMap<H256, PartitionAssignment>,
    /// Capacity partition assignments mapped by partition hash
    pub capacity_partitions: HashMap<H256, PartitionAssignment>,
    /// Ordered sequence of active partition hashes
    pub all_active_partitions: Vec<H256>,
    /// Address of the local miner
    pub miner_address: Address,
}

impl Actor for EpochServiceActor {
    type Context = Context<Self>;
}

/// Sent when a new epoch block is reached (and at genesis)
#[derive(Message, Debug)]
#[rtype(result = "Result<(),EpochServiceError>")]
pub struct NewEpochMessage(pub IrysBlockHeader);

impl NewEpochMessage {
    fn into_inner(self) -> IrysBlockHeader {
        self.0
    }
}

/// Reasons why the epoch service actors epoch tasks might fail
#[derive(Debug)]
pub enum EpochServiceError {
    /// Catchall error until more detailed errors are added
    InternalError,
    NotAnEpochBlock,
}

impl Handler<NewEpochMessage> for EpochServiceActor {
    type Result = Result<(), EpochServiceError>;
    fn handle(&mut self, msg: NewEpochMessage, ctx: &mut Self::Context) -> Self::Result {
        let new_epoch_block = msg.0;

        self.perform_epoch_tasks(new_epoch_block)?;

        Ok(())
    }
}

/// Temporary struct tracking partition assignments to miners - will be moved to database
#[derive(Debug, PartialEq)]
pub struct PartitionAssignment {
    /// Hash of the partition
    pub partition_hash: H256,
    /// Address of the miner pledged to store it
    pub miner_address: Address,
    /// If assigned to a ledger, the ledger number
    pub ledger_num: Option<u64>,
    /// If assigned to a ledger, the index in the ledger
    pub slot_index: Option<usize>,
}

impl EpochServiceActor {
    /// Create a new instance of the epoch service actor
    pub fn new(miner_address: Address) -> Self {
        Self {
            last_epoch_hash: H256::zero(),
            ledgers: Ledgers::new(),
            data_partitions: HashMap::new(),
            capacity_partitions: HashMap::new(),
            all_active_partitions: Vec::new(),
            miner_address,
        }
    }

    /// Main worker function
    pub fn perform_epoch_tasks(
        &mut self,
        new_epoch_block: IrysBlockHeader,
    ) -> Result<(), EpochServiceError> {
        // Validate this is an epoch block height
        if new_epoch_block.height % NUM_BLOCKS_IN_EPOCH != 0 {
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
            for ledger_num in Ledger::iter() {
                num_data_partitions += self.ledgers[ledger_num].allocate_slots(1);
            }

            // Calculate the total number of partitions
            let num_partitions = num_data_partitions
                + Self::get_num_capacity_partitions(num_data_partitions, CAPACITY_SCALAR);

            self.add_capacity_partitions(num_partitions);
        }
    }

    /// Loops though all of the term ledgers and looks for slots that are older
    /// than the epoch_length (term length) of the ledger.
    fn expire_term_ledger_slots(&mut self, new_epoch_block: &IrysBlockHeader) {
        let epoch_height = new_epoch_block.height;
        let expired_hashes = self.ledgers.get_expired_partition_hashes(epoch_height);

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
            self.ledgers[ledger_num].allocate_slots(part_slots);
        }
    }

    /// Based on the amount of data in each ledger, this function calculates
    /// the number of partitions the protocol should be managing and allocates
    /// additional partitions (and their state) as needed.
    fn allocate_additional_capacity(&mut self) {
        // Calculate total number of active partitions based on the amount of data stored
        let num_data_partitions = self.data_partitions.len() as u64;
        let num_capacity_partitions =
            Self::get_num_capacity_partitions(num_data_partitions, CAPACITY_SCALAR);
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
        // Start with a sorted list of capacity partitions (sorted by hash)
        let mut capacity_partitions: Vec<H256> = self.capacity_partitions.keys().copied().collect();

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
        let slot_needs = self.ledgers.get_slot_needs(ledger);
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
                self.ledgers
                    .push_partition_to_slot(ledger, slot_index, partition_hash);
            }
        }
    }

    /// Computes active capacity partitions available for pledges based on
    /// data partitions and scaling factor
    fn get_num_capacity_partitions(data_partition_count: u64, scalar: u64) -> u64 {
        // Every ledger needs at least one slot filled with data partitions
        let min_count = Ledger::ALL.len() as u64 * NUM_PARTITIONS_PER_SLOT;
        let base_count = std::cmp::max(data_partition_count, min_count);
        let log_10 = (base_count as f64).log10();
        let trunc = truncate_to_3_decimals(log_10);
        let scaled = truncate_to_3_decimals(trunc * scalar as f64);
        let rounded = truncate_to_3_decimals(scaled).ceil() as u64;
        // println!(
        //     "- base_count: {}, log_10: {}, trunc: {}, scaled: {}, rounded: {}",
        //     base_count, log_10, trunc, scaled, rounded
        // );
        rounded
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
        for partition_hash in &self.all_active_partitions {
            self.capacity_partitions.insert(
                *partition_hash,
                PartitionAssignment {
                    partition_hash: *partition_hash,
                    miner_address: self.miner_address.clone(),
                    ledger_num: None,
                    slot_index: None,
                },
            );
        }
    }

    // Updates PartitionAssignment information about a partition hash, marking
    // it as expired (or unassigned to a slot in a data ledger)
    fn mark_partition_as_expired(&mut self, partition_hash: H256) {
        // Convert data partition to capacity partition if it exists
        if let Some(mut assignment) = self.data_partitions.remove(&partition_hash) {
            // Clear ledger assignment
            assignment.ledger_num = None;
            assignment.slot_index = None;

            // Add to capacity pool
            self.capacity_partitions.insert(partition_hash, assignment);
        }
    }

    /// Takes a capacity partition hash and updates its PartitionAssignment
    /// state to indicate it is part of a data ledger
    fn assign_partition_to_slot(
        &mut self,
        partition_hash: H256,
        ledger: Ledger,
        slot_index: usize,
    ) {
        if let Some(mut assignment) = self.capacity_partitions.remove(&partition_hash) {
            assignment.ledger_num = Some(ledger as u64);
            assignment.slot_index = Some(slot_index);
            self.data_partitions.insert(partition_hash, assignment);
        }
    }

    /// For a given ledger indicated by ledger_num, calculate the number of
    /// partition slots to add to the ledger based on remaining capacity
    /// and data ingress this epoch
    fn calculate_additional_slots(
        &self,
        new_epoch_block: &IrysBlockHeader,
        ledger_num: Ledger,
    ) -> u64 {
        let ledger = &self.ledgers[ledger_num];
        let num_slots = ledger.slot_count() as u64;
        let max_capacity = (num_slots * PARTITION_SIZE) as u128;
        let ledger_size = new_epoch_block.ledgers[ledger_num as usize].ledger_size;

        // Add capacity slots if ledger usage exceeds 50% of partition size from max capacity
        let add_capacity_threshold: u128 = max_capacity - PARTITION_SIZE as u128 / 2;
        let mut slots_to_add: u64 = 0;
        if ledger_size >= add_capacity_threshold {
            // Add 1 slot for buffer plus enough slots to handle size above threshold
            let excess = ledger_size.saturating_sub(max_capacity);
            slots_to_add = 1 + (excess as u64 / PARTITION_SIZE);

            // Check if we need to add an additional slot for excess > half of PARTITION_SIZE
            if excess as u64 % PARTITION_SIZE >= PARTITION_SIZE / 2 {
                slots_to_add += 1;
            }
        }

        // Compute Data uploaded to the ledger last epoch
        // TODO: need a block index to do this
        if new_epoch_block.height >= NUM_BLOCKS_IN_EPOCH {
            // let last_epoch_block =
            //     block_index.get(new_new_epoch_block.height - NUM_BLOCKS_IN_EPOCH);
            // let data_added: u64 = ledger_size - last_epoch_block.ledger_size;
            // slots_to_add += Math::ceil(data_added / PARTITION_SIZE)
        }

        slots_to_add
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

#[derive(Debug)]
/// Simple PRNG compatible with JavaScript implementation
pub struct SimpleRNG {
    /// Current state of the generator
    seed: u32,
}

impl SimpleRNG {
    /// Creates new PRNG with given seed
    pub fn new(seed: u32) -> Self {
        Self { seed }
    }

    /// Generates next pseudorandom number
    pub fn next(&mut self) -> u32 {
        // Xorshift algorithm
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.seed = x;
        x
    }

    /// Generates random number between 0 and max (exclusive)
    pub fn next_range(&mut self, max: u32) -> u32 {
        self.next() % max
    }
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    // use assert_matches::assert_matches;

    #[actix::test]
    async fn genesis_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;

        // Create epoch service with random miner address
        let miner_address = Address::random();
        let mut epoch_service = EpochServiceActor::new(miner_address);

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let _ = epoch_service.handle(NewEpochMessage(genesis_block), &mut Context::new());

        // Verify the correct number of ledgers have been added
        let expected_ledger_count = Ledger::ALL.len();
        assert_eq!(epoch_service.ledgers.len(), expected_ledger_count);

        // Verify each ledger has one slot and the correct number of partitions
        let pub_slots = epoch_service.ledgers.get_slots(Ledger::Publish);
        let sub_slots = epoch_service.ledgers.get_slots(Ledger::Submit);

        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 1);

        assert_eq!(
            pub_slots[0].partitions.len() as u64,
            NUM_PARTITIONS_PER_SLOT
        );
        assert_eq!(
            sub_slots[0].partitions.len() as u64,
            NUM_PARTITIONS_PER_SLOT
        );

        let pub_ledger_num = Ledger::Publish as u64;
        let sub_ledger_num = Ledger::Submit as u64;

        // Verify data partition assignments match _PUBLISH_ ledger slots
        for (slot_idx, slot) in pub_slots.iter().enumerate() {
            for &partition_hash in &slot.partitions {
                let assignment = epoch_service
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
            assert_eq!(slot.partitions.len(), NUM_PARTITIONS_PER_SLOT as usize);
        }

        // Verify data partition assignments match _SUBMIT_ledger slots
        for (slot_idx, slot) in sub_slots.iter().enumerate() {
            for &partition_hash in &slot.partitions {
                let assignment = epoch_service
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
            assert_eq!(slot.partitions.len(), NUM_PARTITIONS_PER_SLOT as usize);
        }

        // Verify the correct number of genesis partitions have been activated
        let data_partition_count = epoch_service.data_partitions.len() as u64;
        let expected_partitions = data_partition_count
            + EpochServiceActor::get_num_capacity_partitions(data_partition_count, CAPACITY_SCALAR);
        assert_eq!(
            epoch_service.all_active_partitions.len(),
            expected_partitions as usize
        );

        // Validate that all the capacity partitions are assigned to the
        // bootstrap miner but not assigned to any ledger
        for pair in &epoch_service.capacity_partitions {
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

        // Debug output for verification
        // println!("Data Partitions: {:#?}", epoch_service.capacity_partitions);
        println!("Ledger State: {:#?}", epoch_service.ledgers);
    }

    #[actix::test]
    async fn add_slots_test() {
        // Initialize genesis block at height 0
        let mut genesis_block = IrysBlockHeader::new();
        genesis_block.height = 0;

        // Create epoch service with random miner address
        let miner_address = Address::random();
        let mut epoch_service = EpochServiceActor::new(miner_address);

        // Process genesis message directly instead of through actor system
        // This allows us to inspect the actor's state after processing
        let mut ctx = Context::new();
        let _ = epoch_service.handle(NewEpochMessage(genesis_block), &mut ctx);

        // Now create a new epoch block & give the Submit ledger enough size to add a slot
        let mut new_epoch_block = IrysBlockHeader::new();
        new_epoch_block.height = NUM_BLOCKS_IN_EPOCH;
        new_epoch_block.ledgers[Ledger::Submit as usize].ledger_size = (PARTITION_SIZE / 2) as u128;

        let _ = epoch_service.handle(NewEpochMessage(new_epoch_block), &mut ctx);

        // Verify each ledger has one slot and the correct number of partitions
        let pub_slots = epoch_service.ledgers.get_slots(Ledger::Publish);
        let sub_slots = epoch_service.ledgers.get_slots(Ledger::Submit);

        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 2);

        // Simulate a subsequent epoch block that adds multiple ledger slots
        let mut new_epoch_block = IrysBlockHeader::new();
        new_epoch_block.height = NUM_BLOCKS_IN_EPOCH * 2;
        new_epoch_block.ledgers[Ledger::Submit as usize].ledger_size =
            (PARTITION_SIZE as f64 * 2.5) as u128;
        new_epoch_block.ledgers[Ledger::Publish as usize].ledger_size =
            (PARTITION_SIZE as f64 * 0.75) as u128;

        let _ = epoch_service.handle(NewEpochMessage(new_epoch_block), &mut ctx);

        // Validate the correct number of ledgers slots were added to each ledger
        let pub_slots = epoch_service.ledgers.get_slots(Ledger::Publish);
        let sub_slots = epoch_service.ledgers.get_slots(Ledger::Submit);
        assert_eq!(pub_slots.len(), 2);
        assert_eq!(sub_slots.len(), 4);

        println!("Ledger State: {:#?}", epoch_service.ledgers);
    }

    #[actix::test]
    async fn expire_slots_test() {}

    #[actix::test]
    async fn capacity_projection_tests() {
        let max_data_parts = 1000;
        for i in (0..max_data_parts).step_by(10) {
            let data_partition_count = i;
            let capacity_count = EpochServiceActor::get_num_capacity_partitions(
                data_partition_count,
                CAPACITY_SCALAR,
            );
            let total = data_partition_count + capacity_count;
            println!(
                "data:{}, capacity:{}, total:{}",
                data_partition_count, capacity_count, total
            );
        }
    }
}
