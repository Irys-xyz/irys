use irys_types::{H256, NUM_BLOCKS_IN_EPOCH, NUM_PARTITIONS_PER_SLOT, SUBMIT_LEDGER_EPOCH_LENGTH};
use std::ops::{Index, IndexMut};

/// A slot in a data ledger containing one or more partition hashes
#[derive(Debug)]
pub struct LedgerSlot {
    /// Assigned partition hashes
    pub partitions: Vec<H256>,
    /// Flag marking weather this partition is expired or not
    pub is_expired: bool,
    /// Block height of most recently added transaction data (chunks)
    pub last_height: u64,
}

#[derive(Debug)]
/// Permanent ledger that persists across epochs
pub struct PermanentLedger {
    /// Sequential ledger slots containing partition assignments
    pub slots: Vec<LedgerSlot>,
    /// Unique index of this ledger within its block
    pub ledger_num: usize,
}

#[derive(Debug)]
/// Temporary ledger that exists for a fixed number of epochs
pub struct TermLedger {
    /// Sequential ledger slots containing partition assignments  
    pub slots: Vec<LedgerSlot>,
    /// Unique index of this ledger within its block
    pub ledger_num: usize,
    /// Number of epochs slots in this ledger exist for
    pub epoch_length: u64,
}

impl PermanentLedger {
    /// Constructs a permanent ledger, always with ledger_num: 0
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            ledger_num: 0,
        }
    }
}

impl TermLedger {
    /// Creates a term ledger with specified index and duration
    pub fn new(ledger_num: usize, epoch_length: u64) -> Self {
        Self {
            slots: Vec::new(),
            ledger_num,
            epoch_length,
        }
    }

    /// Returns a slice of the ledgers slots
    pub fn get_slots(&self) -> &Vec<LedgerSlot> {
        &self.slots
    }

    /// Returns indices of newly expired slots
    pub fn expire_old_slots(&mut self, epoch_height: u64) -> Vec<usize> {
        let mut expired_indices = Vec::new();

        // Make sure enough blocks have transpired before calculating expiry height
        if epoch_height < self.epoch_length * NUM_BLOCKS_IN_EPOCH {
            return expired_indices;
        }

        let expiry_height = epoch_height - self.epoch_length * NUM_BLOCKS_IN_EPOCH;

        // Collect indices of slots to expire
        for (idx, slot) in self.slots.iter().enumerate() {
            if slot.last_height <= expiry_height && !slot.is_expired {
                expired_indices.push(idx);
            }
        }

        // Mark collected slots as expired
        for &idx in &expired_indices {
            self.slots[idx].is_expired = true;
        }

        expired_indices
    }
}

/// A trait for common operations for all data ledgers
pub trait LedgerCore {
    /// Total number of slots in the ledger
    fn slot_count(&self) -> usize;

    /// Unique index of this ledger within its block
    fn ledger_num(&self) -> usize;

    /// Adds slots to the ledger, reserving space for partitions
    fn allocate_slots(&mut self, slots: u64) -> u64;

    /// Get the slot needs for the ledger, returning a vector of (slot index, number of partitions needed)
    fn get_slot_needs(&self) -> Vec<(usize, usize)>;

    fn get_slots(&self) -> &Vec<LedgerSlot>;
}

impl LedgerCore for PermanentLedger {
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
    fn ledger_num(&self) -> usize {
        self.ledger_num
    }
    fn allocate_slots(&mut self, slots: u64) -> u64 {
        let mut num_partitions_added = 0;
        for _ in 0..slots {
            self.slots.push(LedgerSlot {
                partitions: Vec::new(),
                is_expired: false,
                last_height: 0,
            });
            num_partitions_added += NUM_PARTITIONS_PER_SLOT;
        }
        num_partitions_added
    }
    fn get_slot_needs(&self) -> Vec<(usize, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = NUM_PARTITIONS_PER_SLOT as usize - slot.partitions.len();
                if needed > 0 {
                    Some((idx, needed))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns a slice of the ledgers slots
    fn get_slots(&self) -> &Vec<LedgerSlot> {
        &self.slots
    }
}

impl LedgerCore for TermLedger {
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
    fn ledger_num(&self) -> usize {
        self.ledger_num
    }
    fn allocate_slots(&mut self, slots: u64) -> u64 {
        let mut num_partitions_added = 0;
        for _ in 0..slots {
            self.slots.push(LedgerSlot {
                partitions: Vec::new(),
                is_expired: false,
                last_height: 0,
            });
            num_partitions_added += NUM_PARTITIONS_PER_SLOT;
        }
        num_partitions_added
    }

    fn get_slot_needs(&self) -> Vec<(usize, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = NUM_PARTITIONS_PER_SLOT as usize - slot.partitions.len();
                if needed > 0 {
                    Some((idx, needed))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns a slice of the ledgers slots
    fn get_slots(&self) -> &Vec<LedgerSlot> {
        &self.slots
    }
}

/// Names for each of the ledgers as well as their ledger_num discriminant
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ledger {
    /// The permanent publish ledger
    Publish = 0,
    /// An expiring term ledger used for submitting to the publish ledger
    Submit = 1,
    // Add more term ledgers as they exist
}

impl Ledger {
    /// An array of all the Ledger numbers in order
    pub const ALL: [Ledger; 2] = [Ledger::Publish, Ledger::Submit];

    /// Make it possible to iterate over all the LedgerNums in order
    pub fn iter() -> impl Iterator<Item = Ledger> {
        Self::ALL.iter().copied()
    }
}

/// Provides a way to enforce accessing ledgers by their [Ledger] enum.
#[derive(Debug)]
pub struct Ledgers {
    perm: PermanentLedger,
    term: Vec<TermLedger>,
}

impl Ledgers {
    /// Instantiate a Ledgers struct with the correct Ledgers
    pub fn new() -> Self {
        Self {
            perm: PermanentLedger::new(),
            term: vec![TermLedger::new(
                Ledger::Submit as usize,
                SUBMIT_LEDGER_EPOCH_LENGTH,
            )],
        }
    }

    /// The number of ledgers being managed
    pub fn len(&self) -> usize {
        1 + self.term.len()
    }

    /// Get all of the partition hashes that have expired out of term ledgers
    pub fn get_expired_partition_hashes(&mut self, epoch_height: u64) -> Vec<H256> {
        let mut expired_hashes: Vec<H256> = Vec::new();

        // Collect expired partition hashes from term ledgers
        for term_ledger in &mut self.term {
            for expired_index in term_ledger.expire_old_slots(epoch_height) {
                // Add each partition hash from expired slots
                expired_hashes.extend(term_ledger.slots[expired_index].partitions.iter().copied());
            }
        }

        expired_hashes
    }

    pub fn get_slots(&self, ledger: Ledger) -> &Vec<LedgerSlot> {
        match ledger {
            Ledger::Publish => self.perm.get_slots(),
            Ledger::Submit => self.term[0].get_slots(),
        }
    }

    /// Get the slot needs for a given ledger.
    pub fn get_slot_needs(&self, ledger: Ledger) -> Vec<(usize, usize)> {
        match ledger {
            Ledger::Submit => {
                // Call `get_slot_needs` on the corresponding `term` ledger
                self.term[0].get_slot_needs()
            }
            Ledger::Publish => {
                // Call `get_slot_needs` on `perm` ledger directly
                self.perm.get_slot_needs()
            }
        }
    }

    /// Pushes a partition hash to the appropriate slot based on the ledger type.
    pub fn push_partition_to_slot(
        &mut self,
        ledger: Ledger,
        slot_index: usize,
        partition_hash: H256,
    ) {
        match ledger {
            Ledger::Submit => {
                self.term[0].slots[slot_index]
                    .partitions
                    .push(partition_hash);
            }
            Ledger::Publish => {
                self.perm.slots[slot_index].partitions.push(partition_hash);
            }
        }
    }
}

// Implement Index to retrieve a LedgerCore by its Ledger name
impl Index<Ledger> for Ledgers {
    type Output = dyn LedgerCore;

    fn index(&self, ledger_num: Ledger) -> &Self::Output {
        match ledger_num {
            Ledger::Publish => &self.perm,
            Ledger::Submit => &self.term[0],
        }
    }
}

// Implement IndexMut to retrieve a LedgerCore by its Ledger name
impl IndexMut<Ledger> for Ledgers {
    fn index_mut(&mut self, ledger_num: Ledger) -> &mut Self::Output {
        match ledger_num {
            Ledger::Publish => &mut self.perm,
            Ledger::Submit => &mut self.term[0],
        }
    }
}
