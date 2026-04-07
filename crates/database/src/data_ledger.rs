use irys_types::{ConsensusConfig, DataLedger, H256, partition::PartitionHash};
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

#[derive(Debug, Clone, Serialize, Hash)]
pub struct LedgerSlot {
    /// Assigned partition hashes
    pub partitions: Vec<H256>,
    /// Flag marking weather this ledger slot is expired or not
    pub is_expired: bool,
    /// Block height of most recently added transaction data (chunks)
    pub last_height: u64,
}

#[derive(Debug, Clone, Copy, Hash)]
pub struct ExpiringPartitionInfo {
    pub partition_hash: PartitionHash,
    pub ledger_id: DataLedger,
    pub slot_index: usize,
}

#[derive(Debug, Clone, Hash)]
/// Permanent ledger that persists across epochs
pub struct PermanentLedger {
    /// Sequential ledger slots containing partition assignments
    pub slots: Vec<LedgerSlot>,
    /// Unique identifier for this ledger, see `Ledger` enum
    pub ledger_id: u32,
    pub num_partitions_per_slot: u64,
}

#[derive(Debug, Clone, Hash)]
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
    /// Creates a term ledger with the specified ledger type and epoch length.
    pub fn new(ledger: DataLedger, config: &ConsensusConfig, epoch_length: u64) -> Self {
        Self {
            slots: Vec::new(),
            ledger_id: ledger as u32,
            epoch_length,
            num_blocks_in_epoch: config.epoch.num_blocks_in_epoch,
            num_partitions_per_slot: config.num_partitions_per_slot,
        }
    }

    /// Returns a slice of the ledgers slots
    pub const fn get_slots(&self) -> &Vec<LedgerSlot> {
        &self.slots
    }

    #[tracing::instrument(level = "trace", skip_all, fields(epoch_height = %epoch_height))]
    pub fn get_expired_slot_indexes(&self, epoch_height: u64) -> Vec<usize> {
        let mut expired_slot_indexes = Vec::new();

        let min_blocks = self
            .epoch_length
            .checked_mul(self.num_blocks_in_epoch)
            .expect("epoch_length * num_blocks_in_epoch overflows u64");

        tracing::debug!(
            "expire_old_slots: epoch_height={}, epoch_length={}, num_blocks_in_epoch={}, min_height_needed={}",
            epoch_height,
            self.epoch_length,
            self.num_blocks_in_epoch,
            min_blocks
        );

        // Make sure enough blocks have transpired before calculating expiry height
        if epoch_height < min_blocks {
            tracing::warn!(
                "Not enough blocks yet: {} < {}, returning empty",
                epoch_height,
                min_blocks
            );
            return expired_slot_indexes;
        }

        let expiry_height = epoch_height - min_blocks;
        tracing::info!("Calculated expiry_height={}", expiry_height);

        // Collect indices of slots to expire
        for (slot_index, slot) in self.slots.iter().enumerate() {
            tracing::debug!(
                "Checking slot {}: last_height={}, is_expired={}, partitions={:?}",
                slot_index,
                slot.last_height,
                slot.is_expired,
                slot.partitions
            );

            if slot_index == self.slots.len() - 1 {
                // Never expire the last slot in a ledger
                tracing::warn!("Skipping slot {} (last slot)", slot_index);
                continue;
            }
            if slot.last_height <= expiry_height && !slot.is_expired {
                tracing::info!("Slot {} is expired! Adding to expired_indices", slot_index);
                expired_slot_indexes.push(slot_index);
            }
        }

        expired_slot_indexes
    }

    /// Returns indices of newly expired slots
    pub fn expire_old_slots(&mut self, epoch_height: u64) -> Vec<usize> {
        let expired_slot_indexes = self.get_expired_slot_indexes(epoch_height);

        // Mark collected slots as expired
        for &idx in &expired_slot_indexes {
            self.slots[idx].is_expired = true;
        }

        expired_slot_indexes
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
/// provide distinct behavior:
/// - The permanent ledger (`perm`) holds published data; when
///   `publish_ledger_epoch_length` is configured, its slots can expire
/// - Term ledgers (`term`) hold temporary data with mandatory expiration
///
/// Expiry logic for both ledger types is handled by `Ledgers` methods
/// (`expire_partitions`, `get_expiring_partitions`), keeping `PermanentLedger`
/// itself clean of expiry concerns.
#[derive(Debug, Clone, Hash)]
pub struct Ledgers {
    perm: PermanentLedger,
    term: Vec<TermLedger>,
    /// When Some(n), publish ledger slots expire after n epochs
    publish_ledger_epoch_length: Option<u64>,
    /// Blocks per epoch (needed for expiry height calculation)
    num_blocks_in_epoch: u64,
}

impl Ledgers {
    /// Instantiate a Ledgers struct with the correct Ledgers.
    /// When `cascade_active` is true, includes OneYear and ThirtyDay term ledgers.
    pub fn new(config: &ConsensusConfig, cascade_active: bool) -> Self {
        let mut term = vec![TermLedger::new(
            DataLedger::Submit,
            config,
            config.epoch.submit_ledger_epoch_length,
        )];
        if let Some(cascade) = cascade_active
            .then_some(config.hardforks.cascade.as_ref())
            .flatten()
        {
            term.push(TermLedger::new(
                DataLedger::OneYear,
                config,
                cascade.one_year_epoch_length,
            ));
            term.push(TermLedger::new(
                DataLedger::ThirtyDay,
                config,
                cascade.thirty_day_epoch_length,
            ));
        }
        Self {
            perm: PermanentLedger::new(config),
            term,
            publish_ledger_epoch_length: config.epoch.publish_ledger_epoch_length,
            num_blocks_in_epoch: config.epoch.num_blocks_in_epoch,
        }
    }

    /// Adds OneYear and ThirtyDay term ledgers when the Cascade hardfork activates mid-chain.
    /// No-op if these ledgers are already present.
    pub fn activate_cascade(&mut self, config: &ConsensusConfig) {
        if self
            .term
            .iter()
            .any(|t| t.ledger_id == DataLedger::OneYear as u32)
        {
            return; // already activated
        }
        if let Some(cascade) = config.hardforks.cascade.as_ref() {
            self.term.push(TermLedger::new(
                DataLedger::OneYear,
                config,
                cascade.one_year_epoch_length,
            ));
            self.term.push(TermLedger::new(
                DataLedger::ThirtyDay,
                config,
                cascade.thirty_day_epoch_length,
            ));
        }
    }

    /// The number of ledgers being managed
    #[expect(
        clippy::len_without_is_empty,
        reason = "Doesn't make sense to add here right now"
    )]
    pub fn len(&self) -> usize {
        1 + self.term.len()
    }

    /// Returns the list of active DataLedger variants managed by this instance.
    /// Use this instead of `DataLedger::ALL` or `DataLedger::iter()` to iterate
    /// only over ledgers that are actually active under the current hardfork state.
    pub fn active_ledgers(&self) -> Vec<DataLedger> {
        let mut ledgers = vec![DataLedger::Publish];
        for term in &self.term {
            // ledger_id is set from known DataLedger variants in TermLedger::new,
            // so this conversion should always succeed.
            if let Ok(ledger) = DataLedger::try_from(term.ledger_id) {
                ledgers.push(ledger);
            }
        }
        ledgers
    }

    /// Get all partition hashes that have expired out of both perm and term ledgers.
    /// Perm slots only expire when `publish_ledger_epoch_length` is configured.
    pub fn expire_partitions(&mut self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
        let mut expired_partitions: Vec<ExpiringPartitionInfo> = Vec::new();

        // Expire perm ledger slots using shared helper
        for (slot_index, partition_hashes, ledger_id) in self.get_perm_expiring_slots(epoch_height)
        {
            self.perm.slots[slot_index].is_expired = true;
            for partition_hash in partition_hashes {
                expired_partitions.push(ExpiringPartitionInfo {
                    partition_hash,
                    ledger_id,
                    slot_index,
                });
            }
        }

        // Collect expired partition hashes from term ledgers
        for term_ledger in &mut self.term {
            let ledger_id = DataLedger::try_from(term_ledger.ledger_id)
                .expect("term ledger_id is always constructed from a valid DataLedger variant");
            for expired_index in term_ledger.expire_old_slots(epoch_height) {
                for partition_hash in term_ledger.slots[expired_index].partitions.iter() {
                    expired_partitions.push(ExpiringPartitionInfo {
                        partition_hash: *partition_hash,
                        ledger_id,
                        slot_index: expired_index,
                    });
                }
            }
        }

        expired_partitions
    }

    /// Get all partition hashes that would expire at this epoch height (read-only).
    /// Unlike `expire_partitions`, this does NOT mark slots as expired.
    pub fn get_expiring_partitions(&self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
        let mut expired_partitions: Vec<ExpiringPartitionInfo> = Vec::new();

        // Check perm ledger slots using shared helper
        for (slot_index, partition_hashes, ledger_id) in self.get_perm_expiring_slots(epoch_height)
        {
            for partition_hash in partition_hashes {
                expired_partitions.push(ExpiringPartitionInfo {
                    partition_hash,
                    ledger_id,
                    slot_index,
                });
            }
        }

        // Collect from term ledgers (existing logic)
        for term_ledger in &self.term {
            let ledger_id = DataLedger::try_from(term_ledger.ledger_id)
                .expect("term ledger_id is always constructed from a valid DataLedger variant");
            for expiring_slot_index in term_ledger.get_expired_slot_indexes(epoch_height) {
                for partition_hash in term_ledger.slots[expiring_slot_index].partitions.iter() {
                    expired_partitions.push(ExpiringPartitionInfo {
                        partition_hash: *partition_hash,
                        ledger_id,
                        slot_index: expiring_slot_index,
                    });
                }
            }
        }

        expired_partitions
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

    /// Returns (slot_index, partition_hashes, perm_ledger_id) for each perm slot
    /// that would expire at `epoch_height`. Read-only — does not mark slots.
    fn get_perm_expiring_slots(
        &self,
        epoch_height: u64,
    ) -> Vec<(usize, Vec<PartitionHash>, DataLedger)> {
        let Some(epoch_length) = self.publish_ledger_epoch_length else {
            return Vec::new();
        };

        let min_blocks = epoch_length
            .checked_mul(self.num_blocks_in_epoch)
            .expect("publish_ledger_epoch_length * num_blocks_in_epoch overflows u64");

        if epoch_height < min_blocks {
            return Vec::new();
        }

        let expiry_height = epoch_height - min_blocks;
        let perm_ledger_id = DataLedger::try_from(self.perm.ledger_id)
            .expect("perm.ledger_id is always DataLedger::Publish");
        let num_slots = self.perm.slots.len();
        let last_slot_index = num_slots.saturating_sub(1);

        let mut result = Vec::new();
        for (slot_index, slot) in self.perm.slots.iter().enumerate() {
            // Never expire the last slot
            if num_slots > 0 && slot_index == last_slot_index {
                continue;
            }
            if slot.last_height <= expiry_height && !slot.is_expired {
                result.push((slot_index, slot.partitions.clone(), perm_ledger_id));
            }
        }
        result
    }

    pub fn get_slots(&self, ledger: DataLedger) -> &Vec<LedgerSlot> {
        match ledger {
            DataLedger::Publish => self.perm.get_slots(),
            ledger => self.get_term_ledger(ledger).get_slots(),
        }
    }

    /// Get the slot needs for the ledger, returning a vector of (slot index, number of partitions needed)
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
            DataLedger::Publish => &mut self.perm as &mut dyn LedgerCore,
            ledger => {
                let ledger_id = ledger as u32;
                self.term
                    .iter_mut()
                    .find(|l| l.ledger_id == ledger_id)
                    .map(|l| l as &mut dyn LedgerCore)
                    .unwrap_or_else(|| panic!("Term ledger {:?} not found", ledger))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{
        DataLedger, UnixTimestamp, config::consensus::ConsensusConfig, hardfork_config::Cascade,
    };

    fn config_with_cascade() -> ConsensusConfig {
        let mut config = ConsensusConfig::testing();
        config.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
        config
    }

    fn make_test_config(publish_epoch_length: Option<u64>) -> ConsensusConfig {
        let mut config = ConsensusConfig::testing();
        config.epoch.publish_ledger_epoch_length = publish_epoch_length;
        config.epoch.num_blocks_in_epoch = 10;
        config
    }

    #[test]
    fn test_ledgers_new_without_cascade() {
        let config = ConsensusConfig::testing();
        let ledgers = Ledgers::new(&config, false);
        assert_eq!(ledgers.len(), 2);
        assert_eq!(
            ledgers.active_ledgers(),
            vec![DataLedger::Publish, DataLedger::Submit]
        );
    }

    #[test]
    fn test_ledgers_new_with_cascade_active() {
        let config = config_with_cascade();
        let ledgers = Ledgers::new(&config, true);
        assert_eq!(ledgers.len(), 4);
        let active = ledgers.active_ledgers();
        assert!(active.contains(&DataLedger::Publish));
        assert!(active.contains(&DataLedger::Submit));
        assert!(active.contains(&DataLedger::OneYear));
        assert!(active.contains(&DataLedger::ThirtyDay));
    }

    #[test]
    fn test_ledgers_new_cascade_active_but_no_config() {
        // cascade_active=true but cascade config is None: only 2 ledgers
        let config = ConsensusConfig::testing(); // cascade is None
        let ledgers = Ledgers::new(&config, true);
        assert_eq!(ledgers.len(), 2);
    }

    #[test]
    fn test_ledgers_activate_cascade() {
        let config = config_with_cascade();
        let mut ledgers = Ledgers::new(&config, false);
        assert_eq!(ledgers.len(), 2);

        ledgers.activate_cascade(&config);
        assert_eq!(ledgers.len(), 4);
        let active = ledgers.active_ledgers();
        assert!(active.contains(&DataLedger::OneYear));
        assert!(active.contains(&DataLedger::ThirtyDay));
    }

    #[test]
    fn test_ledgers_activate_cascade_idempotent() {
        let config = config_with_cascade();
        let mut ledgers = Ledgers::new(&config, false);

        ledgers.activate_cascade(&config);
        assert_eq!(ledgers.len(), 4);

        // Second call: no-op
        ledgers.activate_cascade(&config);
        assert_eq!(ledgers.len(), 4);
    }

    #[test]
    fn test_ledgers_activate_cascade_no_config() {
        let config = ConsensusConfig::testing(); // cascade is None
        let mut ledgers = Ledgers::new(&config, false);
        assert_eq!(ledgers.len(), 2);

        ledgers.activate_cascade(&config);
        assert_eq!(ledgers.len(), 2); // no change
    }

    #[test]
    fn test_perm_expiry_disabled() {
        let config = make_test_config(None);
        let mut ledgers = Ledgers::new(&config, false);
        // Add a perm slot at height 1
        ledgers.perm.allocate_slots(1, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());
        // At height 1000, nothing should expire
        let expired = ledgers.expire_partitions(1000);
        assert!(expired.iter().all(|e| e.ledger_id != DataLedger::Publish));
    }

    #[test]
    fn test_perm_expiry_enabled() {
        let config = make_test_config(Some(2)); // 2 epochs
        let mut ledgers = Ledgers::new(&config, false);
        // num_blocks_in_epoch = 10, epoch_length = 2
        // expiry_height = epoch_height - (2 * 10) = epoch_height - 20

        // Add two perm slots
        ledgers.perm.allocate_slots(1, 1); // slot 0 at height 1
        ledgers.perm.slots[0].partitions.push(H256::random());
        ledgers.perm.allocate_slots(1, 25); // slot 1 at height 25
        ledgers.perm.slots[1].partitions.push(H256::random());

        // At epoch_height = 30: expiry_height = 30 - 20 = 10
        // Slot 0 (last_height=1) <= 10: EXPIRED
        // Slot 1 (last_height=25) > 10: NOT expired (also last slot)
        let expired = ledgers.expire_partitions(30);
        let perm_expired: Vec<_> = expired
            .iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expired.len(), 1);
        assert!(ledgers.perm.slots[0].is_expired);
        assert!(!ledgers.perm.slots[1].is_expired);
    }

    #[test]
    fn test_perm_expiry_never_expires_last_slot() {
        let config = make_test_config(Some(1)); // 1 epoch
        let mut ledgers = Ledgers::new(&config, false);
        // Add only one perm slot
        ledgers.perm.allocate_slots(1, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());

        // At epoch_height = 100: should NOT expire (it's the last slot)
        let expired = ledgers.expire_partitions(100);
        let perm_expired: Vec<_> = expired
            .iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expired.len(), 0);
        assert!(!ledgers.perm.slots[0].is_expired);
    }

    #[test]
    fn test_perm_expiry_not_enough_blocks() {
        let config = make_test_config(Some(2)); // 2 epochs * 10 blocks = 20 min
        let mut ledgers = Ledgers::new(&config, false);
        ledgers.perm.allocate_slots(2, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());
        ledgers.perm.slots[1].partitions.push(H256::random());

        // At epoch_height = 15 (< 20 minimum): nothing expires
        let expired = ledgers.expire_partitions(15);
        let perm_expired: Vec<_> = expired
            .iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expired.len(), 0);
    }

    #[test]
    fn test_get_expiring_partitions_includes_perm() {
        let config = make_test_config(Some(2));
        let mut ledgers = Ledgers::new(&config, false);
        ledgers.perm.allocate_slots(2, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());
        ledgers.perm.slots[1].partitions.push(H256::random());

        // Read-only: should report slot 0 as expiring without marking it
        let expiring = ledgers.get_expiring_partitions(30);
        let perm_expiring: Vec<_> = expiring
            .iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expiring.len(), 1);
        // Verify NOT marked as expired (read-only)
        assert!(!ledgers.perm.slots[0].is_expired);
    }

    #[test]
    fn test_perm_get_slot_needs_filters_expired() {
        let config = ConsensusConfig::testing();
        let mut perm = PermanentLedger::new(&config);

        // Add two slots (both empty, so both need partitions)
        perm.allocate_slots(2, 1);

        // Mark slot 0 as expired
        perm.slots[0].is_expired = true;

        let needs = perm.get_slot_needs();
        // Slot 0 is expired — should not appear in needs
        // Slot 1 needs partitions — should appear
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].0, 1); // slot index 1
    }

    #[test]
    fn test_get_expiring_partitions_disabled_perm() {
        let config = make_test_config(None);
        let mut ledgers = Ledgers::new(&config, false);
        ledgers.perm.allocate_slots(1, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());

        let expiring = ledgers.get_expiring_partitions(1000);
        assert!(expiring.iter().all(|e| e.ledger_id != DataLedger::Publish));
    }
}
