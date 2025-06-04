use irys_types::{ConsensusConfig, DataLedger, H256};
use serde::Serialize;
use std::ops::{Index, IndexMut};
/// Manages the global ledger state within the epoch service, tracking:
/// - All ledger types (Publish, Submit, etc.)
/// - Their associated partitions
/// - Expiration status of term-based ledgers
///
/// This provides a complete view of the protocol's data storage and
/// validation state at any given time.
/// A slot in a data ledger containing one or more partition hashes

#[derive(Debug, Clone, Serialize)]
pub struct LedgerSlot {
    /// Assigned partition hashes
    pub partitions: Vec<H256>,
    /// Flag marking weather this ledger slot is expired or not
    pub is_expired: bool,
    /// Block height of most recently added transaction data (chunks)
    pub last_height: u64,
}

#[derive(Debug, Clone)]
/// Permanent ledger that persists across epochs
pub struct PermanentLedger {
    /// Sequential ledger slots containing partition assignments
    pub slots: Vec<LedgerSlot>,
    /// Unique identifier for this ledger, see `Ledger` enum
    pub ledger_id: u32,
    pub num_partitions_per_slot: u64,
}

#[derive(Debug, Clone)]
/// Temporary ledger that exists for a fixed number of epochs
pub struct TermLedger {
    /// Sequential ledger slots containing partition assignments  
    pub slots: Vec<LedgerSlot>,
    /// Unique identifier for this ledger, see `Ledger` enum
    pub ledger_id: u32,
    /// Number of epochs slots in this ledger exist for
    pub epoch_length: u64,
    pub num_blocks_in_epoch: u64,
    pub num_partitions_per_slot: u64,
}

impl PermanentLedger {
    /// Constructs a permanent ledger, always with `Ledger::Publish` as the id
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            slots: Vec::new(),
            ledger_id: DataLedger::Publish as u32,
            num_partitions_per_slot: config.num_partitions_per_slot,
        }
    }
}

impl TermLedger {
    /// Creates a term ledger with specified index and duration
    pub fn new(ledger: DataLedger, config: &ConsensusConfig) -> Self {
        Self {
            slots: Vec::new(),
            ledger_id: ledger as u32,
            epoch_length: config.epoch.submit_ledger_epoch_length,
            num_blocks_in_epoch: config.epoch.num_blocks_in_epoch,
            num_partitions_per_slot: config.num_partitions_per_slot,
        }
    }

    /// Returns a slice of the ledgers slots
    pub const fn get_slots(&self) -> &Vec<LedgerSlot> {
        &self.slots
    }

    /// Returns indices of newly expired slots
    pub fn expire_old_slots(&mut self, epoch_height: u64) -> Vec<usize> {
        let mut expired_indices = Vec::new();

        // Make sure enough blocks have transpired before calculating expiry height
        if epoch_height < self.epoch_length * self.num_blocks_in_epoch {
            return expired_indices;
        }

        let expiry_height = epoch_height - self.epoch_length * self.num_blocks_in_epoch;

        // Collect indices of slots to expire
        for (idx, slot) in self.slots.iter().enumerate() {
            if idx == self.slots.len() - 1 {
                // Never expire the last slot in a ledger
                continue;
            }
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
    fn ledger_id(&self) -> u32;

    /// Adds slots to the ledger, reserving space for partitions
    fn allocate_slots(&mut self, slots: u64, height: u64) -> u64;

    /// Get the slot needs for the ledger, returning a vector of (slot index, number of partitions needed)
    fn get_slot_needs(&self) -> Vec<(usize, usize)>;

    fn get_slots(&self) -> &Vec<LedgerSlot>;
}

impl LedgerCore for PermanentLedger {
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
    fn ledger_id(&self) -> u32 {
        self.ledger_id
    }
    fn allocate_slots(&mut self, slots: u64, height: u64) -> u64 {
        let mut num_partitions_added = 0;
        for _ in 0..slots {
            self.slots.push(LedgerSlot {
                partitions: Vec::new(),
                is_expired: false,
                last_height: height,
            });
            num_partitions_added += self.num_partitions_per_slot;
        }
        num_partitions_added
    }
    fn get_slot_needs(&self) -> Vec<(usize, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = self.num_partitions_per_slot as usize - slot.partitions.len();
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
    /// Get total slot count for capacity planning and chunk allocation decisions
    ///
    /// Returns the total number of slots (both expired and active) in the term ledger.
    /// This count is critical for:
    /// 1. Tracking maximum theoretical storage capacity over time
    /// 2. Determining when to allocate additional slots based on data ingress rate
    /// 3. Comparing against max_chunk_offset to assess if we're approaching capacity
    ///    (within half a partition of maximum) and need to add additional slots
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
    fn ledger_id(&self) -> u32 {
        self.ledger_id
    }
    fn allocate_slots(&mut self, slots: u64, height: u64) -> u64 {
        let mut num_partitions_added = 0;
        for _ in 0..slots {
            self.slots.push(LedgerSlot {
                partitions: Vec::new(),
                is_expired: false,
                last_height: height,
            });
            num_partitions_added += self.num_partitions_per_slot;
        }
        num_partitions_added
    }

    fn get_slot_needs(&self) -> Vec<(usize, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = self.num_partitions_per_slot as usize - slot.partitions.len();
                if needed > 0 && !slot.is_expired {
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

/// A container for managing permanent and term ledgers with type-safe access
/// through the [Ledger] enum.
///
/// The permanent and term ledgers are intentionally given different types to
/// prevent runtime errors:
/// - The permanent ledger (`perm`) holds critical data that must never
///   be expired or lost
/// - Term ledgers (`term`) hold temporary data and support expiration
///
/// This type separation ensures operations like partition expiration can only
/// be performed on term ledgers, making any attempt to expire a permanent
/// ledger partition fail at compile time.
#[derive(Debug, Clone)]
pub struct Ledgers {
    perm: PermanentLedger,
    term: Vec<TermLedger>,
}

impl Ledgers {
    /// Instantiate a Ledgers struct with the correct Ledgers
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            perm: PermanentLedger::new(config),
            term: vec![TermLedger::new(DataLedger::Submit, config)],
        }
    }

    /// The number of ledgers being managed
    #[allow(
        clippy::len_without_is_empty,
        reason = "Doesn't make sense to add here right now"
    )]
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

    // Private helper methods for term ledger lookups
    fn get_term_ledger(&self, ledger: DataLedger) -> &TermLedger {
        self.term
            .iter()
            .find(|l| l.ledger_id == ledger as u32)
            .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger))
    }

    fn get_term_ledger_mut(&mut self, ledger: DataLedger) -> &mut TermLedger {
        self.term
            .iter_mut()
            .find(|l| l.ledger_id == ledger as u32)
            .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger))
    }

    pub fn get_slots(&self, ledger: DataLedger) -> &Vec<LedgerSlot> {
        match ledger {
            DataLedger::Publish => self.perm.get_slots(),
            ledger => self.get_term_ledger(ledger).get_slots(),
        }
    }

    pub fn get_slot_needs(&self, ledger: DataLedger) -> Vec<(usize, usize)> {
        match ledger {
            DataLedger::Publish => self.perm.get_slot_needs(),
            ledger => self.get_term_ledger(ledger).get_slot_needs(),
        }
    }

    pub fn push_partition_to_slot(
        &mut self,
        ledger: DataLedger,
        slot_index: usize,
        partition_hash: H256,
    ) {
        match ledger {
            DataLedger::Publish => {
                self.perm.slots[slot_index].partitions.push(partition_hash);
            }
            ledger => {
                self.get_term_ledger_mut(ledger).slots[slot_index]
                    .partitions
                    .push(partition_hash);
            }
        }
    }

    pub fn remove_partition_from_slot(
        &mut self,
        ledger: DataLedger,
        slot_index: usize,
        partition_hash: &H256,
    ) {
        match ledger {
            DataLedger::Publish => {
                self.perm.slots[slot_index]
                    .partitions
                    .retain(|p| p != partition_hash);
            }
            ledger => {
                self.get_term_ledger_mut(ledger).slots[slot_index]
                    .partitions
                    .retain(|p| p != partition_hash);
            }
        }
    }
}

// Implement Index to retrieve a LedgerCore by its Ledger name
impl Index<DataLedger> for Ledgers {
    type Output = dyn LedgerCore;

    fn index(&self, ledger: DataLedger) -> &Self::Output {
        match ledger {
            DataLedger::Publish => &self.perm,
            ledger => self
                .term
                .iter()
                .find(|l| l.ledger_id == ledger as u32)
                .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger)),
        }
    }
}

// Implement IndexMut to retrieve a LedgerCore by its Ledger name
impl IndexMut<DataLedger> for Ledgers {
    fn index_mut(&mut self, ledger: DataLedger) -> &mut Self::Output {
        match ledger {
            DataLedger::Publish => &mut self.perm,
            DataLedger::Submit => &mut self.term[0],
        }
    }
}
