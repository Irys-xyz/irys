use irys_types::{DataRoot, TxChunkOffset, UnpackedChunk};
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct ActiveDrain {
    token: u64,
    /// Chunks this drain popped and still has to put back. They count toward
    /// the per-root cap even though they are not in the map.
    reserved: usize,
    /// An external chunk parked while this drain was the active one.
    gained: bool,
}

#[derive(Debug, Default)]
struct DrainFlags {
    next_token: u64,
    active: HashMap<DataRoot, ActiveDrain>,
}

/// How a park interacts with an active drain. Decided under the drain lock so
/// a cancel cannot clear the flag between the check and the gained bit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DrainAdmission {
    /// The root is not draining. The caller applies the normal cap.
    Idle,
    /// This drain is re-ingesting a chunk it already holds.
    RejectOwn,
    /// The popped set plus chunks already parked fill the cap.
    RejectCap { occupied: usize },
    /// An external chunk fits. The gained bit is set. The caller must put it.
    Accept,
}

/// Ends one `begin_drain` when dropped, including when the drain task is cancelled.
///
/// `end` removes the flag only when this ticket is still the active drain. A later
/// drain of the same root keeps its own token. If this ticket ends a drain that
/// accepted a new chunk, it runs the follow-up once.
#[must_use = "dropping the ticket ends the drain"]
pub(crate) struct DrainTicket {
    flags: Arc<Mutex<DrainFlags>>,
    data_root: DataRoot,
    token: u64,
    armed: bool,
    on_gained: Option<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for DrainTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DrainTicket")
            .field("data_root", &self.data_root)
            .field("token", &self.token)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

impl DrainTicket {
    pub(crate) fn set_gained_follow_up(&mut self, follow_up: impl FnOnce() + Send + 'static) {
        self.on_gained = Some(Box::new(follow_up));
    }

    pub(crate) fn end(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let schedule = {
            let mut flags = lock_drain_flags(&self.flags);
            match flags.active.get(&self.data_root) {
                Some(state) if state.token == self.token => {
                    let gained = state.gained;
                    flags.active.remove(&self.data_root);
                    gained
                }
                _ => false,
            }
        };
        if schedule && let Some(follow_up) = self.on_gained.take() {
            follow_up();
        }
    }
}

impl Drop for DrainTicket {
    fn drop(&mut self) {
        self.end();
    }
}

/// A pending chunks cache that prioritizes entries with more chunks.
///
/// When at capacity, evicts the entry with the fewest chunks (not LRU).
/// This prevents attackers from evicting legitimate uploads by flooding
/// with single-chunk fake entries.
#[derive(Debug)]
pub struct PriorityPendingChunks {
    entries: HashMap<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    max_entries: usize,
    max_chunks_per_entry: usize,
    draining: Arc<Mutex<DrainFlags>>,
}

impl PriorityPendingChunks {
    pub fn new(max_entries: usize, max_chunks_per_entry: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(max_entries),
            max_entries,
            max_chunks_per_entry,
            draining: Arc::new(Mutex::new(DrainFlags::default())),
        }
    }

    fn lock_drains(&self) -> std::sync::MutexGuard<'_, DrainFlags> {
        lock_drain_flags(&self.draining)
    }

    /// Mark `data_root` draining. `reserved` is the popped set this drain still holds.
    pub(crate) fn begin_drain(&self, data_root: DataRoot, reserved: usize) -> DrainTicket {
        let mut flags = self.lock_drains();
        let token = flags.next_token;
        flags.next_token = flags.next_token.wrapping_add(1);
        flags.active.insert(
            data_root,
            ActiveDrain {
                token,
                reserved,
                gained: false,
            },
        );
        DrainTicket {
            flags: Arc::clone(&self.draining),
            data_root,
            token,
            armed: true,
            on_gained: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_draining(&self, data_root: &DataRoot) -> bool {
        self.lock_drains().active.contains_key(data_root)
    }

    /// Decide whether a chunk may park while `data_root` is draining.
    ///
    /// An external chunk that fits sets the gained bit under this lock. The
    /// caller puts the chunk and must not start another drain: the active
    /// drain schedules one follow-up. The drain's own chunks are rejected so
    /// the restore can put them back without evicting a sibling.
    pub(crate) fn drain_admission(
        &self,
        data_root: &DataRoot,
        current: usize,
        cap: usize,
        own_reingest: bool,
    ) -> DrainAdmission {
        let mut flags = self.lock_drains();
        let Some(state) = flags.active.get_mut(data_root) else {
            return DrainAdmission::Idle;
        };
        let occupied = current.saturating_add(state.reserved);
        if own_reingest {
            return DrainAdmission::RejectOwn;
        }
        if occupied >= cap {
            return DrainAdmission::RejectCap { occupied };
        }
        state.gained = true;
        DrainAdmission::Accept
    }

    /// Clear the drain flag. True when an external chunk parked during it.
    pub(crate) fn take_arrived_and_clear_drain(&self, data_root: &DataRoot) -> bool {
        self.lock_drains()
            .active
            .remove(data_root)
            .is_some_and(|state| state.gained)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert a chunk into the cache.
    ///
    /// If the data_root doesn't exist and the cache is at capacity,
    /// evicts the entry with the fewest chunks first.
    pub fn put(&mut self, chunk: UnpackedChunk) {
        let data_root = chunk.data_tree_root;
        let tx_offset = chunk.tx_offset;

        if let Some(chunks) = self.entries.get_mut(&data_root.0) {
            chunks.put(tx_offset, chunk);
            return;
        }

        if self.entries.len() >= self.max_entries {
            self.evict_lowest_priority();
        }

        let mut new_cache = LruCache::new(
            NonZeroUsize::new(self.max_chunks_per_entry).expect("max_chunks_per_entry must be > 0"),
        );
        new_cache.put(tx_offset, chunk);
        self.entries.insert(data_root.0, new_cache);
    }

    #[must_use]
    pub fn pop(&mut self, data_root: &DataRoot) -> Option<LruCache<TxChunkOffset, UnpackedChunk>> {
        self.entries.remove(data_root)
    }

    pub fn get_mut(
        &mut self,
        data_root: &DataRoot,
    ) -> Option<&mut LruCache<TxChunkOffset, UnpackedChunk>> {
        self.entries.get_mut(data_root)
    }

    /// Get read-only access to the chunks for a given data_root without modifying LRU order.
    pub fn get(&self, data_root: &DataRoot) -> Option<&LruCache<TxChunkOffset, UnpackedChunk>> {
        self.entries.get(data_root)
    }

    /// Evict the entry with the fewest chunks.
    fn evict_lowest_priority(&mut self) {
        if self.entries.is_empty() {
            return;
        }

        // Find entry with fewest chunks
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, chunks)| chunks.len())
            .map(|(data_root, _)| *data_root);

        if let Some(data_root) = victim {
            tracing::debug!(?data_root, "Evicting lowest-priority pending chunks entry");
            self.entries.remove(&data_root);
        }
    }
}

fn lock_drain_flags(flags: &Mutex<DrainFlags>) -> std::sync::MutexGuard<'_, DrainFlags> {
    flags
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::DataTreeRoot;
    use irys_types::{DataRoot, TxChunkOffset};
    use proptest::prelude::*;

    fn make_chunk(data_root: [u8; 32], offset: u32, data_size: u64) -> UnpackedChunk {
        UnpackedChunk {
            data_tree_root: DataTreeRoot(DataRoot::from(data_root)),
            data_size,
            tx_offset: TxChunkOffset::from(offset),
            data_path: Default::default(),
            bytes: Default::default(),
        }
    }

    #[test]
    fn evicts_entry_with_fewest_chunks() {
        let mut cache = PriorityPendingChunks::new(3, 10);

        let root_a = [1_u8; 32];
        cache.put(make_chunk(root_a, 0, 1000));
        cache.put(make_chunk(root_a, 1, 1000));
        cache.put(make_chunk(root_a, 2, 1000));

        let root_b = [2_u8; 32];
        cache.put(make_chunk(root_b, 0, 1000));

        let root_c = [3_u8; 32];
        cache.put(make_chunk(root_c, 0, 1000));
        cache.put(make_chunk(root_c, 1, 1000));

        assert_eq!(cache.len(), 3);

        let root_d = [4_u8; 32];
        cache.put(make_chunk(root_d, 0, 1000));

        assert_eq!(cache.len(), 3);
        assert!(cache.entries.contains_key(&DataRoot::from(root_a))); // 3 chunks - kept
        assert!(!cache.entries.contains_key(&DataRoot::from(root_b))); // 1 chunk - evicted
        assert!(cache.entries.contains_key(&DataRoot::from(root_c))); // 2 chunks - kept
        assert!(cache.entries.contains_key(&DataRoot::from(root_d))); // new entry
    }

    proptest! {
        #[test]
        fn multi_chunk_entries_survive_single_chunk_flooding(
            cache_size in 5..50_usize,
            legitimate_chunk_count in 2..10_usize,
            attacker_count in 10..100_usize,
        ) {
            let mut cache = PriorityPendingChunks::new(cache_size, 100);

            let legitimate_root = [0xAA_u8; 32];
            for i in 0..legitimate_chunk_count as u32 {
                cache.put(make_chunk(legitimate_root, i, 1000));
            }

            for i in 0..attacker_count {
                let attacker_root = [i as u8; 32];
                cache.put(make_chunk(attacker_root, 0, 1000));
            }

            // Property: multi-chunk entry survives single-chunk flooding
            prop_assert!(cache.entries.contains_key(&DataRoot::from(legitimate_root)));
            prop_assert_eq!(
                cache.entries.get(&DataRoot::from(legitimate_root)).unwrap().len(),
                legitimate_chunk_count
            );
        }
    }

    #[test]
    fn respects_max_chunks_per_entry() {
        let mut cache = PriorityPendingChunks::new(10, 3);

        let root = [1_u8; 32];
        for i in 0..5 {
            cache.put(make_chunk(root, i, 1000));
        }

        // Only 3 chunks should be kept (max_chunks_per_entry = 3)
        assert_eq!(cache.entries.get(&DataRoot::from(root)).unwrap().len(), 3);
    }

    /// Map-level illustration of the TX/chunk TOCTOU order (drain-empty then
    /// park). The product decision that *triggers* the second drain is covered
    /// by `should_schedule_pending_drain_after_park` tests in `chunks.rs`.
    #[test]
    fn early_drain_then_park_leaves_entry_until_second_pop() {
        let mut cache = PriorityPendingChunks::new(10, 10);
        let root_bytes = [0x3e_u8; 32];
        let root = DataRoot::from(root_bytes);

        assert!(cache.pop(&root).is_none());
        cache.put(make_chunk(root_bytes, 0, 12));
        assert!(cache.entries.contains_key(&root));
        let recovered = cache
            .pop(&root)
            .expect("second drain recovers parked chunk");
        assert_eq!(recovered.len(), 1);
    }

    /// Cancel drops the ticket. The root must accept parks again. A ticket that
    /// already ended must not clear a drain that started after it.
    #[test]
    fn drain_ticket_clears_only_its_own_token() {
        let cache = PriorityPendingChunks::new(4, 4);
        let root = DataRoot::from([0xD1; 32]);

        let ticket = cache.begin_drain(root, 0);
        assert!(cache.is_draining(&root));
        drop(ticket);
        assert!(
            !cache.is_draining(&root),
            "dropping the drain ticket must clear the flag"
        );

        let mut ended = cache.begin_drain(root, 0);
        ended.end();
        assert!(!cache.is_draining(&root));
        let next = cache.begin_drain(root, 0);
        drop(ended);
        assert!(
            cache.is_draining(&root),
            "an ended ticket must not clear the next drain"
        );
        drop(next);
        assert!(!cache.is_draining(&root));
    }

    /// Cancel ends the drain. A chunk that parked during it still needs one follow-up.
    #[test]
    fn cancel_schedules_follow_up_when_a_chunk_arrived_during_the_drain() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let cache = PriorityPendingChunks::new(4, 4);
        let root = DataRoot::from([0xD2; 32]);
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let mut ticket = cache.begin_drain(root, 1);
        ticket.set_gained_follow_up(move || flag.store(true, Ordering::SeqCst));
        assert_eq!(
            cache.drain_admission(&root, 0, 4, false),
            DrainAdmission::Accept
        );
        drop(ticket);
        assert!(
            fired.load(Ordering::SeqCst),
            "dropping the drain must schedule the chunk that arrived"
        );
        assert!(!cache.is_draining(&root));
    }

    /// The drain already observed the arrived chunk and cleared itself.
    /// The ticket must not schedule a second follow-up.
    #[test]
    fn ticket_drop_does_not_follow_up_after_the_drain_clears_the_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let cache = PriorityPendingChunks::new(4, 4);
        let root = DataRoot::from([0xD3; 32]);
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        let mut ticket = cache.begin_drain(root, 1);
        ticket.set_gained_follow_up(move || flag.store(true, Ordering::SeqCst));
        assert_eq!(
            cache.drain_admission(&root, 0, 4, false),
            DrainAdmission::Accept
        );
        assert!(cache.take_arrived_and_clear_drain(&root));
        drop(ticket);
        assert!(
            !fired.load(Ordering::SeqCst),
            "the drain already scheduled the follow-up"
        );
    }

    /// The popped set counts toward the cap. The drain's own chunk is not parked here.
    #[test]
    fn drain_admission_reserves_the_popped_set_and_rejects_its_own_chunk() {
        let cache = PriorityPendingChunks::new(4, 4);
        let root = DataRoot::from([0xD4; 32]);
        let _ticket = cache.begin_drain(root, 2);
        assert_eq!(
            cache.drain_admission(&root, 0, 2, false),
            DrainAdmission::RejectCap { occupied: 2 }
        );
        assert_eq!(
            cache.drain_admission(&root, 0, 4, true),
            DrainAdmission::RejectOwn
        );
        assert_eq!(
            cache.drain_admission(&root, 1, 4, false),
            DrainAdmission::Accept
        );
    }
}
