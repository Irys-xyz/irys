use irys_types::{Compact, TransactionLedger, CONFIG, H256};
use serde::{Deserialize, Serialize};
use std::ops::{Index, IndexMut};
/// Manages the global ledger state within the epoch service, tracking:
/// - All ledger types (Publish, Submit, etc.)
/// - Their associated partitions
/// - Expiration status of term-based ledgers
///
/// This provides a complete view of the protocol's data storage and
/// validation state at any given time.
/// A slot in a data ledger containing one or more partition hashes

#[derive(Debug, Clone)]
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
}

impl Default for PermanentLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl PermanentLedger {
    /// Constructs a permanent ledger, always with `Ledger::Publish` as the id
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            ledger_id: Ledger::Publish as u32,
        }
    }
}

impl TermLedger {
    /// Creates a term ledger with specified index and duration
    pub const fn new(ledger: Ledger, epoch_length: u64) -> Self {
        Self {
            slots: Vec::new(),
            ledger_id: ledger as u32,
            epoch_length,
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
        if epoch_height < self.epoch_length * CONFIG.num_blocks_in_epoch {
            return expired_indices;
        }

        let expiry_height = epoch_height - self.epoch_length * CONFIG.num_blocks_in_epoch;

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
    fn ledger_id(&self) -> u32;

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
    fn ledger_id(&self) -> u32 {
        self.ledger_id
    }
    fn allocate_slots(&mut self, slots: u64) -> u64 {
        let mut num_partitions_added = 0;
        for _ in 0..slots {
            self.slots.push(LedgerSlot {
                partitions: Vec::new(),
                is_expired: false,
                last_height: 0,
            });
            num_partitions_added += CONFIG.num_partitions_per_slot;
        }
        num_partitions_added
    }
    fn get_slot_needs(&self) -> Vec<(usize, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = CONFIG.num_partitions_per_slot as usize - slot.partitions.len();
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
    fn ledger_id(&self) -> u32 {
        self.ledger_id
    }
    fn allocate_slots(&mut self, slots: u64) -> u64 {
        let mut num_partitions_added = 0;
        for _ in 0..slots {
            self.slots.push(LedgerSlot {
                partitions: Vec::new(),
                is_expired: false,
                last_height: 0,
            });
            num_partitions_added += CONFIG.num_partitions_per_slot;
        }
        num_partitions_added
    }

    fn get_slot_needs(&self) -> Vec<(usize, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = CONFIG.num_partitions_per_slot as usize - slot.partitions.len();
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

/// Names for each of the ledgers as well as their `ledger_id` discriminant
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Compact, PartialOrd, Ord)]
#[repr(u32)]
pub enum Ledger {
    /// The permanent publish ledger
    Publish = 0,
    /// An expiring term ledger used for submitting to the publish ledger
    Submit = 1,
    // Add more term ledgers as they exist
}

impl Default for Ledger {
    fn default() -> Self {
        Self::Publish
    }
}

impl Ledger {
    /// An array of all the Ledger numbers in order
    pub const ALL: [Self; 2] = [Self::Publish, Self::Submit];

    /// Make it possible to iterate over all the `LedgerNums` in order
    pub fn iter() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied()
    }
    /// get the associated numeric ID
    pub const fn get_id(&self) -> u32 {
        *self as u32
    }

    // Takes "perm" or some term e.g. "1year", or an integer ID
    pub fn from_url(s: &str) -> eyre::Result<Self> {
        if let Ok(ledger_id) = s.parse::<u32>() {
            return Ledger::try_from(ledger_id).map_err(|e| eyre::eyre!(e));
        }
        match s {
            "perm" => eyre::Result::Ok(Self::Publish),
            "5days" => eyre::Result::Ok(Self::Submit),
            _ => Err(eyre::eyre!("Ledger {} not supported", s)),
        }
    }
}

impl From<Ledger> for u32 {
    fn from(ledger: Ledger) -> Self {
        ledger as Self
    }
}

impl TryFrom<u32> for Ledger {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Publish),
            1 => Ok(Self::Submit),
            _ => Err("Invalid ledger number"),
        }
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

impl Default for Ledgers {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledgers {
    /// Instantiate a Ledgers struct with the correct Ledgers
    pub fn new() -> Self {
        Self {
            perm: PermanentLedger::new(),
            term: vec![TermLedger::new(
                Ledger::Submit,
                CONFIG.submit_ledger_epoch_length,
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

    // Private helper methods for term ledger lookups
    fn get_term_ledger(&self, ledger: Ledger) -> &TermLedger {
        self.term
            .iter()
            .find(|l| l.ledger_id == ledger as u32)
            .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger))
    }

    fn get_term_ledger_mut(&mut self, ledger: Ledger) -> &mut TermLedger {
        self.term
            .iter_mut()
            .find(|l| l.ledger_id == ledger as u32)
            .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger))
    }

    pub fn get_slots(&self, ledger: Ledger) -> &Vec<LedgerSlot> {
        match ledger {
            Ledger::Publish => self.perm.get_slots(),
            ledger => self.get_term_ledger(ledger).get_slots(),
        }
    }

    pub fn get_slot_needs(&self, ledger: Ledger) -> Vec<(usize, usize)> {
        match ledger {
            Ledger::Publish => self.perm.get_slot_needs(),
            ledger => self.get_term_ledger(ledger).get_slot_needs(),
        }
    }

    pub fn push_partition_to_slot(
        &mut self,
        ledger: Ledger,
        slot_index: usize,
        partition_hash: H256,
    ) {
        match ledger {
            Ledger::Publish => {
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
        ledger: Ledger,
        slot_index: usize,
        partition_hash: &H256,
    ) {
        match ledger {
            Ledger::Publish => {
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
impl Index<Ledger> for Ledgers {
    type Output = dyn LedgerCore;

    fn index(&self, ledger: Ledger) -> &Self::Output {
        match ledger {
            Ledger::Publish => &self.perm,
            ledger => self
                .term
                .iter()
                .find(|l| l.ledger_id == ledger as u32)
                .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger)),
        }
    }
}

// Implement IndexMut to retrieve a LedgerCore by its Ledger name
impl IndexMut<Ledger> for Ledgers {
    fn index_mut(&mut self, ledger: Ledger) -> &mut Self::Output {
        match ledger {
            Ledger::Publish => &mut self.perm,
            Ledger::Submit => &mut self.term[0],
        }
    }
}

impl Index<Ledger> for Vec<TransactionLedger> {
    type Output = TransactionLedger;

    fn index(&self, ledger: Ledger) -> &Self::Output {
        self.iter()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No transaction ledger found for given ledger type")
    }
}

impl IndexMut<Ledger> for Vec<TransactionLedger> {
    fn index_mut(&mut self, ledger: Ledger) -> &mut Self::Output {
        self.iter_mut()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No transaction ledger found for given ledger type")
    }
}
