use irys_types::{Compact, Config, H256List, TransactionLedger, H256};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ops::{Index, IndexMut},
};
use tracing::debug;
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
    /// slot number
    pub slot_number: u64,
    /// Assigned partition hashes
    pub partitions: Vec<H256>,
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
    /// Map between slots numbers and its ledger containing partition assignments  
    pub slots: BTreeMap<u64, LedgerSlot>,
    /// Nest slot index
    pub next_slot_idx: u64,
    /// Unique identifier for this ledger, see `Ledger` enum
    pub ledger_id: u32,
    /// Number of epochs slots in this ledger exist for
    pub epoch_length: u64,
    pub num_blocks_in_epoch: u64,
    pub num_partitions_per_slot: u64,
}

impl PermanentLedger {
    /// Constructs a permanent ledger, always with `Ledger::Publish` as the id
    pub fn new(config: &Config) -> Self {
        Self {
            slots: Vec::new(),
            ledger_id: Ledger::Publish as u32,
            num_partitions_per_slot: config.num_partitions_per_slot,
        }
    }
}

impl TermLedger {
    /// Creates a term ledger with specified index and duration
    pub fn new(ledger: Ledger, config: &Config) -> Self {
        Self {
            slots: BTreeMap::new(),
            ledger_id: ledger as u32,
            next_slot_idx: 0,
            epoch_length: config.submit_ledger_epoch_length,
            num_blocks_in_epoch: config.num_blocks_in_epoch,
            num_partitions_per_slot: config.num_partitions_per_slot,
        }
    }

    /// Returns a slice of the ledgers slots
    pub fn get_slots(&self) -> Vec<LedgerSlot> {
        self.slots.range(..).map(|(_, slot)| slot.clone()).collect()
    }

    /// Returns indices of newly expired slots
    pub fn expire_old_slots(&mut self, epoch_height: u64) -> Vec<(u64, H256List)> {
        let mut expired_indices = Vec::new();

        // Make sure enough blocks have transpired before calculating expiry height
        if epoch_height < self.epoch_length * self.num_blocks_in_epoch {
            debug!("Not enough blocks have transpired to expire slots");
            return expired_indices;
        }

        let expiry_height = epoch_height - self.epoch_length * self.num_blocks_in_epoch;

        // Collect indices of slots to expire
        for (idx, slot) in self.slots.iter() {
            if slot.last_height <= expiry_height {
                debug!(
                    "Slot {} expired at height {}",
                    slot.slot_number, expiry_height
                );
                expired_indices.push((*idx, H256List(slot.partitions.clone())));
            }
        }

        // Delete expired slots
        for (idx, _) in &expired_indices {
            self.slots.remove(idx);
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
    fn allocate_slots(&mut self, slots_num: usize) -> u64;

    /// Get the slot needs for the ledger, returning a vector of (slot index, number of partitions needed)
    fn get_slot_needs(&self) -> Vec<(u64, usize)>;

    fn get_slots(&self) -> Vec<LedgerSlot>;
}

impl LedgerCore for PermanentLedger {
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
    fn ledger_id(&self) -> u32 {
        self.ledger_id
    }
    fn allocate_slots(&mut self, slots_num: usize) -> u64 {
        let mut num_partitions_added = 0;
        let last_slot = self.slots.len() as u64;
        for slot in 0..slots_num {
            self.slots.push(LedgerSlot {
                slot_number: last_slot + slot as u64,
                partitions: Vec::new(),
                last_height: 0,
            });
            num_partitions_added += self.num_partitions_per_slot;
        }
        num_partitions_added
    }
    fn get_slot_needs(&self) -> Vec<(u64, usize)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| {
                let needed = self.num_partitions_per_slot as usize - slot.partitions.len();
                if needed > 0 {
                    Some((idx as u64, needed))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns a slice of the ledgers slots
    fn get_slots(&self) -> Vec<LedgerSlot> {
        self.slots.clone()
    }
}

impl LedgerCore for TermLedger {
    fn slot_count(&self) -> usize {
        self.slots.len()
    }
    fn ledger_id(&self) -> u32 {
        self.ledger_id
    }
    fn allocate_slots(&mut self, slots_num: usize) -> u64 {
        let mut num_partitions_added: u64 = 0;
        for _slot in 0..slots_num {
            self.slots.insert(
                self.next_slot_idx,
                LedgerSlot {
                    slot_number: self.next_slot_idx,
                    partitions: Vec::new(),
                    last_height: 0,
                },
            );
            self.next_slot_idx += 1;
            num_partitions_added += self.num_partitions_per_slot;
        }
        num_partitions_added
    }

    fn get_slot_needs(&self) -> Vec<(u64, usize)> {
        self.slots
            .iter()
            .filter_map(|(idx, slot)| {
                let needed = self.num_partitions_per_slot as usize - slot.partitions.len();
                if needed > 0 {
                    Some((*idx, needed))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns ledgers slots
    fn get_slots(&self) -> Vec<LedgerSlot> {
        self.slots.range(..).map(|(_, slot)| slot.clone()).collect()
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

impl Ledgers {
    /// Instantiate a Ledgers struct with the correct Ledgers
    pub fn new(config: &Config) -> Self {
        Self {
            perm: PermanentLedger::new(config),
            term: vec![TermLedger::new(Ledger::Submit, config)],
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
            for (_expired_index, expired_partitions) in term_ledger.expire_old_slots(epoch_height) {
                // Add each partition hash from expired slots
                expired_hashes.extend(expired_partitions.0.iter());
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

    pub fn get_slots(&self, ledger: Ledger) -> Vec<LedgerSlot> {
        match ledger {
            Ledger::Publish => self.perm.get_slots(),
            ledger => self.get_term_ledger(ledger).get_slots(),
        }
    }

    pub fn get_slot_needs(&self, ledger: Ledger) -> Vec<(u64, usize)> {
        match ledger {
            Ledger::Publish => self.perm.get_slot_needs(),
            ledger => self.get_term_ledger(ledger).get_slot_needs(),
        }
    }

    pub fn push_partition_to_slot(
        &mut self,
        ledger: Ledger,
        slot_index: u64,
        partition_hash: H256,
    ) {
        match ledger {
            Ledger::Publish => {
                self.perm.slots
                    [usize::try_from(slot_index).expect("slot index representation error")]
                .partitions
                .push(partition_hash);
            }
            ledger => {
                self.get_term_ledger_mut(ledger)
                    .slots
                    .get_mut(&slot_index)
                    .map(|slot| slot.partitions.push(partition_hash));
            }
        }
    }

    pub fn remove_partition_from_slot(
        &mut self,
        ledger: Ledger,
        slot_index: u64,
        partition_hash: &H256,
    ) {
        match ledger {
            Ledger::Publish => {
                self.perm.slots
                    [usize::try_from(slot_index).expect("slot index representation error")]
                .partitions
                .retain(|p| p != partition_hash);
            }
            ledger => {
                self.get_term_ledger_mut(ledger)
                    .slots
                    .get_mut(&slot_index)
                    .map(|slot| slot.partitions.retain(|p| p != partition_hash));
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
