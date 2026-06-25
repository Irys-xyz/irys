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
    /// Block height at which this slot was allocated. Set once at allocation and
    /// never mutated, so slot expiry (`last_height <= expiry_height`) is a pure
    /// function of canonical slot state — see `get_all_expired_slot_indexes`.
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

    /// Like [`get_expired_slot_indexes`](Self::get_expired_slot_indexes) but returns
    /// **all** slots whose storage has expired as of `epoch_height`, including those
    /// already marked `is_expired` from a previous epoch. The never-expire-the-last-slot
    /// rule is preserved.
    ///
    /// `get_expired_slot_indexes` returns only *newly* expiring slots (it filters out
    /// `is_expired`), because the refund/fee pipeline must act on each slot exactly once.
    /// The NC-0042 publish-candidate filter and validator check need the opposite: a tx
    /// whose Submit slot expired must be treated as non-promotable for *every* block
    /// at-or-after the expiry, not just the single epoch block where it newly expires.
    /// Keying on `last_height` (set once at allocation, never mutated) makes this a pure
    /// function of canonical slot state — producer and validator always agree.
    pub fn get_all_expired_slot_indexes(&self, epoch_height: u64) -> Vec<usize> {
        let min_blocks = self
            .epoch_length
            .checked_mul(self.num_blocks_in_epoch)
            .expect("epoch_length * num_blocks_in_epoch overflows u64");

        if epoch_height < min_blocks {
            return Vec::new();
        }

        let expiry_height = epoch_height - min_blocks;
        let last_slot_index = self.slots.len().saturating_sub(1);

        self.slots
            .iter()
            .enumerate()
            .filter(|(slot_index, slot)| {
                // Never expire the last slot in a ledger (matches get_expired_slot_indexes).
                *slot_index != last_slot_index && slot.last_height <= expiry_height
            })
            .map(|(slot_index, _)| slot_index)
            .collect()
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

    /// Slot indexes of the given term `ledger` whose storage has expired as of
    /// `epoch_height`, inclusive of already-expired slots. Used by the NC-0042
    /// publish-candidate filter (producer) and validator check, which must treat
    /// a tx as non-promotable for every block at-or-after its slot's expiry.
    /// See [`TermLedger::get_all_expired_slot_indexes`].
    pub fn get_all_expired_term_slot_indexes(
        &self,
        ledger: DataLedger,
        epoch_height: u64,
    ) -> Vec<usize> {
        self.get_term_ledger(ledger)
            .get_all_expired_slot_indexes(epoch_height)
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

    /// Mutable counterpart of [`get_slots`]: the slot `Vec` backing `ledger`.
    fn slots_mut(&mut self, ledger: DataLedger) -> &mut Vec<LedgerSlot> {
        match ledger {
            DataLedger::Publish => &mut self.perm.slots,
            ledger => &mut self.get_term_ledger_mut(ledger).slots,
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
        self.slots_mut(ledger)[slot_index]
            .partitions
            .push(partition_hash);
    }

    pub fn remove_partition_from_slot(
        &mut self,
        ledger: DataLedger,
        slot_index: usize,
        partition_hash: &H256,
    ) {
        self.slots_mut(ledger)[slot_index]
            .partitions
            .retain(|p| p != partition_hash);
    }

    /// Refresh `last_height` on every slot that received new canonical data
    /// during this epoch, so a slot's expiry clock counts from the last time
    /// data was written into it rather than from when the slot was allocated.
    ///
    /// `prev_total_chunks` / `new_total_chunks` are the ledger's cumulative
    /// chunk counts at the previous and current epoch blocks (read from the
    /// block header's `DataTransactionLedger.total_chunks`). `chunks_per_slot`
    /// is `num_chunks_in_partition` — the canonical chunks held by one slot,
    /// matching the capacity model in `calculate_additional_slots` and the slot
    /// range math in `block_producer::ledger_expiry::compute_chunk_range`.
    ///
    /// Caller is responsible for gating this on the Cascade hardfork so that
    /// pre-activation chains replay bit-identically (slots keep their
    /// allocation-time `last_height`).
    pub fn touch_filled_slots(
        &mut self,
        ledger: DataLedger,
        prev_total_chunks: u64,
        new_total_chunks: u64,
        chunks_per_slot: u64,
        height: u64,
    ) {
        // No data added this epoch (or misconfigured slot size) -> nothing to do.
        if new_total_chunks <= prev_total_chunks || chunks_per_slot == 0 {
            return;
        }

        // The new chunks [prev_total_chunks, new_total_chunks) land in slots
        // first..=last (each slot i owns chunk range [i*C, (i+1)*C)).
        let first = prev_total_chunks / chunks_per_slot;
        let last = (new_total_chunks - 1) / chunks_per_slot;

        let slots = self.slots_mut(ledger);

        // Clamp the touched range to the slots that actually exist: `first`/`last`
        // come from cumulative chunk counts, so `last` can point past the
        // allocated slots (e.g. when allocation lags ingress). Iterate the
        // existing slice directly rather than probing non-existent indices.
        let Ok(first) = usize::try_from(first) else {
            return;
        };
        if first >= slots.len() {
            return;
        }
        let last = usize::try_from(last)
            .unwrap_or(usize::MAX)
            .min(slots.len() - 1);

        for slot in &mut slots[first..=last] {
            // Don't resurrect an already-expired slot.
            if !slot.is_expired {
                slot.last_height = height;
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

    /// Allocate `count` Submit slots, all stamped with `last_height = alloc_height`.
    fn ledgers_with_submit_slots(count: u64, alloc_height: u64) -> Ledgers {
        let config = ConsensusConfig::testing();
        let mut ledgers = Ledgers::new(&config, false);
        ledgers[DataLedger::Submit].allocate_slots(count, alloc_height);
        ledgers
    }

    fn submit_last_heights(ledgers: &Ledgers) -> Vec<u64> {
        ledgers
            .get_slots(DataLedger::Submit)
            .iter()
            .map(|s| s.last_height)
            .collect()
    }

    #[test]
    fn test_touch_filled_slots_marks_all_written_slots() {
        // 3 slots of 10 chunks each; new chunks [0, 25) span slots 0,1,2.
        let mut ledgers = ledgers_with_submit_slots(3, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 0, 25, 10, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![100, 100, 100]);
    }

    #[test]
    fn test_touch_filled_slots_partial_window_touches_only_overlap() {
        // New chunks [12, 18) fall entirely within slot 1 (covers [10, 20)).
        let mut ledgers = ledgers_with_submit_slots(3, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 12, 18, 10, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![1, 100, 1]);
    }

    #[test]
    fn test_touch_filled_slots_boundary_aligned_prev() {
        // prev exactly on a slot boundary: [10, 11) is the first chunk of slot 1,
        // so slot 0 must NOT be touched.
        let mut ledgers = ledgers_with_submit_slots(3, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 10, 11, 10, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![1, 100, 1]);
    }

    #[test]
    fn test_touch_filled_slots_noop_when_no_data() {
        // new <= prev means no chunks were added this epoch.
        let mut ledgers = ledgers_with_submit_slots(2, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 15, 15, 10, 100);
        ledgers.touch_filled_slots(DataLedger::Submit, 20, 10, 10, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![1, 1]);
    }

    #[test]
    fn test_touch_filled_slots_noop_when_zero_slot_size() {
        // chunks_per_slot == 0 must be a guarded no-op (no divide-by-zero panic).
        let mut ledgers = ledgers_with_submit_slots(2, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 0, 100, 0, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![1, 1]);
    }

    #[test]
    fn test_touch_filled_slots_skips_expired_slots() {
        // An already-expired slot must not be resurrected, even if data lands in it.
        let mut ledgers = ledgers_with_submit_slots(3, 1);
        ledgers.slots_mut(DataLedger::Submit)[1].is_expired = true;
        ledgers.touch_filled_slots(DataLedger::Submit, 0, 25, 10, 100);

        let slots = ledgers.get_slots(DataLedger::Submit);
        assert_eq!(slots[0].last_height, 100);
        assert_eq!(
            slots[1].last_height, 1,
            "expired slot keeps its last_height"
        );
        assert!(slots[1].is_expired, "expired slot stays expired");
        assert_eq!(slots[2].last_height, 100);
    }

    #[test]
    fn test_touch_filled_slots_ignores_out_of_range_indices() {
        // Window implies slots up to index 4 but only 2 slots exist: no panic,
        // existing slots still updated.
        let mut ledgers = ledgers_with_submit_slots(2, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 0, 50, 10, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![100, 100]);
    }

    #[test]
    fn test_touch_filled_slots_refreshes_publish_perm_ledger() {
        // `touch_active_ledger_slots` iterates `active_ledgers()`, which INCLUDES
        // Publish. When `publish_ledger_epoch_length` makes perm slots expirable,
        // the touch must refresh them through the `slots_mut(Publish)` (perm) path
        // exactly as for term ledgers — otherwise perm expiry would still count
        // from allocation instead of the last write.
        let config = ConsensusConfig::testing();
        let mut ledgers = Ledgers::new(&config, false);
        ledgers[DataLedger::Publish].allocate_slots(3, 1);
        // New chunks [0, 25) span perm slots 0,1,2 (10 chunks each).
        ledgers.touch_filled_slots(DataLedger::Publish, 0, 25, 10, 100);
        let heights: Vec<u64> = ledgers
            .get_slots(DataLedger::Publish)
            .iter()
            .map(|s| s.last_height)
            .collect();
        assert_eq!(heights, vec![100, 100, 100]);
    }

    /// `get_all_expired_slot_indexes` is the NC-0042 §4b/§4c non-promotability
    /// predicate. It must never expire the last/only slot — the case a per-tx
    /// cycle-math approximation got wrong, since the genesis slot stays the "last
    /// slot" and its data is still stored until a newer slot is allocated.
    #[test]
    fn get_all_expired_slot_indexes_never_expires_the_only_slot() {
        let config = ConsensusConfig::testing();
        let epoch_length = config.epoch.submit_ledger_epoch_length;
        let blocks_per_cycle = epoch_length * config.epoch.num_blocks_in_epoch;

        let mut ledger = TermLedger::new(DataLedger::Submit, &config, epoch_length);
        ledger.allocate_slots(1, 0); // single slot allocated at genesis

        // Even far past the cycle boundary, the only slot is the last slot and
        // never expires — exactly where the cycle-math approximation diverged.
        assert!(
            ledger
                .get_all_expired_slot_indexes(blocks_per_cycle)
                .is_empty()
        );
        assert!(
            ledger
                .get_all_expired_slot_indexes(blocks_per_cycle * 10)
                .is_empty()
        );
    }

    /// The inclusive set keeps a slot once it has expired (the cross-block
    /// double-pay guard), whereas `get_expired_slot_indexes` returns only the
    /// *newly* expiring slot (the once-per-slot refund trigger).
    #[test]
    fn get_all_expired_slot_indexes_includes_already_expired_slots() {
        let config = ConsensusConfig::testing();
        let epoch_length = config.epoch.submit_ledger_epoch_length;
        let num_blocks = config.epoch.num_blocks_in_epoch;
        let blocks_per_cycle = epoch_length * num_blocks;

        // slot 0 allocated at genesis, slot 1 allocated one epoch later → slot 0
        // is non-last and expires at 0 + blocks_per_cycle.
        let mut ledger = TermLedger::new(DataLedger::Submit, &config, epoch_length);
        ledger.allocate_slots(1, 0); // slot 0
        ledger.allocate_slots(1, num_blocks); // slot 1 (the kept last slot)

        let expiry = blocks_per_cycle; // slot 0 expires here

        // One epoch before: nothing expired yet.
        assert!(
            ledger
                .get_all_expired_slot_indexes(expiry - num_blocks)
                .is_empty()
        );

        // At expiry, slot 0 is in both sets.
        assert_eq!(ledger.get_all_expired_slot_indexes(expiry), vec![0]);
        assert_eq!(ledger.get_expired_slot_indexes(expiry), vec![0]);

        // After it is marked expired, the inclusive set still returns it
        // (a tx in slot 0 stays non-promotable at every later block) while the
        // newly-expiring set drops it (it must only be refunded once).
        ledger.expire_old_slots(expiry);
        assert_eq!(
            ledger.get_all_expired_slot_indexes(expiry + 1),
            vec![0],
            "an already-expired slot must remain in the inclusive set (cross-block guard)"
        );
        assert!(
            ledger.get_expired_slot_indexes(expiry + 1).is_empty(),
            "get_expired_slot_indexes returns only newly-expiring slots"
        );
    }

    /// NC-0042 P0-1: `get_all_expired_slot_indexes` CAN return a non-contiguous
    /// (holey) set when an empty pre-allocated middle slot ages out by its
    /// allocation height while a lower slot stays live. This is the exact input
    /// the `expired_submit_range` contiguity guard fails loud on — keep them in
    /// lockstep. Built with the `last_height = [1, 100, 1, 1]` shape the touch
    /// produces (touch only slot 1).
    #[test]
    fn get_all_expired_slot_indexes_can_be_non_prefix() {
        let config = ConsensusConfig::testing();
        let min_blocks = config.epoch.submit_ledger_epoch_length * config.epoch.num_blocks_in_epoch;

        // 4 slots allocated at height 1; touch only slot 1 (chunks [12,18) with a
        // 10-chunk partition) → last_height [1, 100, 1, 1].
        let mut ledgers = ledgers_with_submit_slots(4, 1);
        ledgers.touch_filled_slots(DataLedger::Submit, 12, 18, 10, 100);
        assert_eq!(submit_last_heights(&ledgers), vec![1, 100, 1, 1]);

        // expiry_height = (min_blocks + 50) - min_blocks = 50 ∈ (1, 100): slots 0
        // and 2 expire (last_height 1), slot 1 stays live (last_height 100), slot
        // 3 is the never-expiring last slot → the non-prefix set {0, 2}.
        assert_eq!(
            ledgers.get_all_expired_term_slot_indexes(DataLedger::Submit, min_blocks + 50),
            vec![0, 2],
            "an empty middle slot aged by allocation height yields a non-prefix expired set"
        );
    }
}
